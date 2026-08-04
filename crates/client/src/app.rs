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
    /// Chunks rebuilt by that worst remesh.
    worst_remesh_chunks: usize,
    /// The last completed window's worst frame, in milliseconds.
    ///
    /// Reported rather than the live figure so the readout holds still long
    /// enough to be read off a screen or a screenshot.
    reported_frame: f32,
    /// The last completed window's worst remesh, in milliseconds.
    reported_remesh: f32,
    /// Chunks rebuilt by that remesh.
    reported_remesh_chunks: usize,
}

impl Pacing {
    /// How long a window is before its worst figures are published, in seconds.
    const WINDOW: f32 = 1.0;

    /// Folds one frame's duration in, publishing the window when it is full.
    fn frame(&mut self, dt: f32) {
        self.worst_frame = self.worst_frame.max(dt * 1000.0);
        self.elapsed += dt;
        if self.elapsed >= Self::WINDOW {
            self.reported_frame = self.worst_frame;
            self.reported_remesh = self.worst_remesh;
            self.reported_remesh_chunks = self.worst_remesh_chunks;
            *self = Self {
                reported_frame: self.reported_frame,
                reported_remesh: self.reported_remesh,
                reported_remesh_chunks: self.reported_remesh_chunks,
                ..Self::default()
            };
        }
    }

    /// Folds one remesh's duration in.
    fn remesh(&mut self, millis: f32, chunks: usize) {
        if millis > self.worst_remesh {
            self.worst_remesh = millis;
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
                        self.renderer.remove_chunk(self.drawn_at(pos));
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

    /// Remeshes up to [`REMESH_BUDGET`] chunks, nearest to the camera first.
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

        // Timed because the cost that matters is not the meshing — it is
        // `set_chunk`, which creates two fresh GPU buffers per chunk and drops
        // the old ones. On a software rasteriser that is a `malloc` and
        // measures as nothing; on a real driver it is device-memory churn that
        // can stall for milliseconds. No test on a headless CI box can tell the
        // difference, so the client measures itself and puts the number where a
        // human running it can read it.
        let started = std::time::Instant::now();
        for pos in &due {
            let Some(chunk) = self.store.get(*pos) else {
                continue;
            };
            let neighbours = self.store.neighbours(*pos);
            let mesh = mesher::mesh_chunk(chunk, &neighbours, ABSENT_POLICY);
            self.renderer.set_chunk(self.drawn_at(*pos), &mesh);
        }
        self.pacing
            .remesh(started.elapsed().as_secs_f32() * 1000.0, due.len());
        due.len()
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

        // Whatever time did not buy a whole tick is how far through the current
        // one this frame is, and the camera is drawn there rather than at the
        // tick boundary. Without it the camera moves 20 times a second no
        // matter how fast the client draws.
        self.follow_body(self.tick_carry / TICK_SECONDS);
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
        let worst = self.pacing.worst_frame_ms();

        vec![
            format!("{:.0} fps", self.fps),
            // The average above is the reassuring number; this is the honest
            // one. Charter rule 18 measures pacing, and a 900 fps average with
            // an 11 ms worst frame is a hitch the average cannot express.
            format!(
                "worst frame {worst:.1} ms ({:.0} fps) · worst remesh {remesh_ms:.1} ms over \
                 {remesh_chunks} chunks",
                if worst > 0.0 { 1000.0 / worst } else { 0.0 }
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
            // The floating-origin check is a human gate, and a gate nobody can
            // find the key for gets reported as "nothing happened".
            "T or F8: jump 50,000 blocks · H or F7: home".to_owned(),
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
        pacing.remesh(11.0, 4);
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
        pacing.remesh(11.0, 4);
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
