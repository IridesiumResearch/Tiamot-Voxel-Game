// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! What happens between frames: network events in, meshes and a camera out.
//!
//! # Deliberately free of `winit`
//!
//! Everything here is driven by plain calls — [`App::pump_network`],
//! [`App::remesh`], [`App::advance`] — and none of them knows what a window is.
//! `main.rs` translates window events into [`Input`] and calls them; a test
//! calls them directly. A frame loop that could only be exercised by opening a
//! window would be a frame loop nobody tested.
//!
//! # The remesh budget
//!
//! Task 02b measured a realistic chunk at 0.108 ms and a remesh after one
//! sub-node edit at 0.106 ms (`docs/subnode-verdict.md`). At four chunks a
//! frame that is under half a millisecond of a 16 ms frame — a fixed cost that
//! keeps a world filling in without ever being the reason a frame is late.
//! Meshing everything that arrived would stall on a join, when a hundred chunks
//! land at once.

use std::collections::BTreeMap;

use tiamot_core::phys::{self, Intent, Tuning};
use tiamot_core::{ChunkPos, MaterialId};

use crate::camera::{Camera, Position};
use crate::config::Config;
use crate::mesher;
use crate::net::{Command, Connection, Event};
use crate::predict::Predictor;
use crate::render::Renderer;
use crate::texture::{Atlas, Image};
use crate::world::{ABSENT_POLICY, ChunkStore};

/// The font every glyph the HUD draws comes from.
///
/// Go Mono, BSD-3-Clause, vendored with its licence and the reasoning in
/// `assets/third-party/go-font/`. Compiled in rather than loaded at runtime: a
/// client that could fail to find its font is a client that can start with an
/// invisible HUD, and 170 KiB is not worth a failure mode.
const HUD_FONT: &[u8] = include_bytes!("../assets/third-party/go-font/Go-Mono.ttf");

/// Installs [`HUD_FONT`] as the only font egui has.
///
/// **Required, not cosmetic.** The client builds egui without `default_fonts`
/// — see the workspace manifest for why — so egui starts with no glyphs at
/// all, and a HUD with no font renders nothing while reporting no error. It is
/// mapped to both families because the HUD wants one look and neither family
/// may be left empty.
pub fn install_fonts(ctx: &egui::Context) {
    let mut fonts = egui::FontDefinitions::empty();
    fonts.font_data.insert(
        "go-mono".to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(HUD_FONT)),
    );
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        fonts
            .families
            .entry(family)
            .or_default()
            .push("go-mono".to_owned());
    }
    ctx.set_fonts(fonts);
}

/// Chunks remeshed per frame. See the module docs.
pub const REMESH_BUDGET: usize = 4;

/// How long a frame may spend remeshing before it stops and finishes next
/// frame.
///
/// The ceiling [`REMESH_BUDGET`] cannot provide on its own. A chunk count is a
/// budget only if you already know what a chunk costs, and that varies by
/// almost two orders of magnitude: 0.124 ms per chunk in release on a fast
/// desktop, 2.97 ms in a debug build of the same code on the same machine, and
/// more again on charter rule 18's minimum spec of a six-core i5. Four chunks
/// is comfortably sub-millisecond in the first case and twelve milliseconds in
/// the second — a visible hitch, which is what was reported.
///
/// 2 ms leaves room for the rest of a 16 ms frame while still draining a
/// streaming queue quickly. It is a *pacing* bound, which is the metric charter
/// rule 18 names, and deliberately not an average-throughput one.
pub const REMESH_TIME_BUDGET: std::time::Duration = std::time::Duration::from_millis(2);

/// How far the debug teleport jumps, in blocks.
///
/// The number in Task 08's acceptance criteria. Far enough that a world-space
/// `f32` would have a representable step coarser than a hundredth of a
/// sub-node, which is what makes the jitter visible if floating origin is
/// broken.
pub const TELEPORT_DISTANCE: f64 = 50_000.0;

/// [`TELEPORT_DISTANCE`] in chunks.
///
/// The jump is made in whole chunks so that the camera's local offset survives
/// it unchanged; 50,000 is 3,125 chunks exactly, which is why the acceptance
/// criterion's round number needs no rounding here.
pub const TELEPORT_CHUNKS: i32 = 3_125;

/// How far ahead of the server the client keeps its input tick.
///
/// An input has to arrive before the tick it is for, so the lead has to cover
/// the trip out plus a little slack. Four ticks is 200 ms, which is generous
/// for loopback and enough for an ordinary internet connection; anything an
/// input misses is covered by the server repeating the last one for a moment
/// (`phys::input::MAX_REPEAT_TICKS`).
///
/// Comfortably inside `phys::input::MAX_LOOKAHEAD`, which is what the server
/// refuses inputs *beyond*.
const INPUT_LEAD: u64 = 4;

/// How many warnings the HUD keeps.
const WARNING_HISTORY: usize = 5;

/// Which way a debug teleport goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Teleport {
    /// Out to `+TELEPORT_DISTANCE` on both horizontal axes.
    Far,
    /// Back to the spawn chunk.
    Home,
}

/// What the player is doing this frame.
///
/// Named intents, not keys. Charter rule 11: mods register named actions and
/// the engine owns the bindings, so nothing above the window layer ever sees a
/// key code — and the free-fly camera is held to the same rule even though it
/// is the engine's own.
#[derive(Debug, Default, Clone, Copy)]
pub struct Input {
    /// Forward is positive.
    pub forward: f32,
    /// Right is positive.
    pub right: f32,
    /// Up is positive.
    pub up: f32,
    /// Mouse movement since the last frame, in pixels.
    pub look: (f32, f32),
    /// Whether to move faster.
    pub sprint: bool,
    /// Whether to move slowly and refuse to walk off edges.
    pub sneak: bool,
    /// Whether to jump.
    pub jump: bool,
    /// A one-shot debug teleport.
    pub teleport: Option<Teleport>,
}

/// Rotates a frame's keys into a world-space [`Intent`].
///
/// The rotation by yaw happens HERE, on the client, and that is a charter rule
/// 4 decision rather than a convenience: `sin` and `cos` are banned from
/// simulation because they are libm calls that differ across platforms. The
/// client is exempt — this is input handling, not the tick — so it rotates once
/// and sends the result, and both ends then simulate from identical numbers.
///
/// Free-standing rather than a method so it can be tested without a GPU. That
/// is not a stylistic preference: while it was a method on `App`, the only way
/// to reach it was to build a renderer, so nothing tested it and it shipped
/// with its strafe axis inverted.
#[expect(
    clippy::disallowed_methods,
    reason = "charter rule 4 exempts client input handling from the deterministic float subset; \
              this rotates a keypress into a direction and the RESULT is what both ends simulate \
              from, so no platform-dependent value ever reaches the tick"
)]
#[must_use]
pub fn intent_at_yaw(yaw: f32, input: Input) -> Intent {
    let (sin_yaw, cos_yaw) = yaw.sin_cos();
    // Matches `Camera::forward`: yaw turns right, and east is −x.
    let forward = [-sin_yaw, cos_yaw];
    // Both components are negated, and dropping either negation swaps A and D.
    // This is the horizontal half of `Camera::right()` — `forward × Y`, which at
    // yaw 0 is −x — and it has to be derived that way rather than guessed:
    // `[cos_yaw, sin_yaw]` points at +x, which `Camera::forward` documents as
    // WEST. Written that way, strafing right walked left. Free-fly was
    // unaffected because it calls `Camera::right()` directly, which is why all
    // of Task 08's window testing went past this without noticing.
    let right = [-cos_yaw, -sin_yaw];

    Intent {
        walk: [
            forward[0] * input.forward + right[0] * input.right,
            forward[1] * input.forward + right[1] * input.right,
        ],
        jump: input.jump,
        gait: if input.sneak {
            phys::Gait::Sneak
        } else if input.sprint {
            phys::Gait::Sprint
        } else {
            phys::Gait::Walk
        },
    }
}

/// The worst frame of the last second, and what it was doing.
///
/// **A smoothed frame rate actively hides what charter rule 18 measures.** The
/// average says 900 fps through a frame that took 11 ms, because one frame in a
/// thousand barely moves an average — and one 11 ms frame is exactly the hitch a
/// player sees. So the worst frame is kept, not the mean.
///
/// The remesh numbers sit beside it to answer the only question that matters
/// once a hitch is real: whether it *is* the remesh. A worst frame of 11 ms
/// alongside a worst remesh of 11 ms is meshing or mesh upload; a worst frame of
/// 11 ms alongside a worst remesh of 0.2 ms is something else entirely, and
/// would send anyone optimising the mesher after the wrong thing.
#[derive(Debug, Default, Clone, Copy)]
pub struct Pacing {
    /// Seconds accumulated into the window still being measured.
    elapsed: f32,
    /// Worst frame so far in the window being measured, in milliseconds.
    worst_frame: f32,
    /// Worst [`App::remesh`] so far in that window, in milliseconds.
    worst_remesh: f32,
    /// How much of that worst remesh was meshing rather than upload.
    worst_remesh_meshing: f32,
    /// Chunks rebuilt by that worst remesh.
    worst_remesh_chunks: usize,
    /// Largest prediction correction seen in the window, in cells.
    worst_correction: f32,
    /// The last completed window's worst frame, in milliseconds.
    ///
    /// Reported rather than the live figure so the readout holds still long
    /// enough to be read off a screen or a screenshot.
    reported_frame: f32,
    /// The last completed window's worst remesh, in milliseconds.
    reported_remesh: f32,
    /// How much of it was meshing rather than upload.
    reported_remesh_meshing: f32,
    /// Chunks rebuilt by that remesh.
    reported_remesh_chunks: usize,
    /// The last completed window's largest correction, in cells.
    reported_correction: f32,
}

impl Pacing {
    /// How long a window is before its worst figures are published, in seconds.
    const WINDOW: f32 = 1.0;

    /// Folds one frame's duration in, publishing the window when it is full.
    fn frame(&mut self, dt: f32) {
        self.worst_frame = self.worst_frame.max(dt * 1000.0);
        self.elapsed += dt;
        if self.elapsed >= Self::WINDOW {
            *self = Self {
                reported_frame: self.worst_frame,
                reported_remesh: self.worst_remesh,
                reported_remesh_meshing: self.worst_remesh_meshing,
                reported_remesh_chunks: self.worst_remesh_chunks,
                reported_correction: self.worst_correction,
                ..Self::default()
            };
        }
    }

    /// Folds in how far this frame's prediction was corrected, in cells.
    fn correction(&mut self, cells: f32) {
        self.worst_correction = self.worst_correction.max(cells);
    }

    /// Folds one remesh's duration in, and how much of it was meshing.
    fn remesh(&mut self, millis: f32, meshing_millis: f32, chunks: usize) {
        if millis > self.worst_remesh {
            self.worst_remesh = millis;
            self.worst_remesh_meshing = meshing_millis;
            self.worst_remesh_chunks = chunks;
        }
    }

    /// The worst frame of the last completed window, in milliseconds.
    #[must_use]
    pub const fn worst_frame_ms(&self) -> f32 {
        self.reported_frame
    }

    /// The worst remesh of the last completed window, and how many chunks it
    /// rebuilt.
    #[must_use]
    pub const fn worst_remesh_ms(&self) -> (f32, usize) {
        (self.reported_remesh, self.reported_remesh_chunks)
    }

    /// The largest prediction correction of the last completed window, in
    /// cells.
    ///
    /// **The number that says whether prediction is working.** A correction
    /// that is never zero means the client and the server disagree every tick,
    /// which is otherwise very hard to notice and very bad: the player sees a
    /// world that is subtly not where they left it, and no single frame looks
    /// wrong. `predict::SNAP_DISTANCE` is where a correction stops being
    /// blended and starts being a teleport.
    #[must_use]
    pub const fn worst_correction_cells(&self) -> f32 {
        self.reported_correction
    }

    /// How that worst remesh split between meshing and uploading, in
    /// milliseconds.
    ///
    /// The split is the diagnosis. Meshing is CPU work that shows up the same
    /// on every machine; uploading is driver work that a software rasteriser
    /// reports as free, so a devcontainer measuring "the remesh" as a single
    /// number can be an order of magnitude out and look entirely healthy.
    #[must_use]
    pub const fn worst_remesh_split_ms(&self) -> (f32, f32) {
        (
            self.reported_remesh_meshing,
            self.reported_remesh - self.reported_remesh_meshing,
        )
    }
}

/// The client, between frames.
pub struct App {
    config: Config,
    connection: Connection,
    renderer: Renderer,
    store: ChunkStore,
    camera: Camera,
    /// Material name by id, for the HUD and for diagnostics.
    materials: BTreeMap<u16, String>,
    /// Where the server said to start. `None` until the world is joined.
    spawn: Option<Position>,
    /// Whether the world has been joined.
    joined: bool,
    /// The most recent warnings, newest last.
    warnings: Vec<String>,
    /// A smoothed frame rate, for the HUD.
    fps: f32,
    /// Frame pacing over the last second, and what the remesh cost during it.
    pacing: Pacing,
    /// The server's tick when it last said so.
    tick: u64,
    /// What the connection reported about the server's certificate.
    server_label: String,
    /// The locally predicted body, once the world has been joined.
    ///
    /// `None` before the join, and until then the camera free-flies — there is
    /// no world to stand in yet, and a controller with nothing under it would
    /// simply fall.
    predictor: Option<Predictor>,
    /// The last position the server confirmed, for diagnostics and the HUD.
    confirmed: Option<(ChunkPos, [f32; 3])>,
    /// The last input tick the server said it had applied.
    confirmed_tick: u64,
    /// The server's answer about what is being broken and how far along.
    ///
    /// Presentation only — the crack overlay is drawn from it. Not predicted:
    /// the server decides when a block goes (charter rule 2), and a client that
    /// guessed would show a block breaking that then came back.
    dig: Option<(tiamot_core::SubNodePos, f32)>,
    /// Seconds carried over toward the next simulation tick.
    ///
    /// The simulation is a fixed 20 Hz (charter rule 4) and rendering is not,
    /// so frames and ticks are decoupled here: a fast machine predicts the same
    /// ticks a slow one does, just with more frames between them.
    tick_carry: f32,
    /// What the server says the player is carrying, in ascending material
    /// order and in **units** (charter rule 5).
    ///
    /// Server-authoritative and never edited here: the client is told what it
    /// has. An inventory a client could change is not an inventory.
    carried: Vec<(u16, u32)>,
    /// Every tool the server's mods registered, in ascending id order.
    ///
    /// Empty until the server says. Charter rule 1: the engine has no tools of
    /// its own, so a client with an empty list is a client connected to a world
    /// nobody can dig in — which is correct rather than broken.
    tools: Vec<tiamot_core::proto::ToolDef>,
    /// Which of them is in hand.
    held_tool: usize,
    /// Which entry of [`App::carried`] the hotbar is on.
    ///
    /// An index into a list that changes as material is gained and spent, so it
    /// is clamped on every update rather than trusted — a slot that outlived
    /// its stack would otherwise build with whatever moved into that position.
    selected: usize,

    /// Whole chunks the drawn world has been displaced by, for the
    /// floating-origin debug teleport. `[0, 0, 0]` in normal play.
    ///
    /// The store always holds chunks at their true positions — this is applied
    /// on the way to the renderer, so nothing above the draw call has to know
    /// the world has been moved.
    displacement: [i32; 3],
}

impl App {
    /// Builds an app around an already-open connection and renderer.
    #[must_use]
    pub fn new(config: Config, connection: Connection, renderer: Renderer) -> Self {
        let camera = Camera {
            fov_y: config.fov_degrees.to_radians(),
            ..Camera::default()
        };
        Self {
            config,
            connection,
            renderer,
            store: ChunkStore::new(),
            camera,
            materials: BTreeMap::new(),
            spawn: None,
            joined: false,
            warnings: Vec::new(),
            fps: 0.0,
            pacing: Pacing::default(),
            tick: 0,
            server_label: "connecting…".to_owned(),
            predictor: None,
            confirmed: None,
            confirmed_tick: 0,
            dig: None,
            tick_carry: 0.0,
            carried: Vec::new(),
            selected: 0,
            tools: Vec::new(),
            held_tool: 0,
            displacement: [0, 0, 0],
        }
    }

    /// Where a chunk is drawn, which is where it is unless the debug teleport
    /// has displaced the world.
    const fn drawn_at(&self, pos: ChunkPos) -> ChunkPos {
        ChunkPos::new(
            pos.x + self.displacement[0],
            pos.y + self.displacement[1],
            pos.z + self.displacement[2],
        )
    }

    /// The camera's chunk in the store's coordinates, undoing any displacement.
    const fn camera_chunk(&self) -> ChunkPos {
        let chunk = self.camera.position.chunk;
        ChunkPos::new(
            chunk.x - self.displacement[0],
            chunk.y - self.displacement[1],
            chunk.z - self.displacement[2],
        )
    }

    /// The renderer, for drawing a frame.
    pub fn renderer(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// How many chunks have a mesh on the GPU.
    ///
    /// A shared borrow, unlike [`App::renderer`], so a frame-loop condition can
    /// ask "has enough of the world arrived yet" without taking the renderer
    /// mutably to find out.
    #[must_use]
    pub fn meshed_chunks(&self) -> usize {
        self.renderer.chunk_count()
    }

    /// The chunks the client is holding.
    ///
    /// Exposed so a test can check what the client believes the world is,
    /// independently of what it drew — the two disagreeing is a whole class of
    /// bug that is otherwise only visible as "the screen is blank".
    #[must_use]
    pub const fn store(&self) -> &ChunkStore {
        &self.store
    }

    /// How many chunks are held but not yet meshed.
    #[must_use]
    pub fn pending_chunks(&self) -> usize {
        self.store.dirty_len()
    }

    /// The adapter the renderer is drawing with.
    #[must_use]
    pub fn adapter(&self) -> &str {
        &self.renderer.gpu().adapter
    }

    /// Keeps the predicted tick ahead of the tick the server has reached.
    ///
    /// **Without this the client silently loses control of its player.** The
    /// server refuses any input whose tick it has already passed, so a client
    /// whose count falls behind has every input refused from then on — and the
    /// server, seeing a gap, repeats the last intent it *did* accept. If that
    /// was "standing still", the player is pinned at spawn while the client
    /// predicts movement and is corrected twenty times a second. That is what
    /// "stuck in place with a lot of jitter" was.
    ///
    /// Falling behind is not an edge case. The client's count advances once per
    /// simulated tick and its frames are neither exactly 20 Hz nor free of
    /// stalls, so it drifts *by construction* — measured at 37 ticks behind
    /// after a few seconds of walking. Rather than try to make a local counter
    /// track a remote clock, this takes the server's own number and stays a
    /// fixed margin in front of it.
    fn resynchronise_tick(&mut self, server_tick: u64) {
        let want = server_tick + INPUT_LEAD;
        if self.tick < want {
            self.tick = want;
        }
    }

    /// The tick the client is predicting, and the last one the server applied.
    ///
    /// The pair is the diagnostic: the client must stay AHEAD, because the
    /// server refuses any input whose tick it has already passed.
    #[must_use]
    pub const fn tick_pair(&self) -> (u64, u64) {
        (self.tick, self.confirmed_tick)
    }

    /// How far the SERVER has the player from where they spawned, in blocks.
    ///
    /// Exposed for tests that need to tell "the client predicted movement"
    /// apart from "the player moved", which are the same picture on screen
    /// right up until the correction arrives.
    #[must_use]
    pub fn server_travelled(&self) -> f64 {
        let Some((origin, local)) = self.confirmed else {
            return 0.0;
        };
        let Some(spawn) = self.spawn else {
            return 0.0;
        };
        let cells = f64::from(tiamot_core::SUBNODES_PER_AXIS);
        let corner = tiamot_core::BlockPos::from_chunk_corner(origin);
        let (sx, _, sz) = spawn.to_world();
        let x = f64::from(corner.x) + f64::from(local[0]) / cells;
        let z = f64::from(corner.z) + f64::from(local[2]) / cells;
        // Not `hypot`: it is a libm call and the determinism lint bans it
        // workspace-wide (float-determinism.md §1). This is only a diagnostic
        // distance, and `sqrt` is in the allowed subset anyway.
        let dx = x - sx;
        let dz = z - sz;
        (dx * dx + dz * dz).sqrt()
    }

    /// The camera.
    #[must_use]
    pub const fn camera(&self) -> &Camera {
        &self.camera
    }

    /// Whether the world has been entered.
    #[must_use]
    pub const fn joined(&self) -> bool {
        self.joined
    }

    /// Frame pacing over the last completed second.
    #[must_use]
    pub const fn pacing(&self) -> &Pacing {
        &self.pacing
    }

    /// Whether there is a predicted body driving the camera.
    ///
    /// Exposed so a test can say that it is exercising the controller rather
    /// than free-fly. The two take different paths through [`App::advance`],
    /// and a test that silently ran the free-fly one would assert nothing about
    /// the controller while looking exactly as though it had.
    #[must_use]
    pub const fn predicting(&self) -> bool {
        self.predictor.is_some()
    }

    /// Points the camera down by `radians` from the horizon.
    ///
    /// For tests. The controller spawns standing on the ground, so aiming
    /// downward is the one direction guaranteed to find something within reach
    /// whatever the terrain looks like — but **straight** down finds the cell
    /// the player is standing on, and digging that drops them into the hole
    /// they then cannot build in. A slant lands a few cells away instead.
    pub fn look_down_by(&mut self, radians: f32) {
        self.camera.pitch = -radians;
    }

    /// What the crosshair is pointing at, if anything is in reach.
    ///
    /// The cell and the face it was entered through, in world cells. Placement
    /// goes in `cell + normal` — the normal points back out of the surface for
    /// exactly that reason.
    ///
    /// `None` when nothing is within [`phys::REACH`], which includes looking at
    /// the sky and looking into terrain that has not arrived yet.
    #[must_use]
    pub fn looking_at(&self) -> Option<phys::Hit> {
        let predictor = self.predictor.as_ref()?;
        let voxels = phys::Voxels::new(&self.store, predictor.origin());
        let eye = predictor.body().eye();
        let forward = self.camera.forward();
        phys::ray::cast(&voxels, eye, [forward.x, forward.y, forward.z], phys::REACH)
    }

    /// The same target, in the world's own coordinates.
    ///
    /// `looking_at` reports cells relative to the predicted body's chunk origin
    /// (charter rule 7 again), and everything on the wire is absolute. Getting
    /// this conversion wrong digs a hole somewhere else entirely.
    fn target_of(&self, cell: [i32; 3]) -> Option<tiamot_core::SubNodePos> {
        let predictor = self.predictor.as_ref()?;
        let origin = predictor.origin();
        let span = tiamot_core::CHUNK_SUBNODES as i32;
        Some(tiamot_core::SubNodePos::new(
            origin.x * span + cell[0],
            origin.y * span + cell[1],
            origin.z * span + cell[2],
        ))
    }

    /// The cell under the crosshair, for digging.
    #[must_use]
    pub fn dig_target(&self) -> Option<tiamot_core::SubNodePos> {
        self.target_of(self.looking_at()?.cell)
    }

    /// The cell a placement would fill: one step out of the surface.
    #[must_use]
    pub fn place_target(&self) -> Option<tiamot_core::SubNodePos> {
        let hit = self.looking_at()?;
        self.target_of([
            hit.cell[0] + hit.normal[0],
            hit.cell[1] + hit.normal[1],
            hit.cell[2] + hit.normal[2],
        ])
    }

    /// Starts or re-aims a dig at whatever the crosshair is on.
    ///
    /// Re-sent every frame the button is held, which is what `StartDig`'s
    /// protocol docs ask for: re-aiming at the same cell keeps its progress, so
    /// repeating is free and it means a dig follows the crosshair.
    pub fn dig(&mut self) {
        let Some(target) = self.dig_target() else {
            return;
        };
        self.connection.send(Command::Dig {
            target: Some(target),
        });
    }

    /// Stops digging, discarding progress.
    pub fn stop_digging(&mut self) {
        self.dig = None;
        self.connection.send(Command::Dig { target: None });
    }

    /// Places the selected material against the face under the crosshair.
    ///
    /// Nothing happens with an empty inventory or nothing in reach. Anything
    /// else the server may still refuse — it owns that decision (charter rule
    /// 2) — and says why, which arrives as a warning.
    pub fn place(&mut self) {
        let Some(material) = self.selected_material() else {
            self.warn("nothing selected to build with".to_owned());
            return;
        };
        let Some(target) = self.place_target() else {
            return;
        };
        self.connection.send(Command::Place { target, material });
    }

    /// The cells the next dig would remove, as world positions.
    ///
    /// **Honours the tool's brush and the block's occupancy**, which is what
    /// makes it worth drawing at all. A `"subnode"` brush outlines the single
    /// cell under the crosshair; a `"block"` brush outlines every *occupied*
    /// cell of the block containing it — so a partially-chiselled block is
    /// outlined in its real shape rather than as the cube it used to be.
    ///
    /// Empty when nothing is in reach, which includes the sky.
    #[must_use]
    pub fn selection(&self) -> Vec<tiamot_core::SubNodePos> {
        let Some(hit) = self.looking_at() else {
            return Vec::new();
        };
        let Some(cell) = self.target_of(hit.cell) else {
            return Vec::new();
        };

        let whole_block = self
            .held_tool()
            .is_none_or(|tool| tool.brush != tiamot_core::dig::Brush::SubNode.name());
        if !whole_block {
            return vec![cell];
        }

        // Every occupied cell of the block. Reading the chunk rather than
        // assuming 27: the whole point of sub-nodes is that a block need not be
        // a cube, and an outline that drew one anyway would be a lie exactly
        // where the player is looking.
        let block = cell.block();
        let base = tiamot_core::SubNodePos::new(block.x * 3, block.y * 3, block.z * 3);
        let Some(chunk) = self.store.get(block.chunk()) else {
            return vec![cell];
        };
        let occupied: Vec<tiamot_core::SubNodePos> = (0..3)
            .flat_map(|y| (0..3).flat_map(move |z| (0..3).map(move |x| (x, y, z))))
            .map(|(x, y, z)| tiamot_core::SubNodePos::new(base.x + x, base.y + y, base.z + z))
            .filter(|at| {
                chunk
                    .get_subnode(*at)
                    .is_some_and(|material| !material.is_air())
            })
            .collect();

        // A block that reads as entirely air is one the ray hit and the store
        // disagrees about — a chunk edit in flight. Outlining the cell that was
        // actually hit is better than outlining nothing.
        if occupied.is_empty() {
            vec![cell]
        } else {
            occupied
        }
    }

    /// Hands the current selection to the renderer, camera-relative.
    ///
    /// The same floating-origin treatment chunk instances get: the offset is
    /// computed in `f64` and narrowed once it is small (charter rule 7), so an
    /// outline at the edge of the world is drawn from the same small numbers as
    /// one at the origin.
    fn update_selection(&mut self) {
        let cells = self.selection();
        let mut corners: Vec<[f32; 3]> = Vec::with_capacity(cells.len());
        let (camera_x, camera_y, camera_z) = self.camera.position.to_world();
        let per_block = f64::from(tiamot_core::SUBNODES_PER_AXIS);
        let shift = [
            f64::from(self.displacement[0]) * f64::from(tiamot_core::CHUNK_BLOCKS),
            f64::from(self.displacement[1]) * f64::from(tiamot_core::CHUNK_BLOCKS),
            f64::from(self.displacement[2]) * f64::from(tiamot_core::CHUNK_BLOCKS),
        ];
        for cell in cells {
            // Cells to blocks, displaced the way the drawn world is, then made
            // relative to the camera BEFORE narrowing to f32.
            let x = (f64::from(cell.x) / per_block + shift[0] - camera_x) * per_block;
            let y = (f64::from(cell.y) / per_block + shift[1] - camera_y) * per_block;
            let z = (f64::from(cell.z) / per_block + shift[2] - camera_z) * per_block;
            corners.push([x as f32, y as f32, z as f32]);
        }
        self.renderer.set_selection(&corners);
    }

    /// Cycles to the next registered tool and tells the server.
    ///
    /// Nothing happens with no tools, which is a world nobody can dig in —
    /// correct rather than broken, and the state a server with no mods is in.
    pub fn next_tool(&mut self) {
        if self.tools.is_empty() {
            return;
        }
        self.held_tool = (self.held_tool + 1) % self.tools.len();
        self.send_held_tool();
    }

    /// Selects the first tool with a sub-node brush, if the mods registered one.
    ///
    /// For tests, which need to pick a brush rather than a name — the engine
    /// has no opinion about what a chisel is called (charter rule 1), so a test
    /// that named one would be asserting about `game/` rather than the engine.
    pub fn select_subnode_tool(&mut self) {
        self.select_brush(tiamot_core::dig::Brush::SubNode.name());
    }

    /// Selects the first tool with a whole-block brush.
    pub fn select_block_tool(&mut self) {
        self.select_brush(tiamot_core::dig::Brush::Block.name());
    }

    fn select_brush(&mut self, brush: &str) {
        if let Some(index) = self.tools.iter().position(|tool| tool.brush == brush) {
            self.held_tool = index;
            self.send_held_tool();
        }
    }

    /// Tells the server which tool is in hand.
    ///
    /// Sent by id rather than by index: the client's list order is its own, and
    /// an index would mean the two ends had to agree about it forever.
    fn send_held_tool(&mut self) {
        let tool = self.tools.get(self.held_tool).map(|tool| tool.id.clone());
        self.connection.send(Command::SelectTool { tool });
    }

    /// The tool in hand, if the server has sent its table.
    #[must_use]
    pub fn held_tool(&self) -> Option<&tiamot_core::proto::ToolDef> {
        self.tools.get(self.held_tool)
    }

    /// The material the hotbar is on, if the player is carrying anything.
    #[must_use]
    pub fn selected_material(&self) -> Option<u16> {
        self.carried.get(self.selected).map(|(id, _)| *id)
    }

    /// Moves the hotbar selection, wrapping.
    ///
    /// Wrapping rather than clamping because the input is a mouse wheel, and a
    /// wheel that stops at the end feels broken.
    pub fn select_next(&mut self, forward: bool) {
        if self.carried.is_empty() {
            return;
        }
        let count = self.carried.len();
        self.selected = if forward {
            (self.selected + 1) % count
        } else {
            (self.selected + count - 1) % count
        };
    }

    /// Selects a slot directly, as the number keys do.
    pub fn select_slot(&mut self, slot: usize) {
        if slot < self.carried.len() {
            self.selected = slot;
        }
    }

    /// What the player is carrying, as `(material, units)` in id order.
    #[must_use]
    pub fn carried(&self) -> &[(u16, u32)] {
        &self.carried
    }

    /// Which slot is selected.
    #[must_use]
    pub const fn selected_slot(&self) -> usize {
        self.selected
    }

    /// Starts simulating a bad network on everything sent from here on.
    ///
    /// **For tests.** See [`crate::net::Command::Impair`] for why it is applied
    /// after the join rather than at connect time.
    ///
    /// Returns whether the connection is still up.
    pub fn impair(&self, impairment: tiamot_server::transport::Impairment) -> bool {
        self.connection.send(Command::Impair(impairment))
    }

    /// Closes the connection.
    pub fn shutdown(self) {
        self.connection.shutdown();
    }

    /// Drains everything the network has produced since the last frame.
    ///
    /// Returns `false` once the connection has ended, which is the signal to
    /// close the window.
    pub fn pump_network(&mut self) -> bool {
        while let Some(event) = self.connection.poll() {
            match event {
                Event::Connected {
                    address,
                    fingerprint,
                    first_use,
                } => {
                    self.server_label = format!("{address} {}", &fingerprint[..12]);
                    if first_use {
                        // Trust on first use: the one connection that cannot be
                        // verified is this one, and the player should be told
                        // rather than left to assume it was checked.
                        self.warn(format!(
                            "first connection to {address}: pinned certificate {}…",
                            &fingerprint[..16]
                        ));
                    }
                }

                Event::Materials { table, images } => {
                    self.materials = table
                        .iter()
                        .map(|entry| (entry.id, entry.name.clone()))
                        .collect();
                    self.renderer.set_atlas(&build_atlas(&table, &images));
                    // Every mesh drawn before this sampled the placeholder
                    // atlas. In practice the table arrives before any chunk,
                    // but "in practice" is not a guarantee the renderer should
                    // rely on.
                    self.store.mark_all_dirty();
                }

                Event::Joined { spawn, tick, .. } => {
                    let position = Position::from_world(
                        f64::from(spawn.x) + 0.5,
                        f64::from(spawn.y) + 2.0,
                        f64::from(spawn.z) + 0.5,
                    );
                    self.camera.position = position;
                    self.spawn = Some(position);
                    self.tick = tick;
                    self.joined = true;

                    // The body starts where the server said, in the same
                    // (chunk, local cells) pair the server simulates in — no
                    // conversion, so nothing to disagree about.
                    let origin = spawn.chunk();
                    let corner = tiamot_core::BlockPos::from_chunk_corner(origin);
                    let cells = tiamot_core::SUBNODES_PER_AXIS as f32;
                    self.predictor = Some(Predictor::new(
                        origin,
                        [
                            (spawn.x - corner.x) as f32 * cells + cells / 2.0,
                            (spawn.y - corner.y) as f32 * cells,
                            (spawn.z - corner.z) as f32 * cells + cells / 2.0,
                        ],
                        tick,
                    ));
                }

                Event::Chunk(chunk) => self.store.insert(*chunk),

                Event::ChunkUnload(pos) => {
                    if self.store.remove(pos) {
                        // The mesh has to go with the data. A renderer holding
                        // a mesh for a chunk the store has forgotten draws a
                        // ghost that nothing will ever update.
                        self.renderer.remove_chunk(&self.drawn_at(pos));
                    }
                }

                Event::Edit(edit) => {
                    self.store.apply(&edit);
                }

                Event::PlayerState(state) => {
                    self.confirmed = Some((state.chunk, state.local));
                    self.confirmed_tick = state.last_processed_input;
                    self.resynchronise_tick(state.last_processed_input);
                    // Not while the debug teleport is displacing the world: the
                    // server does not know about it, so every state would drag
                    // the camera back and the floating-origin check could not
                    // be looked at.
                    if self.displacement == [0, 0, 0]
                        && let Some(predictor) = self.predictor.as_mut()
                    {
                        let voxels = phys::Voxels::new(&self.store, predictor.origin());
                        predictor.reconcile(&voxels, &state, &Tuning::DEFAULT);
                    }
                }

                Event::DigProgress { target, progress } => {
                    self.dig = Some((target, progress));
                }

                Event::Tools { tools } => {
                    // The default first, so a player who never touches the tool
                    // key is holding whatever the mods call a bare hand.
                    self.held_tool = tools.iter().position(|tool| tool.default).unwrap_or(0);
                    self.tools = tools;
                    // **Recorded, not announced.** The table arrives while the
                    // session is still `Authenticated`, and `SelectTool` is only
                    // valid in world — replying here got the client disconnected
                    // with "SelectTool is not valid in phase Authenticated".
                    //
                    // There is nothing to announce anyway: a player who has
                    // selected nothing digs with the server's own default, which
                    // is the same tool this just picked. The first `next_tool`
                    // is the first time the two could disagree, and that is when
                    // it is sent.
                }

                Event::Inventory { stacks } => {
                    self.carried = stacks;
                    // Clamped rather than trusted. The list shrinks when a
                    // stack is spent, and a selection left pointing past the
                    // end would build with whatever slid into that position —
                    // which is the sort of bug a player reports as "it placed
                    // the wrong thing" and nobody can reproduce.
                    self.selected = self.selected.min(self.carried.len().saturating_sub(1));
                }

                Event::Chat { text, .. } => tracing::info!("{text}"),

                Event::Warning(text) => self.warn(text),

                Event::Disconnected { reason } => {
                    self.warn(format!("disconnected: {reason}"));
                    return false;
                }
            }
        }
        true
    }

    /// Remeshes chunks nearest the camera, within [`REMESH_BUDGET`] and
    /// [`REMESH_TIME_BUDGET`].
    ///
    /// Returns how many were rebuilt.
    pub fn remesh(&mut self) -> usize {
        // The store's coordinates, not the camera's, or a displaced camera
        // makes "nearest first" order the queue from 50,000 blocks away.
        let centre = self.camera_chunk();
        let due = self.store.take_dirty(centre, REMESH_BUDGET);
        if due.is_empty() {
            return 0;
        }

        // Meshing and upload are timed SEPARATELY, because they are different
        // problems with different fixes and one number could not tell them
        // apart. That ambiguity cost a whole round trip: a 15.6 ms remesh was
        // read as mesh upload on the strength of how little geometry was
        // resident, and the split then showed it was 15.1 ms of meshing and
        // 0.1 ms of upload. Mesh cost scales with the CELLS SCANNED — all
        // 110,592 of them, every time — and not at all with how small the
        // resulting mesh is, which is exactly the inference that went wrong.
        let started = std::time::Instant::now();
        let mut meshing = std::time::Duration::ZERO;
        let mut rebuilt = 0;

        for (index, pos) in due.iter().enumerate() {
            let Some(chunk) = self.store.get(*pos) else {
                continue;
            };
            let neighbours = self.store.neighbours(*pos);

            let mesh_started = std::time::Instant::now();
            let mesh = mesher::mesh_chunk(chunk, &neighbours, ABSENT_POLICY);
            meshing += mesh_started.elapsed();

            self.renderer.set_chunk(self.drawn_at(*pos), &mesh);
            rebuilt += 1;

            // A count is not a budget on a machine you have not measured.
            // Four chunks is half a millisecond in release on a fast desktop
            // and twelve in a debug build; charter rule 18's minimum spec is a
            // six-core i5, so the fixed count that is comfortable here is not
            // comfortable everywhere. Time is the thing actually being
            // protected, so time is what this spends. At least one chunk
            // always goes through — a budget that can rebuild nothing would let
            // a slow frame stop the world filling in for ever.
            if started.elapsed() >= REMESH_TIME_BUDGET && index + 1 < due.len() {
                self.store.requeue(&due[index + 1..]);
                break;
            }
        }

        self.pacing.remesh(
            started.elapsed().as_secs_f32() * 1000.0,
            meshing.as_secs_f32() * 1000.0,
            rebuilt,
        );
        rebuilt
    }

    /// Moves the camera and records the frame time.
    pub fn advance(&mut self, input: Input, dt: f32) {
        // Smoothed rather than instantaneous. A per-frame number is unreadable,
        // and charter rule 18 cares about pacing — which a jittering readout
        // actively hides.
        if dt > 0.0 {
            let instant = 1.0 / dt;
            self.fps = if self.fps == 0.0 {
                instant
            } else {
                self.fps * 0.9 + instant * 0.1
            };
        }
        // The unsmoothed half. See [`Pacing`]: the average above cannot show a
        // hitch, and the hitch is the thing charter rule 18 is about.
        self.pacing.frame(dt);

        let sensitivity = self.config.mouse_sensitivity;
        self.camera
            .look(input.look.0 * sensitivity, -input.look.1 * sensitivity);

        if self.predictor.is_some() {
            self.walk(input, dt);
        } else {
            // Not in the world yet, so there is nothing to stand on and no
            // server to reconcile with. Free-fly until the join lands.
            let speed = self.config.fly_speed * if input.sprint { 4.0 } else { 1.0 } * dt;
            if input.forward != 0.0 || input.right != 0.0 || input.up != 0.0 {
                self.camera
                    .fly(input.forward * speed, input.right * speed, input.up * speed);
            }
        }

        if let Some(teleport) = input.teleport {
            self.teleport(teleport);
        }
    }

    /// Runs whole simulation ticks and points the camera at the result.
    ///
    /// Frames and ticks are deliberately decoupled. The simulation is a fixed
    /// 20 Hz because charter rule 4 requires a fixed timestep, and rendering is
    /// whatever the machine manages — so this accumulates real time and spends
    /// it in whole ticks. A 200 fps machine predicts exactly the ticks a 40 fps
    /// machine does, which is what stops the frame rate changing how fast a
    /// player walks.
    fn walk(&mut self, input: Input, dt: f32) {
        const TICK_SECONDS: f32 = 1.0 / 20.0;
        /// Ticks simulated in one frame before the rest is abandoned.
        ///
        /// After a stall — an alt-tab, a long chunk upload — the carry can hold
        /// seconds of unspent time. Spending it would fast-forward the player
        /// through the world in one frame; the server never saw those inputs
        /// and would drag them straight back.
        const MAX_CATCH_UP: u32 = 4;

        self.tick_carry += dt;
        let mut spent = 0;
        while self.tick_carry >= TICK_SECONDS && spent < MAX_CATCH_UP {
            self.tick_carry -= TICK_SECONDS;
            spent += 1;
            self.tick += 1;

            let intent = self.intent_from(input);
            if let Some(predictor) = self.predictor.as_mut() {
                let voxels = phys::Voxels::new(&self.store, predictor.origin());
                predictor.predict(&voxels, self.tick, intent, &Tuning::DEFAULT);
            }
            self.report_input(input, intent);
        }
        if spent == MAX_CATCH_UP {
            self.tick_carry = 0.0;
        }

        // Presentation only: blends away whatever the last correction was.
        if let Some(predictor) = self.predictor.as_mut() {
            predictor.smooth(dt / TICK_SECONDS);
        }
        // Recorded before the blend finishes, so what is measured is how far
        // the server disagreed rather than how much of the disagreement is
        // left. A correction that is never zero is prediction failing, and it
        // is otherwise almost impossible to notice: no single frame looks
        // wrong, the world is just subtly not where it was left.
        if let Some(predictor) = self.predictor.as_ref() {
            self.pacing.correction(predictor.error());
        }

        // Whatever time did not buy a whole tick is how far through the current
        // one this frame is, and the camera is drawn there rather than at the
        // tick boundary. Without it the camera moves 20 times a second no
        // matter how fast the client draws.
        self.follow_body(self.tick_carry / TICK_SECONDS);
        // After the camera moves, so the outline is where the crosshair points
        // this frame rather than last.
        self.update_selection();
    }

    /// Turns this frame's keys into a world-space intent.
    fn intent_from(&self, input: Input) -> Intent {
        intent_at_yaw(self.camera.yaw, input)
    }

    /// Puts the camera at the predicted body's eyes.
    fn follow_body(&mut self, alpha: f32) {
        let Some(predictor) = self.predictor.as_ref() else {
            return;
        };
        let local = predictor.render_local_at(alpha);
        let cells = f64::from(tiamot_core::SUBNODES_PER_AXIS);
        // Displaced by the debug teleport, or this drags the camera straight
        // back to the body's real position while the world stays 50,000 blocks
        // out — an empty sky, one frame after the jump. `drawn_at` displaces
        // the chunks, so the camera has to be displaced by the same amount or
        // the two are drawn in different coordinate systems. Free-fly never hit
        // this because nothing was writing the camera position every frame.
        let corner = tiamot_core::BlockPos::from_chunk_corner(self.drawn_at(predictor.origin()));

        // Cells to blocks, and the eye offset on top. Presentation arithmetic:
        // the division by three is exact enough for a camera and never feeds
        // back into the body.
        self.camera.position = Position::from_world(
            f64::from(corner.x) + f64::from(local[0]) / cells,
            f64::from(corner.y) + f64::from(local[1] + phys::EYE_HEIGHT) / cells,
            f64::from(corner.z) + f64::from(local[2]) / cells,
        );
    }

    /// Jumps to the edge of the world, for the floating-origin check.
    ///
    /// The world moves **with** the camera, by the same whole number of chunks,
    /// so the view is unchanged and only the coordinates it is computed from
    /// grow. That is the whole claim of a floating origin, and it is the only
    /// arrangement that can be looked at: leaving the world behind while the
    /// camera jumps 50,000 blocks puts every chunk far outside the 1,000-block
    /// far plane, and an empty sky demonstrates nothing. The chunk store is
    /// untouched either way — displacement is applied on the way to the
    /// renderer.
    ///
    /// Absolute rather than cumulative, so pressing the key twice is the same
    /// as pressing it once.
    pub fn teleport(&mut self, teleport: Teleport) {
        let target = match teleport {
            Teleport::Far => [TELEPORT_CHUNKS, 0, TELEPORT_CHUNKS],
            Teleport::Home => [0, 0, 0],
        };
        let delta = [
            target[0] - self.displacement[0],
            target[1] - self.displacement[1],
            target[2] - self.displacement[2],
        ];

        // Whole chunks, so the camera's local offset is bit-identical either
        // side of the jump. Anything else would move the view a fraction of a
        // block and the shimmer being hunted would have a mundane cause.
        let chunk = self.camera.position.chunk;
        self.camera.position.chunk =
            ChunkPos::new(chunk.x + delta[0], chunk.y + delta[1], chunk.z + delta[2]);
        self.renderer.rebase(delta);
        self.displacement = target;

        self.warn(match teleport {
            Teleport::Far => format!(
                "teleported to chunk {}, {}, {} — the world came too, so this frame should be \
                 identical to the one at spawn. Any shimmer is a world coordinate that \
                 survived {TELEPORT_DISTANCE} blocks out",
                self.camera.position.chunk.x,
                self.camera.position.chunk.y,
                self.camera.position.chunk.z
            ),
            Teleport::Home => "back at the origin".to_owned(),
        });
    }

    /// Sends one tick's input to the server.
    ///
    /// The movement sent is the **world-space** vector the client just
    /// simulated with, not the raw keys — see [`App::intent_from`]. Sending the
    /// keys and letting the server rotate them would put a `sin` and a `cos`
    /// inside the tick, which charter rule 4 forbids, and would give the two
    /// ends two chances to disagree about the same movement.
    fn report_input(&self, input: Input, intent: Intent) {
        use tiamot_core::proto::actions;

        let turn = std::f32::consts::TAU;
        let mut held = 0;
        if intent.jump {
            held |= actions::JUMP;
        }
        match intent.gait {
            phys::Gait::Sprint => held |= actions::SPRINT,
            phys::Gait::Sneak => held |= actions::SNEAK,
            phys::Gait::Walk => {}
        }
        let _ = input;

        self.connection.send(Command::Input {
            tick: self.tick,
            movement: [intent.walk[0], 0.0, intent.walk[1]],
            look: [self.camera.yaw / turn, self.camera.pitch / turn],
            actions: held,
        });
    }

    /// The HUD lines, top to bottom.
    #[must_use]
    pub fn hud(&self) -> Vec<String> {
        let (x, y, z) = self.camera.position.to_world();
        let facing = compass(self.camera.yaw);
        let material_count = self.materials.len();

        let (remesh_ms, remesh_chunks) = self.pacing.worst_remesh_ms();
        let (meshing, upload) = self.pacing.worst_remesh_split_ms();
        let worst = self.pacing.worst_frame_ms();
        let (created, reused) = self.renderer.buffer_stats();
        let correction = self.pacing.worst_correction_cells();

        vec![
            format!("{:.0} fps", self.fps),
            // The average above is the reassuring number; this is the honest
            // one. Charter rule 18 measures pacing, and a 900 fps average with
            // an 11 ms worst frame is a hitch the average cannot express.
            format!(
                "worst frame {worst:.1} ms ({:.0} fps) · worst remesh {remesh_ms:.1} ms over \
                 {remesh_chunks} chunks ({meshing:.1} mesh + {upload:.1} upload)",
                if worst > 0.0 { 1000.0 / worst } else { 0.0 }
            ),
            format!(
                "{created} mesh buffers created, {reused} reused from the pool · worst \
                 correction {correction:.2} cells",
            ),
            format!("{x:.1}, {y:.1}, {z:.1}  ({facing})"),
            format!(
                "chunk {}, {}, {}",
                self.camera.position.chunk.x,
                self.camera.position.chunk.y,
                self.camera.position.chunk.z
            ),
            format!(
                "{} chunks held, {} meshed, {} drawn, {} queued",
                self.store.len(),
                self.renderer.chunk_count(),
                self.renderer.drawn(),
                self.store.dirty_len()
            ),
            format!(
                "{:.1} MiB of meshes, {material_count} materials",
                self.renderer.mesh_bytes() as f64 / (1024.0 * 1024.0)
            ),
            format!(
                "{} on {} / {}",
                self.server_label,
                self.renderer.gpu().adapter,
                self.renderer.gpu().backend
            ),
            // The hotbar, such as it is. Names rather than ids: a player
            // debugging a placement needs to know it is stone, not that it is 2.
            if self.carried.is_empty() {
                "carrying nothing — dig something".to_owned()
            } else {
                self.carried
                    .iter()
                    .enumerate()
                    .map(|(slot, (id, units))| {
                        let name = self
                            .materials
                            .get(id)
                            .map_or_else(|| format!("#{id}"), Clone::clone);
                        // Charter rule 5's display: blocks and spare nodes, not
                        // a raw unit count. 27 units is one block.
                        let (blocks, spares) = tiamot_core::inventory::display(*units);
                        let marker = if slot == self.selected { ">" } else { " " };
                        format!("{marker}{}:{name} {blocks}b+{spares}n", slot + 1)
                    })
                    .collect::<Vec<_>>()
                    .join("  ")
            },
            // The tool, and what it does. The brush is the point: a player
            // needs to know whether the next click takes a cell or a cube, and
            // that is the whole reason sub-nodes exist.
            match self.held_tool() {
                Some(tool) => format!("holding {} ({} brush)", tool.name, tool.brush),
                None => "no tools registered — nothing here can be dug".to_owned(),
            },
            // What the crosshair is on, which is the other half of knowing why
            // a placement went where it did.
            match self.looking_at() {
                Some(hit) => {
                    let target = self.place_target().map_or_else(String::new, |cell| {
                        format!(" → {},{},{}", cell.x, cell.y, cell.z)
                    });
                    format!(
                        "looking at cell {},{},{} face {:?}{target}",
                        hit.cell[0], hit.cell[1], hit.cell[2], hit.normal
                    )
                }
                None => "looking at nothing in reach".to_owned(),
            },
            // The floating-origin check is a human gate, and a gate nobody can
            // find the key for gets reported as "nothing happened".
            "LMB dig · RMB place · R: tool · 1-9/wheel: slot · T/F8: jump 50,000 · H/F7: home"
                .to_owned(),
        ]
    }

    /// Warnings the player should see, newest last.
    #[must_use]
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn warn(&mut self, text: String) {
        tracing::warn!("{text}");
        self.warnings.push(text);
        if self.warnings.len() > WARNING_HISTORY {
            self.warnings.remove(0);
        }
    }
}

/// The compass direction a yaw points, for the HUD.
///
/// Yaw 0 looks along +z, and the axes are named the way the world is: +x east,
/// +z north.
#[must_use]
pub fn compass(yaw: f32) -> &'static str {
    let turns = yaw / std::f32::consts::TAU;
    // Eight sectors, offset by half a sector so "north" spans the boundary
    // rather than starting at it.
    let sector = ((turns * 8.0 + 0.5).rem_euclid(8.0)) as usize;
    [
        "north",
        "north-east",
        "east",
        "south-east",
        "south",
        "south-west",
        "west",
        "north-west",
    ][sector.min(7)]
}

/// Packs decoded textures into an atlas indexed by material id.
///
/// The atlas slot **is** the material id, which is what lets the shader turn a
/// vertex's material into tile coordinates with arithmetic rather than a
/// lookup table. Ids the server did not send a texture for become the magenta
/// checker; ids that are simply absent from the table — there are none in
/// practice, but the table is a server's word — become it too.
#[must_use]
pub fn build_atlas(
    table: &[tiamot_core::proto::MaterialDef],
    images: &BTreeMap<u16, Image>,
) -> Atlas {
    let highest = table.iter().map(|entry| entry.id).max().unwrap_or(0);
    let mut slots: Vec<Option<Image>> = vec![None; usize::from(highest) + 1];

    for entry in table {
        // Air is never drawn — the mesher culls it before a quad exists — so
        // giving it a tile would waste one and, worse, make an atlas of only
        // air and unknown look like it had content.
        if entry.id == MaterialId::AIR.0 {
            continue;
        }
        if let Some(image) = images.get(&entry.id) {
            slots[usize::from(entry.id)] = Some(image.clone());
        }
    }

    Atlas::build(&slots)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiamot_core::proto::MaterialDef;

    /// An `Input` holding one direction, everything else neutral.
    fn pressing(forward: f32, right: f32) -> Input {
        Input {
            forward,
            right,
            ..Input::default()
        }
    }

    #[test]
    fn strafing_walks_the_way_the_camera_calls_right() {
        // The bug this pins: `intent_at_yaw` built its own strafe axis, got the
        // sign wrong, and sent the player left when they pressed D. Free-fly
        // asks `Camera::right()` and was always correct, so the two disagreed
        // for a whole task without any test noticing. Deriving the expected
        // answer FROM the camera is the point — a test that hard-codes the
        // numbers would have been written from the same wrong reasoning as the
        // code, and would have passed.
        for eighth in 0..8 {
            let yaw = std::f32::consts::TAU * eighth as f32 / 8.0;
            let camera = Camera {
                yaw,
                ..Camera::default()
            };

            let strafe = intent_at_yaw(yaw, pressing(0.0, 1.0)).walk;
            let expected = camera.right();
            assert!(
                (strafe[0] - expected.x).abs() < 1e-5 && (strafe[1] - expected.z).abs() < 1e-5,
                "at yaw {yaw} pressing right walked {strafe:?}, but the camera calls right \
                 ({}, {})",
                expected.x,
                expected.z
            );

            // And forward, on the same basis, so a future edit cannot fix one
            // axis by rotating both.
            let ahead = intent_at_yaw(yaw, pressing(1.0, 0.0)).walk;
            let facing = camera.forward();
            assert!(
                (ahead[0] - facing.x).abs() < 1e-5 && (ahead[1] - facing.z).abs() < 1e-5,
                "at yaw {yaw} pressing forward walked {ahead:?}, but the camera faces ({}, {})",
                facing.x,
                facing.z
            );
        }
    }

    #[test]
    fn pressing_right_while_facing_north_walks_east() {
        // The concrete case a player reports, spelled out in compass terms so
        // the sign is readable without composing two rotations in your head.
        // `Camera::forward` documents east as −x.
        let strafe = intent_at_yaw(0.0, pressing(0.0, 1.0)).walk;
        assert!(
            strafe[0] < -0.9,
            "facing north, pressing right should walk east at −x, not {strafe:?}"
        );
        // The counter-example: the inverted version walks to +x, which the
        // camera calls west. Without this line the assertion above would still
        // pass on an implementation that had merely scaled the axis.
        assert!(
            strafe[0] < 0.0,
            "pressing right walked west; A and D are swapped"
        );
    }

    #[test]
    fn pacing_reports_the_worst_frame_and_not_the_average() {
        // The whole reason this type exists. A second of 900 fps with one 11 ms
        // frame in it averages to 900 fps — the hitch a player actually sees
        // rounds away to nothing. Charter rule 18 measures pacing.
        let mut pacing = Pacing::default();
        for _ in 0..899 {
            pacing.frame(1.0 / 900.0);
        }
        pacing.remesh(11.0, 0.4, 4);
        pacing.frame(0.011);
        // One more frame to close the window and publish it.
        pacing.frame(1.0 / 900.0);

        assert!(
            (pacing.worst_frame_ms() - 11.0).abs() < 0.01,
            "the window reported {} ms; the 11 ms frame was averaged away, which is the bug \
             this type exists to prevent",
            pacing.worst_frame_ms()
        );
        assert_eq!(
            pacing.worst_remesh_ms(),
            (11.0, 4),
            "the remesh that coincided with the worst frame has to be reported with it, or \
             there is no way to tell a meshing hitch from any other kind"
        );
    }

    #[test]
    fn a_new_pacing_window_forgets_the_last_one() {
        // Otherwise the worst frame is the worst frame EVER, and a single stall
        // during startup would sit on the HUD for the rest of the session
        // claiming the client still hitches.
        let mut pacing = Pacing::default();
        pacing.remesh(11.0, 0.4, 4);
        pacing.frame(0.011);
        pacing.frame(1.0);
        assert!((pacing.worst_frame_ms() - 1000.0).abs() < 0.01);

        // A quiet second after it.
        for _ in 0..60 {
            pacing.frame(1.0 / 60.0);
        }
        pacing.frame(1.0 / 60.0);
        assert!(
            pacing.worst_frame_ms() < 20.0,
            "a quiet second still reported {} ms from the stall before it",
            pacing.worst_frame_ms()
        );
        assert_eq!(
            pacing.worst_remesh_ms(),
            (0.0, 0),
            "the remesh figure outlived its window too"
        );
    }

    #[test]
    fn the_compass_names_every_sector_and_wraps() {
        // Yaw 0 looks along +z, which the world calls north.
        assert_eq!(compass(0.0), "north");
        assert_eq!(compass(std::f32::consts::FRAC_PI_2), "east");
        assert_eq!(compass(std::f32::consts::PI), "south");
        assert_eq!(compass(std::f32::consts::PI * 1.5), "west");
        // And a yaw just short of a full turn is north again rather than
        // indexing past the end of the table.
        assert_eq!(compass(std::f32::consts::TAU - 0.01), "north");
    }

    #[test]
    fn the_atlas_puts_each_material_in_its_own_id_slot() {
        // The shader turns a material id into tile coordinates arithmetically,
        // so slot and id must be the same number. A packed atlas that skipped
        // unused ids would draw every block with its neighbour's texture.
        let table = vec![
            MaterialDef {
                id: 0,
                name: "engine:air".to_owned(),
                texture: None,
            },
            MaterialDef {
                id: 5,
                name: "core:white".to_owned(),
                texture: Some([0u8; 32]),
            },
        ];
        let mut images = BTreeMap::new();
        images.insert(5u16, Image::white_with_border());

        let atlas = build_atlas(&table, &images);
        let uv = atlas.tile_uv(atlas.slot_of(5));
        let elsewhere = atlas.tile_uv(atlas.slot_of(4));
        assert_ne!(uv, elsewhere, "material 5 must not share a tile with 4");

        // And the tile it points at is the texture that was supplied, not the
        // placeholder.
        let side = atlas.side() as f32;
        let x = (uv.0 * side) as u32;
        let y = (uv.1 * side) as u32;
        assert_eq!(
            atlas.image.pixel(x + 8, y + 8),
            Image::white_with_border().pixel(8, 8)
        );
    }

    #[test]
    fn a_material_with_no_texture_gets_the_magenta_checker() {
        // Visibly wrong beats invisible: a player reports "this block looks
        // broken" rather than "there is a hole in the world".
        let table = vec![MaterialDef {
            id: 2,
            name: "mod:untextured".to_owned(),
            texture: None,
        }];
        let atlas = build_atlas(&table, &BTreeMap::new());

        let uv = atlas.tile_uv(atlas.slot_of(2));
        let side = atlas.side() as f32;
        let x = (uv.0 * side) as u32;
        let y = (uv.1 * side) as u32;
        assert_eq!(atlas.image.pixel(x, y), Image::missing().pixel(0, 0));
    }

    #[test]
    fn the_remesh_budget_is_a_small_fraction_of_a_frame() {
        // Task 02b measured a realistic chunk at 0.108 ms. The budget exists so
        // a join — where a hundred chunks land at once — fills in over several
        // frames instead of stalling one.
        let worst_case_ms = REMESH_BUDGET as f64 * 0.108;
        assert!(
            worst_case_ms < 16.0 / 4.0,
            "a full remesh budget is {worst_case_ms} ms, too much of a 16 ms frame"
        );

        // But that arithmetic only holds while a chunk costs what the spike
        // measured, and the reported hitch was a debug build where it cost
        // 2.97 ms — 27x — turning this "safe" budget into 12 ms of a frame. The
        // time budget is what holds when the per-chunk assumption does not, so
        // it has to be the smaller of the two bounds on a slow build.
        assert!(
            REMESH_TIME_BUDGET < std::time::Duration::from_millis(16 / 4),
            "the time budget is not a bound on a quarter of a 16 ms frame"
        );
        let slow_build_ms = REMESH_BUDGET as f64 * 2.97;
        assert!(
            REMESH_TIME_BUDGET.as_secs_f64() * 1000.0 < slow_build_ms,
            "at the debug build's measured {slow_build_ms} ms for a full count budget, the \
             time budget has to be what stops the frame, and it is not"
        );
    }

    /// Lays out a string and returns how wide it came out.
    fn measure(ctx: &egui::Context, text: &str) -> f32 {
        // A pass has to have run before there are fonts to lay out with.
        // `run_ui` rather than `run`: egui 0.35 renamed it and hands back a
        // `Ui` instead of a `Context`.
        let _ = ctx.run_ui(egui::RawInput::default(), |_| {});
        ctx.fonts_mut(|fonts| {
            fonts
                .layout_no_wrap(
                    text.to_owned(),
                    egui::FontId::monospace(14.0),
                    egui::Color32::WHITE,
                )
                .size()
                .x
        })
    }

    #[test]
    fn the_hud_has_a_font_to_draw_with() {
        // egui is built WITHOUT `default_fonts`, because its bundled ones are
        // under licences this project would rather not argue about. That makes
        // `install_fonts` load-bearing: forget it and egui has no glyphs, so
        // the HUD renders nothing at all — no panic, no warning, just an empty
        // corner of the screen that looks like a HUD bug rather than a missing
        // font.
        //
        // The counter-example is the whole test. A context WITHOUT the call
        // must measure zero, or this would pass on a build that still had the
        // default fonts and prove nothing about the vendored one.
        let bare = egui::Context::default();
        assert_eq!(
            measure(&bare, "1234567890").to_bits(),
            0.0f32.to_bits(),
            "egui came with fonts of its own; this test cannot tell whether the vendored font is \
             installed"
        );

        let ctx = egui::Context::default();
        install_fonts(&ctx);
        assert!(
            measure(&ctx, "1234567890") > 1.0,
            "the vendored font produced no glyphs"
        );
    }

    #[test]
    fn the_font_is_monospaced_so_the_hud_does_not_jitter() {
        // The HUD is mostly numbers that change every frame. In a proportional
        // font the columns shuffle sideways as digits change, which reads as
        // the readout being unstable rather than the values being.
        let ctx = egui::Context::default();
        install_fonts(&ctx);

        let narrow = measure(&ctx, "1111111111");
        let wide = measure(&ctx, "0000000000");
        assert!(
            (narrow - wide).abs() < 0.01,
            "digits are not the same width: {narrow} against {wide}"
        );
    }
}
