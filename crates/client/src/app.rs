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

/// How far you can see inside a fluid, in blocks.
///
/// Sixteen — one chunk. Far enough to make out the shape of a pool you are
/// swimming in, close enough that being under is unmistakable and that the
/// world beyond dissolves rather than merely tinting. Not a mod's choice yet: a
/// fluid declares its colour, and how far light carries through it is a second
/// knob that can be added when something wants one.
const UNDERWATER_VISIBILITY: f32 = 16.0;

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
/// The light mode 1 meshes against.
///
/// Full daylight everywhere, so every cell's shading key is identical and
/// greedy merging is unaffected by light. Mode 1 is Task 08's world, and Task
/// 08's world had no propagated light to split a quad on.
const FLAT_DAYLIGHT: crate::shade::Uniform =
    crate::shade::Uniform(tiamot_core::light::Light::DAYLIGHT);

/// How far behind the player the third-person camera sits, in blocks.
const THIRD_PERSON_DISTANCE: f64 = 4.0;

/// How many chat lines the client keeps.
///
/// A session on a busy server would otherwise hold every line anybody said for
/// as long as it ran. Enough to scroll back through a conversation.
const MAX_CHAT_LINES: usize = 200;

/// How many of the player's slots the number keys reach.
///
/// **The engine's number, not a mod's.** Charter rule 11 puts key bindings
/// here, and the engine registers `engine:hotbar_1` through `_9` — so nine is
/// how many places there are to select. What a HUD DRAWS is still the mod's
/// (`core_ui/hud.lua` has its own `SLOTS`), and one that drew five would show
/// five of these nine.
const HOTBAR_SLOTS: usize = 9;

/// The heading a figure needs to face the way a camera is looking.
///
/// # Two conventions, and they are mirror images
///
/// A figure's heading is the one a mod writes: `atan2(dx, dz)`, so facing is
/// `(sin θ, cos θ)` — that is what a mod computes for an entity and what the
/// rig's vertex shader turns by. The camera's yaw counts the other way, because
/// the world is right-handed with `+z` north and east is `−x`, so its forward
/// is `(−sin θ, cos θ)`.
///
/// Negating is the whole conversion, and getting it wrong is invisible until
/// you turn: at yaw zero both point north and agree exactly. Reported from the
/// window as not facing the way you are walking.
const fn figure_yaw(camera_yaw: f32) -> f32 {
    tiamot_core::ent::figure_yaw(camera_yaw)
}

/// Below this the player's figure stands still, in cells per tick.
///
/// Not zero: a body resting against geometry keeps a hair of velocity from the
/// solver, and a figure that walked on the spot whenever somebody leant on a
/// wall would look broken in a way nobody could describe.
const IDLE_SPEED: f32 = 0.05;

/// Above this it runs rather than walks, in cells per tick.
const RUN_SPEED: f32 = 0.55;

/// Most dialog events held while waiting to send them.
///
/// A player mashing buttons on a stalled connection must not grow this without
/// limit. Generous for anything a human does between two network sends, and
/// finite against a client that cannot reach its server at all — the oldest are
/// kept and the excess dropped, because the first click is the one that meant
/// something.
const MAX_QUEUED_DIALOG_EVENTS: usize = 256;

const INPUT_LEAD: u64 = 4;

/// One frame's row in the per-frame log, and where it is going.
struct FrameLog {
    writer: std::io::BufWriter<std::fs::File>,
    frame: u64,
    started: std::time::Instant,
}

/// Rows the per-frame log will write before it stops.
///
/// A minute at a thousand frames a second. Bounded because the log exists to be
/// read, and a file too large to open is a file that answers nothing.
const MAX_LOGGED_FRAMES: u64 = 60_000;

/// How many consecutive ticks one jump press is sent for.
///
/// Two, and the ceiling on it is physical rather than chosen: a jump is only
/// honoured from the ground, so an extra copy is a no-op while the body is
/// airborne — but the shortest airtime in the game is the three-tick hop under a
/// low ceiling, and a window that reached it would let one press jump twice.
///
/// One tick was not enough. `InputQueue::offer` refuses an input whose tick the
/// server has already passed, so a single late packet lost the whole jump while
/// the client had already taken it, and the two simulations diverged by a jump's
/// arc — the `worst correction 5.37 cells` reported at a landing.
const JUMP_EDGE_TICKS: u8 = 2;

/// How many skipped ticks a resynchronise will simulate rather than renumber.
///
/// Four, matching `walk`'s own catch-up bound and for the same reason: past this
/// the client was not lagging, it was away, and replaying seconds of movement in
/// one frame would fast-forward the player through the world.
const MAX_RESYNC_CATCH_UP: u32 = 4;

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
#[expect(
    clippy::struct_excessive_bools,
    reason = "one flag per movement key, which is what a keyboard is; a bitfield here would \
              be less legible at every call site than the four it replaces"
)]
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
    /// Whether flight is on.
    ///
    /// **What the server GRANTED, not what was pressed.** A client that flew
    /// locally while the server refused would be corrected back to the ground
    /// every tick, which is worse than not flying at all.
    pub fly: bool,
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
        // **Predicted the same way the server will judge it.** A client that
        // flew locally while the server refused would be corrected back to the
        // ground every tick; one that did not predict it would lurch a round
        // trip behind its own key. So the client tracks what the server GRANTED
        // — see `App::may_fly` — rather than what the player pressed.
        fly: input.fly,
    }
}

/// Where one frame's time went, in milliseconds.
///
/// **Measured for every frame, kept for the worst one.** Independent per-phase
/// maxima do not add up to the worst frame — they are maxima of different
/// frames — so a breakdown assembled that way can account for 3 ms of an 11 ms
/// hitch and leave no way to tell whether the missing 8 ms was one phase or all
/// of them. These are the phases of the frame that actually hitched.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct Phases {
    /// Draining the network and inserting what arrived.
    pub network: f32,
    /// [`App::remesh`], which is also reported on its own.
    pub remesh: f32,
    /// Movement, prediction, and aiming.
    pub advance: f32,
    /// Waiting for a swapchain image. **Time spent here is the GPU or the
    /// compositor holding the frame, not work the client is doing.**
    pub acquire: f32,
    /// Recording and submitting the world pass.
    pub world: f32,
    /// Laying out and drawing the HUD.
    pub hud: f32,
    /// Presenting. Blocks on the swapchain for the same reason `acquire` does.
    pub present: f32,
}

impl Phases {
    /// Everything accounted for, in milliseconds.
    #[must_use]
    pub fn total(&self) -> f32 {
        self.network
            + self.remesh
            + self.advance
            + self.acquire
            + self.world
            + self.hud
            + self.present
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
///
/// [`Phases`] is the general form of that argument, added after a reading came
/// back with an 8.8 ms worst frame beside a 2.3 ms worst remesh — enough to
/// clear the mesher and not enough to convict anything else.
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
    /// Where the worst frame of the window spent its time.
    worst_phases: Phases,
    /// Largest prediction correction seen in the window, in cells.
    worst_correction: f32,
    /// How much of that correction was vertical — see
    /// `predict::Predictor::vertical_share`.
    worst_correction_vertical: f32,
    /// The largest per-tick disagreement this window, in cells.
    ///
    /// Not the same number as the correction: a correction is the gap between
    /// the newest prediction and a replay, which grows with the input lead. This
    /// compares ONE tick's two answers, so it is the honest measure of whether
    /// the two simulations agree.
    worst_divergence: f32,
    /// Frames that reached the screen this window.
    ///
    /// **Counted separately from frames STARTED, and the gap is the diagnostic.**
    /// It found its bug: with the swapchain asked for LAST, a frame that could
    /// not get an image had already pumped the network, spent a full remesh
    /// budget and advanced the world, and threw all of it away — measured at
    /// `211 fps · 103 presented` during a streaming burst. The image is now
    /// acquired first, so these two should track each other; a gap that reopens
    /// means frames are being built and dropped again.
    presented: u32,
    /// How often the body changed its mind about being on the ground.
    ///
    /// **The number that says "jolting" out loud.** A body walking over even
    /// ground is on it every tick and a body falling is off it every tick; a body
    /// alternating is one whose support is flickering, and no amount of camera
    /// smoothing makes that feel right. Counted per tick rather than per frame,
    /// because it is a question about the simulation.
    footing_changes: u32,
    /// What the footing was on the previous tick, to notice the changes.
    last_footing: bool,
    /// Whether prediction consulted a chunk it does not have this window.
    ///
    /// **The instrument for a correction nobody can explain.** The client's
    /// collision treats an absent chunk as solid, because the alternative is
    /// falling out of the world — but the server has that chunk and walks
    /// straight through where the client hit a wall. The two then disagree for as
    /// long as the chunk is late, and the disagreement arrives as a correction
    /// the player feels. Guessing at that from "1,044 chunks held" is what the
    /// frame-pacing chase did with one number for a week; this measures it.
    predicted_into_the_unloaded: bool,
    /// Ticks this window where the client's own world contradicted the server's
    /// answer about the ground.
    ///
    /// **The instrument that tells a physics bug from a streaming one.** Every
    /// other number here measures the two simulations against each other, and
    /// they can only ever say "these disagree". This asks a different question:
    /// given where the server says the body is, does the client's copy of the
    /// world even have ground there? If it does not — and it holds the chunk, so
    /// it is not guessing — the two are not simulating the same world, and no
    /// amount of input bookkeeping will make them agree.
    terrain_conflicts: u32,
    /// Every terrain contradiction since the client started.
    ///
    /// **Carried across the window boundary, unlike everything else here.** The
    /// windowed figures exist to be read off a screen, which means they forget;
    /// a test asking "did this ever happen" cannot use a number that is only
    /// true for a second. Summing the windowed one per frame is worse than
    /// useless — it counts one event sixty times and reported forty for a
    /// session whose trace held none.
    terrain_conflicts_total: u32,
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
    /// How much of it was vertical.
    reported_correction_vertical: f32,
    /// Whether that window's prediction reached into chunks that had not
    /// arrived.
    reported_unloaded: bool,
    /// The last completed window's count of footing changes.
    reported_footing_changes: u32,
    /// The last completed window's count of frames that reached the screen.
    reported_presented: u32,
    /// The last completed window's largest per-tick disagreement.
    reported_divergence: f32,
    /// The last completed window's count of terrain contradictions.
    reported_terrain_conflicts: u32,
    /// Where that window's worst frame spent its time.
    reported_phases: Phases,
}

impl Pacing {
    /// How long a window is before its worst figures are published, in seconds.
    const WINDOW: f32 = 1.0;

    /// Folds one frame's duration in, publishing the window when it is full.
    ///
    /// `phases` are the phases of the frame `dt` measures, which is the frame
    /// *before* this one: `dt` is the gap between two frame starts, so it is
    /// the previous frame's duration, and the caller records phases when a
    /// frame ends. Pairing them the other way would label every hitch with the
    /// work of the frame that came after it.
    fn frame(&mut self, dt: f32, phases: Phases) {
        let millis = dt * 1000.0;
        if millis > self.worst_frame {
            self.worst_frame = millis;
            self.worst_phases = phases;
        }
        self.elapsed += dt;
        if self.elapsed >= Self::WINDOW {
            *self = Self {
                reported_frame: self.worst_frame,
                reported_remesh: self.worst_remesh,
                reported_remesh_meshing: self.worst_remesh_meshing,
                reported_remesh_chunks: self.worst_remesh_chunks,
                reported_correction: self.worst_correction,
                reported_correction_vertical: self.worst_correction_vertical,
                reported_unloaded: self.predicted_into_the_unloaded,
                reported_footing_changes: self.footing_changes,
                reported_presented: self.presented,
                reported_divergence: self.worst_divergence,
                reported_terrain_conflicts: self.terrain_conflicts,
                // Carried across the window boundary, or the first tick of every
                // window would read as a change.
                last_footing: self.last_footing,
                // Carried for the opposite reason: it is the one figure here
                // that is about the whole session rather than this second.
                terrain_conflicts_total: self.terrain_conflicts_total,
                reported_phases: self.worst_phases,
                ..Self::default()
            };
        }
    }

    /// Folds in how far this frame's prediction was corrected, in cells.
    fn correction(&mut self, cells: f32, vertical_share: f32) {
        if cells > self.worst_correction {
            self.worst_correction = cells;
            self.worst_correction_vertical = vertical_share;
        }
    }

    /// Notes that prediction collided against a chunk that has not arrived.
    const fn predicted_into_the_unloaded(&mut self) {
        self.predicted_into_the_unloaded = true;
    }

    /// Folds in one tick's disagreement with the server.
    fn divergence(&mut self, cells: f32) {
        self.worst_divergence = self.worst_divergence.max(cells);
    }

    /// Notes a tick where the client's world contradicted the server's ground.
    const fn terrain_conflict(&mut self) {
        self.terrain_conflicts += 1;
        self.terrain_conflicts_total += 1;
    }

    /// Notes that a frame reached the screen.
    const fn presented(&mut self) {
        self.presented += 1;
    }

    /// Folds in this tick's footing, counting the changes.
    const fn footing(&mut self, on_ground: bool) {
        if on_ground != self.last_footing {
            self.footing_changes += 1;
            self.last_footing = on_ground;
        }
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

    /// Where that worst frame spent its time.
    #[must_use]
    pub const fn worst_frame_phases(&self) -> Phases {
        self.reported_phases
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

    /// How much of the worst correction was vertical, as a percentage.
    ///
    /// Read it beside the magnitude: a disagreement about a jump is nearly all
    /// vertical, and one about walking into geometry is not.
    /// `+ 0.5` and a truncating cast rather than `round`, which the determinism
    /// lint bans workspace-wide. This is a HUD readout and could have taken an
    /// exemption; not needing one is better than explaining one.
    #[must_use]
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a share of 0..=1 scaled to a percentage, for a HUD line"
    )]
    pub fn worst_correction_vertical_percent(&self) -> u32 {
        (self.reported_correction_vertical * 100.0 + 0.5) as u32
    }

    /// The largest per-tick disagreement with the server in the last window.
    ///
    /// **Read this rather than the correction when asking whether the two
    /// simulations agree.** A correction includes the replay; this does not.
    #[must_use]
    pub const fn worst_divergence_cells(&self) -> f32 {
        self.reported_divergence
    }

    /// How many frames reached the screen in the last completed window.
    ///
    /// Read it against the frame rate. Equal means every frame the client built
    /// was shown. Far below means the loop is building frames the swapchain
    /// cannot take — each one having already pumped the network, remeshed and
    /// advanced the world before finding out.
    #[must_use]
    pub const fn presented_last_second(&self) -> u32 {
        self.reported_presented
    }

    /// How many times the body changed between standing and airborne in the
    /// last completed window.
    ///
    /// Two is a jump. Twenty is a body being held up and dropped twenty times a
    /// second, which is what a player means by jolting — and it distinguishes
    /// that from a camera artefact, because this counts the SIMULATION's answer.
    #[must_use]
    pub const fn footing_changes_last_second(&self) -> u32 {
        self.reported_footing_changes
    }

    /// Whether the last window's prediction collided against chunks that had
    /// not arrived.
    ///
    /// **Read this beside the correction.** A non-zero correction with this set
    /// is the client having invented a wall the server does not have, which is a
    /// streaming problem; a non-zero correction WITHOUT it is the two simulations
    /// genuinely disagreeing, which is a physics problem. They want opposite
    /// fixes and the number alone cannot tell them apart.
    #[must_use]
    pub const fn predicted_into_unloaded(&self) -> bool {
        self.reported_unloaded
    }

    /// Ticks in the last window where the client's world had no ground where
    /// the server says the body is standing.
    ///
    /// Zero is the only acceptable value. Anything else means the client is
    /// holding a stale copy of a chunk, and the corrections that follow are a
    /// symptom rather than the fault.
    #[must_use]
    pub const fn terrain_conflicts_last_second(&self) -> u32 {
        self.reported_terrain_conflicts
    }

    /// Every terrain contradiction since the client started.
    ///
    /// The figure to assert on: the windowed one above forgets, and a crossing
    /// is over in three ticks.
    #[must_use]
    pub const fn terrain_conflicts_total(&self) -> u32 {
        self.terrain_conflicts_total
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

/// How often a held attack button throws another punch.
///
/// The length of the rig's own swing clip. A dig re-aimed is the same dig and
/// costs nothing to repeat; a punch re-sent is another punch, and a held button
/// at a hundred frames a second would be a hundred of them.
const PUNCH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(400);

/// How long one swing of a hand takes.
///
/// Short enough that a held dig reads as repeated swings rather than one long
/// wave, and long enough to see. Not tied to the dig's own timing: a block that
/// takes four seconds is not one slow swing.
const SWING_TIME: std::time::Duration = std::time::Duration::from_millis(250);

/// Where a ray enters a box, in the ray's own units, or `None` if it misses.
///
/// The slab test. Division by a zero direction component gives an infinity
/// rather than a `NaN`, and the `min`/`max` ordering below is what makes the
/// infinities cancel — which is the whole reason this is written with a
/// reciprocal rather than with a branch per axis.
fn ray_box(origin: [f32; 3], direction: [f32; 3], min: [f32; 3], max: [f32; 3]) -> Option<f32> {
    let mut near = f32::NEG_INFINITY;
    let mut far = f32::INFINITY;
    for axis in 0..3 {
        let inverse = 1.0 / direction[axis];
        let first = (min[axis] - origin[axis]) * inverse;
        let second = (max[axis] - origin[axis]) * inverse;
        near = near.max(first.min(second));
        far = far.min(first.max(second));
    }
    // Behind the eye, or no overlap at all.
    (far >= near.max(0.0)).then(|| near.max(0.0))
}

/// The client, between frames.
///
/// Several independent debug toggles, which clippy counts as too many bools.
/// They ARE independent — borders, source outlines and the time override answer
/// unrelated questions and are turned on and off separately — so folding them
/// into an enum or a bitflags type would be obeying the lint rather than the
/// reason for it.
#[allow(clippy::struct_excessive_bools)]
pub struct App {
    config: Config,
    /// Every named action: the engine's, plus whatever the server's mods added.
    ///
    /// Here rather than in the window because this is where a server's messages
    /// land — `Event::Actions` arrives on join and the window only ever reads
    /// the result. Charter rule 11's split, in the type layout: the client owns
    /// bindings, and a mod owns nothing but the name.
    actions: crate::input::Actions,
    /// What each action is bound to.
    bindings: crate::input::Bindings,
    /// Every sound the server's mods registered.
    sounds: Vec<tiamot_core::proto::SoundDef>,
    /// Dialog events the player has raised and the server has not been told.
    ///
    /// Queued rather than sent immediately for the reason every other client
    /// message is: sending happens on the network side, and the renderer runs
    /// inside a frame.
    dialog_events: Vec<crate::dialog::Raised>,
    /// Chat lines received, newest last.
    ///
    /// Engine-native, because moderation and RCON depend on chat existing
    /// whatever mods a server runs — a chat that arrived with a mod would be a
    /// chat an operator could not rely on.
    chat: std::collections::VecDeque<String>,
    /// Whether the chat input line is open and taking keys.
    chat_open: bool,
    /// Whether the input line still needs to be given keyboard focus.
    ///
    /// Set when chat opens and taken once. **Asking every frame is what broke
    /// sending**: egui reports a single-line field's Enter as `lost_focus`, and
    /// a field handed focus back every frame never loses it, so Enter did
    /// nothing at all.
    chat_focus: bool,
    /// What is typed into it.
    chat_draft: String,
    /// What each inventory view holds, as the server last said.
    ///
    /// The server's answer, not the client's belief: a slot moves when the
    /// server says it moved, which is why a click sends a request and this
    /// arrives afterwards.
    views: std::collections::BTreeMap<String, crate::dialog::ViewContents>,
    /// Dialogs the server has open on this player's screen, by form name.
    ///
    /// A `BTreeMap` so the draw order is the same every frame: two dialogs
    /// open at once should not swap places because a hash changed.
    dialogs: std::collections::BTreeMap<String, crate::dialog::Screen>,
    /// The audio backend, or a silent stand-in where there is no device.
    mixer: crate::audio::Mixer,
    /// Which sound each named event plays, as the server last said.
    ///
    /// Kept as a map rather than the wire's list: load order decides a
    /// conflict, so the list is folded once on arrival and the LAST binding for
    /// a cue is the one that survives.
    cues: std::collections::BTreeMap<String, String>,
    /// The block this dig is locked onto while the button is held.
    ///
    /// **A block brush finishes the block it started on.** Sub-nodes come away
    /// as you dig, so within half a second the raycast is looking THROUGH the
    /// hole it just made and lands on whatever is behind — which retargets the
    /// dig, throws away the progress, and starts chewing the next block while
    /// the first stands half-eaten. Reported from the window exactly that way.
    ///
    /// Cleared by releasing the button, and by the block running out. A chisel
    /// does not lock: taking one named cell is the whole point of it, and
    /// pointing through a hole to reach the cell behind is what it is for.
    dig_lock: Option<tiamot_core::SubNodePos>,
    /// Whether the player was on the ground last frame, for the landing cue.
    was_on_ground: bool,
    /// The sandbox that runs pushed HUD scripts, if one could be built.
    ///
    /// `None` means the Lua runtime itself would not start, which is an engine
    /// fault rather than a mod one — the client goes on drawing its own HUD and
    /// says so once, because a client that refused to run without a scripting
    /// VM would be a client nobody could play on.
    hud_vm: Option<tiamot_core::script::HudVm>,
    /// Sounds this client has been told about and not yet played.
    ///
    /// A queue rather than an immediate call, because playing one belongs to
    /// the audio backend and this is the frame loop. Charter rule 4's scope
    /// note applies: none of this is simulation, so nothing here has to be
    /// deterministic.
    heard: Vec<crate::net::Event>,
    /// What each material sounds like to walk on, by world material id.
    step_sounds: std::collections::BTreeMap<u16, String>,
    /// Distance walked since the last footstep, in blocks.
    stride: f32,
    /// Where the body was when that distance was last measured.
    last_step_at: [f32; 3],
    /// Whether the volumes have changed since they were written out.
    volumes_dirty: bool,
    /// Where the interface-scale slider has been dragged to and not let go of.
    ///
    /// Held rather than applied, because applying it moves the slider — see
    /// [`crate::widget::settle`].
    ui_scale_draft: Option<f32>,
    /// Whether the settings screen is showing.
    settings_open: bool,
    /// Whether the player asked to quit from the menu.
    quit_requested: bool,
    /// Materials that may not be put in the world: the items.
    ///
    /// See [`tiamot_core::proto::MaterialDef::placeable`]. Empty until a server
    /// sends its table, which is correct: a client with no table has nothing to
    /// place either.
    items: std::collections::BTreeSet<u16>,
    /// Whether the world this client is looking at is stopped.
    ///
    /// Only ever true for an embedded server the client paused itself: a hosted
    /// world has other people in it and does not stop for anybody's menu. See
    /// [`App::walk`].
    world_paused: bool,
    /// Where each material sits in the atlas, for whatever draws a slot.
    ///
    /// Kept beside the renderer's copy rather than read back out of it: the
    /// interface needs the layout, the renderer needs the pixels, and the
    /// atlas itself — several megabytes for a large mod set — is dropped once
    /// it is on the GPU.
    tiles: crate::texture::TileMap,
    /// Whether the atlas texture changed and egui has not been told.
    atlas_changed: bool,
    /// When the hands last started a swing.
    ///
    /// One clock for both, because they swing for the same reasons and never at
    /// the same time — a dig or a placement is one action, whichever hand it
    /// came from.
    swung_at: Option<std::time::Duration>,
    /// Whether the pause menu is on the screen.
    ///
    /// **Escape opens a menu rather than only releasing the cursor.** Releasing
    /// it was all Escape did, so the settings screen was reachable by one
    /// undocumented function key and the interface had no front door at all.
    menu_open: bool,
    /// The action waiting for a key, while the settings screen captures one.
    ///
    /// While this is set the window sends EVERY press here instead of acting on
    /// it, which is what lets a player rebind a key that is already bound to
    /// something — including the key that opens this screen.
    rebinding: Option<String>,
    /// Whether the bindings have changed since they were last written out.
    ///
    /// The `App` does not know where the file is — the window does — so it
    /// raises a flag rather than saving, and the window writes it. One place
    /// knows the path and one place knows the state.
    bindings_dirty: bool,
    connection: Connection,
    renderer: Renderer,
    store: ChunkStore,
    /// Entities the server has told this client about.
    entities: crate::entities::Entities,
    /// This machine's clock, for stamping when an update arrived.
    ///
    /// **A local monotonic base, never the server's tick.** See
    /// `crate::entities`: relating two machines' clocks is a thing that can be
    /// wrong, and when it is wrong it shows entities in a future nobody has
    /// been told about. Arrival time cannot drift because it measures nothing
    /// about the other machine.
    since_start: std::time::Instant,
    /// When the last punch was thrown, so a held button is a swing rather than
    /// a hundred of them a second. See [`App::dig`].
    last_punch: std::time::Duration,
    camera: Camera,
    /// Material name by id, for the HUD and for diagnostics.
    materials: BTreeMap<u16, String>,
    /// Where the server said to start. `None` until the world is joined.
    spawn: Option<Position>,
    /// Whether the world has been joined.
    joined: bool,
    /// The radius the server is actually streaming, in chunks.
    ///
    /// **Separate from `config.view_distance`, which stays the player's
    /// PREFERENCE.** The server clamps to its own limit, so the two differ
    /// whenever a server is stingier than this client asked to be — and
    /// overwriting the preference with the grant would make a reconnect ask for
    /// what it was last given rather than what the player configured, ratcheting
    /// the world smaller every time they joined a strict server.
    ///
    /// The fog is drawn from this one, so the world ends in haze rather than in
    /// clear air.
    granted_view: tiamot_core::interest::ViewDistance,
    /// The most recent warnings, newest last.
    warnings: Vec<String>,
    /// A smoothed frame rate, for the HUD.
    fps: f32,
    /// The most recent frame's duration, in seconds, for the per-frame log.
    ///
    /// The smoothed rate above is the readable number and the wrong one for a
    /// log: a hitch is one row, and an average hides it.
    last_dt: f32,
    /// Frame pacing over the last second, and what the remesh cost during it.
    pacing: Pacing,
    /// Where the frame that just ended spent its time, waiting to be paired
    /// with the `dt` that measures it on the next frame.
    last_phases: Phases,
    /// The sky a mod described, and where the day stands.
    ///
    /// Starts as [`crate::sky::Sky::none`] — a world with no day — and is
    /// replaced when the server sends one. That default is not a placeholder:
    /// it is what a world whose mods register no sky legitimately looks like.
    sky: crate::sky::Sky,
    /// The server's tick when it last said so.
    tick: u64,
    /// Whether the camera sits behind the player rather than in their eyes.
    ///
    /// A debugging affordance for looking at shadows: the world holds still and
    /// the only moving caster is the player, so a first-person camera can never
    /// see the one shadow that shows whether the cascades follow anything.
    third_person: bool,
    /// Whether the clock has been scrubbed by hand.
    ///
    /// Presentation only, and it has to be: the sunlight the server stores is
    /// always full daylight and the client scales it by time of day at draw
    /// time (see `Globals::sun_intensity`), so moving the clock locally shows
    /// exactly what that hour looks like without the world disagreeing about
    /// anything. What it does NOT do is change when mobs spawn or anything else
    /// the server decides — this is for looking at the sky, not for playing.
    time_override: bool,
    /// The address others can join this world at, if it is open to the LAN.
    ///
    /// **A host who cannot tell anybody where to connect has not hosted
    /// anything**, so this is shown on the pause menu rather than logged.
    hosting: Option<String>,
    /// Whether the server has said this player may fly.
    may_fly: bool,
    /// Whether flight is on.
    flying: bool,
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
    /// Where a per-FRAME log is being written, if one was asked for.
    ///
    /// The physics trace above records one line per server message, which is the
    /// right grain for "did the two simulations agree". This is the other grain:
    /// one line per frame, with the phase timings, the streaming counters and the
    /// body all on the same row, so a hitch can be lined up against what the
    /// client was doing when it happened. Bounded, because a thousand frames a
    /// second fills a disk faster than it fills a diagnosis.
    frames: Option<std::cell::RefCell<FrameLog>>,
    /// Where a per-tick physics trace is being written, if one was asked for.
    ///
    /// **The tool for a disagreement that will not reproduce.** A HUD reports a
    /// maximum over a second; this writes one line per tick, so the exact tick
    /// two simulations part company on can be read off afterwards rather than
    /// guessed at from a sampled number. Opened once, from
    /// `TIAMOT_TRACE_PHYSICS`, and silently absent otherwise — a diagnostic that
    /// could refuse to start a session would be a poor one.
    trace: Option<std::cell::RefCell<std::io::BufWriter<std::fs::File>>>,
    /// The present mode the swapchain is actually using, once the window has
    /// told us.
    ///
    /// **Reported instead of the `vsync` config flag**, because the two spent a
    /// week disagreeing: the HUD said "vsync on" beside 1,200 fps, which strict
    /// vsync cannot produce, and nothing on screen could tell a requested mode
    /// from an effective one. A headless `App` has no swapchain and says so.
    present_mode: Option<&'static str>,
    /// Ticks left to keep sending the current jump press.
    ///
    /// See [`JUMP_EDGE_TICKS`].
    jump_edge: u8,
    /// The intent applied on the previous simulation tick.
    ///
    /// What a resynchronise repeats for the ticks it has to catch up on — the
    /// same thing `InputQueue::take` repeats server-side for a tick nobody spoke
    /// for, so the two agree about what happened while nobody was talking.
    previous_intent: Intent,
    /// The keys as they stood at the previous simulation tick.
    ///
    /// For edge detection: "was this pressed" and "is this newly pressed" are
    /// different questions, and only the second one can express one hop per
    /// press. Kept as a whole [`Input`] rather than one flag per action because
    /// the next action wanting an edge should not need another field.
    previous_input: Input,
    /// What the server says the player is carrying, in ascending material
    /// order and in **units** (charter rule 5).
    ///
    /// Server-authoritative and never edited here: the client is told what it
    /// has. An inventory a client could change is not an inventory.
    carried: Vec<tiamot_core::proto::StackDef>,
    /// The hotbar: the first [`HOTBAR_SLOTS`] slots of `player:main`.
    ///
    /// Derived from the view rather than sent separately, so what the number
    /// keys reach and what the inventory screen's top row shows cannot drift.
    hotbar: Vec<Option<tiamot_core::proto::StackDef>>,
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
        Self::with_bindings(
            config,
            connection,
            renderer,
            crate::input::Bindings::default(),
        )
    }

    /// As [`App::new`], with the player's saved key bindings.
    ///
    /// Separate so tests and the bot can build an `App` without a bindings file
    /// on disk, which is the overwhelmingly common case for both.
    #[must_use]
    pub fn with_bindings(
        config: Config,
        connection: Connection,
        mut renderer: Renderer,
        bindings: crate::input::Bindings,
    ) -> Self {
        let camera = Camera {
            fov_y: config.fov_degrees.to_radians(),
            ..Camera::default()
        };
        // **Every renderer setting the config holds, pushed once, here.**
        // Reported from the window: shadows turned off on the front screen
        // were still on after joining. The renderer is built with its own
        // defaults and each of these had exactly one caller — the in-game key
        // that toggles it — so a setting chosen anywhere else reached the file
        // and the HUD and never reached the frame.
        //
        // Anything settable from the front screen belongs in this function,
        // and the test below counts them so a new one cannot be added to the
        // settings tab and quietly skipped here.
        apply_to_renderer(&config, &mut renderer);
        // Opened before `config` is moved into the struct below. Never fails:
        // a machine with no sound device runs the game silently.
        let mixer = crate::audio::Mixer::open(config.volumes.clone());

        Self {
            // Until the server says otherwise, assume it grants what was
            // asked. A client that drew no world until the grant arrived would
            // flash empty for a round trip on every join.
            granted_view: config.view(),
            config,
            actions: crate::input::Actions::engine(),
            bindings,
            sounds: Vec::new(),
            dialogs: std::collections::BTreeMap::new(),
            views: std::collections::BTreeMap::new(),
            chat: std::collections::VecDeque::new(),
            chat_open: false,
            chat_focus: false,
            chat_draft: String::new(),
            dialog_events: Vec::new(),
            mixer,
            heard: Vec::new(),
            step_sounds: std::collections::BTreeMap::new(),
            items: std::collections::BTreeSet::new(),
            hosting: None,
            may_fly: false,
            flying: false,
            world_paused: false,
            stride: 0.0,
            last_step_at: [0.0; 3],
            volumes_dirty: false,
            ui_scale_draft: None,
            settings_open: false,
            menu_open: false,
            quit_requested: false,
            tiles: crate::texture::TileMap::default(),
            atlas_changed: false,
            swung_at: None,
            rebinding: None,
            bindings_dirty: false,
            connection,
            renderer,
            store: ChunkStore::new(),
            entities: crate::entities::Entities::new(),
            since_start: std::time::Instant::now(),
            last_punch: std::time::Duration::ZERO,
            camera,
            materials: BTreeMap::new(),
            spawn: None,
            joined: false,
            warnings: Vec::new(),
            cues: std::collections::BTreeMap::new(),
            dig_lock: None,
            was_on_ground: true,
            hud_vm: match tiamot_core::script::HudVm::new(tiamot_core::script::HudLimits::default())
            {
                Ok(vm) => Some(vm),
                Err(err) => {
                    tracing::warn!(%err, "no HUD script runtime; pushed HUDs will not run");
                    None
                }
            },
            fps: 0.0,
            last_dt: 0.0,
            pacing: Pacing::default(),
            last_phases: Phases::default(),
            sky: crate::sky::Sky::none(),
            tick: 0,
            third_person: false,
            time_override: false,
            server_label: "connecting…".to_owned(),
            predictor: None,
            confirmed: None,
            confirmed_tick: 0,
            dig: None,
            tick_carry: 0.0,
            trace: None,
            frames: None,
            present_mode: None,
            jump_edge: 0,
            previous_intent: Intent::default(),
            previous_input: Input::default(),
            carried: Vec::new(),
            hotbar: vec![None; HOTBAR_SLOTS],
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

        // **Ahead is a bug too, and it used to be a permanent one.**
        //
        // Reported from the window: after opening the pause menu in a
        // singleplayer world, walking put the player straight back where they
        // started, for ever, and their own body stopped animating. A paused
        // world stops the SERVER's tick and not this one, so the client counts
        // on through the menu — and `InputQueue::offer` refuses anything more
        // than `MAX_LOOKAHEAD` past the tick being applied. The gap never
        // closes on its own, because the server only catches up at the same
        // 20 Hz the client is still running at. Every input after that menu was
        // thrown away, which is exactly what "the game thinks it is still
        // paused" looks like from inside.
        //
        // The bound is the server's own, not a number of this module's: this
        // has to trigger before an input would be refused rather than after.
        // See `resync_plan`, which decides and is tested on its own.
        let steps = match resync_plan(self.tick, server_tick) {
            Resync::Settled => return,
            Resync::Ahead => {
                self.tick = want;
                if let Some(predictor) = self.predictor.as_mut() {
                    predictor.renumber(want);
                }
                return;
            }
            Resync::Behind { steps } => steps,
        };

        // **The counter is not the body, and moving one without the other is what
        // made a landing jolt.**
        //
        // Renumbering alone left the body short of its own tick label: the server
        // simulates every tick between, so its state for tick N had one more step
        // in it than the client's memory of N. Traced, at a jump:
        //
        //     tick 50 d +0.0000 -1.0584 +0.0000 dv +0.0000 -0.2400 +0.0000
        //     tick 51 d +0.0000 -1.2984 +0.0000 dv +0.0000 -0.2400 +0.0000
        //
        // `-0.2400` is exactly `Tuning::gravity` — one tick of it, every tick,
        // which is a body one step behind rather than one in the wrong place. The
        // gap only shows while accelerating, which is why walking looked perfect
        // and every landing did not.
        //
        // So the skipped ticks are simulated rather than skipped, with the intent
        // the server uses for a tick nobody spoke for: the last one, minus the
        // jump, exactly as `InputQueue::take` repeats it. Bounded, because a
        // client that has been away for a minute must not replay a minute.
        let gap = steps;
        let mut intent = self.previous_intent;
        intent.jump = false;
        for _ in 0..gap {
            self.tick += 1;
            if let Some(predictor) = self.predictor.as_mut() {
                let voxels = phys::Voxels::with_fluid(&self.store, &self.store, predictor.origin());
                predictor.predict(&voxels, self.tick, intent, &Tuning::DEFAULT);
            }
        }
        // Whatever is left after the bound is a renumber, as before: past that
        // distance the client was not lagging, it was absent.
        self.tick = want;
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

    /// The key reminders, with the state of the settings that have one.
    ///
    /// Its own method because `hud` is at the line limit. Every key here was
    /// added for somebody to look at something with, and a key nobody can find is
    /// a feature nobody has.
    fn keys_line(&self) -> String {
        format!(
            "keys: L light · K shadows · V third person · B borders {} · N sources · [ ] time \
             · \\ resync \
             · G blocks · Y/H teleport · T chat · E inventory · F1 settings · F3 this",
            if self.renderer.chunk_borders() {
                "ON"
            } else {
                "off"
            }
        )
    }

    /// The frame-rate line: frames built, and frames that became pictures.
    ///
    /// Its own method because `hud` is at the line limit, and the two numbers
    /// belong together: apart they invite the reading that a big one is good.
    fn frame_rate_line(&self) -> String {
        format!(
            "{:.0} fps · {} presented",
            self.fps,
            self.pacing.presented_last_second()
        )
    }

    /// The buffer-pool and prediction line of the HUD.
    ///
    /// Its own method because `hud` is at the line limit and because this line
    /// carries the one readout that needs a caveat attached to it: a correction
    /// is only a physics disagreement if prediction was working from geometry it
    /// actually had. See [`Pacing::predicted_into_unloaded`].
    fn prediction_line(&self, created: u64, reused: u64, correction: f32) -> String {
        format!(
            "{created} mesh buffers created, {reused} reused from the pool · worst correction \
             {correction:.2} cells ({}% vertical) · diverge {:.2} · footing {}x · tick {}/{} \
             (lead {}){}",
            self.pacing.worst_correction_vertical_percent(),
            self.pacing.worst_divergence_cells(),
            self.pacing.footing_changes_last_second(),
            self.tick,
            self.confirmed_tick,
            // Signed, because behind and ahead are different bugs: the server
            // refuses an input whose tick it has passed, so a lead that has gone
            // to zero or negative means inputs are being thrown away, while a
            // lead far above `INPUT_LEAD` means the client is predicting into a
            // future the server has not been told about.
            self.tick as i64 - self.confirmed_tick as i64,
            // Only when it happened, so the line stays short in the normal case
            // and the words appear exactly when they explain something.
            if self.pacing.predicted_into_unloaded() {
                " (predicted into unloaded chunks)"
            } else {
                ""
            }
        ) + &self.terrain_conflict_note()
    }

    /// The words that appear only when the two sides hold different worlds.
    ///
    /// Silent in the normal case, on purpose. A HUD that always shows a zero
    /// teaches the eye to skip the place the number will appear.
    fn terrain_conflict_note(&self) -> String {
        let conflicts = self.pacing.terrain_conflicts_last_second();
        if conflicts == 0 {
            String::new()
        } else {
            format!(" · STALE TERRAIN {conflicts}x")
        }
    }

    /// Takes the server's word on where this player is.
    ///
    /// Its own method because `pump_network` is at the line limit, and because
    /// everything here is one thought: adopt the server's answer, measure how far
    /// it was from ours, and say so.
    fn accept_player_state(&mut self, state: &crate::predict::Authoritative) {
        self.confirmed = Some((state.chunk, state.local));
        self.confirmed_tick = state.last_processed_input;
        self.resynchronise_tick(state.last_processed_input);

        // Not while the debug teleport is displacing the world: the server does
        // not know about it, so every state would drag the camera back and the
        // floating-origin check could not be looked at.
        if self.displacement != [0, 0, 0] {
            return;
        }
        let Some(predictor) = self.predictor.as_mut() else {
            return;
        };

        // **With the fluid, because the replay must be the same simulation the
        // server ran.** Reconciliation re-steps the unconfirmed ticks; replaying
        // them against a world with no milk in it would put a swimming player
        // back where a falling one would have been, and the correction would
        // arrive as a lurch every time the server's state message landed.
        let voxels = phys::Voxels::with_fluid(&self.store, &self.store, predictor.origin());
        predictor.reconcile(&voxels, state, &Tuning::DEFAULT);
        let divergence = predictor.divergence();
        // Asked after the replay rather than before: what matters is whether the
        // ticks being re-simulated consulted geometry the client does not have,
        // because those are the ticks whose answer cannot match the server's.
        let touched_absent = voxels.touched_absent();

        if let Some(divergence) = divergence {
            self.pacing.divergence(divergence.distance);
            if divergence.terrain_contradicted() {
                self.pacing.terrain_conflict();
            }
        }
        self.trace(divergence.as_ref(), state);
        if touched_absent {
            self.pacing.predicted_into_the_unloaded();
        }
    }

    /// Starts writing a per-frame log to `path`, as CSV.
    ///
    /// Returns whether the file could be opened. Asked for explicitly, like the
    /// physics trace, and for the same reason — see [`App::trace_physics_to`].
    pub fn log_frames_to(&mut self, path: &std::path::Path) -> bool {
        use std::io::Write as _;

        let Ok(file) = std::fs::File::create(path) else {
            return false;
        };
        let mut writer = std::io::BufWriter::new(file);
        // Named in full: a column nobody can identify is a column nobody reads.
        let header = "frame,elapsed_ms,dt_ms,presented,net_ms,remesh_ms,advance_ms,acquire_ms,\
                      world_ms,hud_ms,present_ms,fps,chunks_held,meshed,drawn,queued,\
                      buffers_created,buffers_reused,tick,confirmed,lead,worst_correction,\
                      vertical_pct,worst_divergence,footing_changes,unloaded,terrain_conflicts,\
                      body_x,body_y,body_z,vel_x,vel_y,vel_z,on_ground,step_lag,camera_y\n";
        if writer.write_all(header.as_bytes()).is_err() {
            return false;
        }
        self.frames = Some(std::cell::RefCell::new(FrameLog {
            writer,
            frame: 0,
            started: std::time::Instant::now(),
        }));
        true
    }

    /// Writes one frame's row, if a log was asked for.
    ///
    /// `presented` is the fact the phase timings cannot supply: a frame that did
    /// everything and never reached the screen looks identical in every other
    /// column.
    pub fn log_frame(&self, phases: &Phases, presented: bool) {
        use std::io::Write as _;

        let Some(log) = self.frames.as_ref() else {
            return;
        };
        let Ok(mut log) = log.try_borrow_mut() else {
            return;
        };
        if log.frame >= MAX_LOGGED_FRAMES {
            return;
        }
        log.frame += 1;
        // Read out before the write borrows the writer mutably.
        let frame = log.frame;
        let elapsed = log.started.elapsed().as_secs_f32() * 1000.0;

        let (tick, confirmed) = self.tick_pair();
        let (created, reused) = self.renderer.buffer_stats();
        let body = self.predictor.as_ref().map(super::predict::Predictor::body);
        let position = body.map_or([0.0; 3], |body| body.position);
        let velocity = body.map_or([0.0; 3], |body| body.velocity);
        let on_ground = body.is_some_and(|body| body.on_ground);
        let step_lag = self
            .predictor
            .as_ref()
            .map_or(0.0, super::predict::Predictor::step_lag);

        let _ = writeln!(
            log.writer,
            "{},{:.3},{:.3},{},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.3},{:.0},{},{},{},{},\
             {created},{reused},{tick},{confirmed},{},{:.4},{},{:.4},{},{},{},{:.4},{:.4},{:.4},\
             {:.4},{:.4},{:.4},{},{:.4},{:.4}",
            frame,
            elapsed,
            self.last_dt * 1000.0,
            u8::from(presented),
            phases.network,
            phases.remesh,
            phases.advance,
            phases.acquire,
            phases.world,
            phases.hud,
            phases.present,
            self.fps,
            self.store.len(),
            self.renderer.chunk_count(),
            self.renderer.drawn(),
            self.store.dirty_len(),
            tick as i64 - confirmed as i64,
            self.pacing.worst_correction_cells(),
            self.pacing.worst_correction_vertical_percent(),
            self.pacing.worst_divergence_cells(),
            self.pacing.footing_changes_last_second(),
            u8::from(self.pacing.predicted_into_unloaded()),
            self.pacing.terrain_conflicts_last_second(),
            position[0],
            position[1],
            position[2],
            velocity[0],
            velocity[1],
            velocity[2],
            u8::from(on_ground),
            step_lag,
            self.camera.position.to_world().1,
        );
    }

    /// Starts writing a per-tick physics trace to `path`.
    ///
    /// Returns whether the file could be opened. **Asked for explicitly rather
    /// than read from the environment here**: the binary reads
    /// `TIAMOT_TRACE_PHYSICS` and calls this, so a test can turn tracing on for
    /// its own client without touching process-global state. Doing it the other
    /// way round cost a red CI run — the test binary runs its cases on parallel
    /// threads, so one test's `set_var` and another's `remove_var` raced, and the
    /// trace came out empty on the machine that lost.
    pub fn trace_physics_to(&mut self, path: &std::path::Path) -> bool {
        match std::fs::File::create(path) {
            Ok(file) => {
                self.trace = Some(std::cell::RefCell::new(std::io::BufWriter::new(file)));
                true
            }
            Err(_) => false,
        }
    }

    /// Writes one line of the physics trace, if one was asked for.
    ///
    /// Deliberately one line per server message rather than per frame: the
    /// question it exists to answer is which TICK the two sides disagreed about,
    /// and a frame is not a tick.
    fn trace(
        &self,
        divergence: Option<&crate::predict::Divergence>,
        state: &crate::predict::Authoritative,
    ) {
        use std::io::Write as _;

        let Some(trace) = self.trace.as_ref() else {
            return;
        };
        let Ok(mut out) = trace.try_borrow_mut() else {
            return;
        };

        // **A line either way, and the missing case is the interesting one.**
        //
        // A comparison needs the client to remember the tick the server is
        // talking about. When it does not, the first version wrote nothing — so a
        // client skipping the very ticks the server was applying produced a
        // SHORTER trace, which is the opposite of what a diagnostic should do.
        // It also made the test flaky: macOS CI wrote four lines where this box
        // wrote seventy.
        let Some(divergence) = divergence else {
            let _ = writeln!(
                out,
                "tick {} client_tick {} unmatched — no memory of this tick",
                state.last_processed_input, self.tick
            );
            let _ = out.flush();
            return;
        };
        let [dx, dy, dz] = divergence.offset;
        // Ignored deliberately: a diagnostic that could take a session down by
        // failing to write to a file would be worse than no diagnostic.
        let _ = writeln!(
            out,
            "tick {} client_tick {} d {dx:+.4} {dy:+.4} {dz:+.4} dist {:.4} \
             dv {:+.4} {:+.4} {:+.4} footing_agreed {} server_ground {} \
             there loaded {} ground {} inside {}",
            divergence.tick,
            self.tick,
            divergence.distance,
            divergence.velocity_offset[0],
            divergence.velocity_offset[1],
            divergence.velocity_offset[2],
            divergence.footing_agreed,
            state.on_ground,
            divergence.there.loaded,
            divergence.there.ground,
            divergence.there.inside,
        );
        let _ = out.flush();
    }

    /// Notes that a frame reached the screen, for the HUD.
    ///
    /// Called by whatever owns the swapchain, because only it knows whether the
    /// frame it started ever became a picture.
    pub const fn note_presented(&mut self) {
        self.pacing.presented();
    }

    /// Records which present mode the swapchain ended up with, for the HUD.
    ///
    /// Called by whatever owns the surface — a headless `App` never calls it and
    /// falls back to reporting the request.
    pub const fn set_present_mode(&mut self, mode: &'static str) {
        self.present_mode = Some(mode);
    }

    /// Frame pacing over the last completed second.
    #[must_use]
    pub const fn pacing(&self) -> &Pacing {
        &self.pacing
    }

    /// Records where the frame that has just finished spent its time.
    ///
    /// Called at the END of a frame by whatever is driving it, because only the
    /// driver sees the phases outside [`App`] — acquiring a swapchain image,
    /// presenting — and those are the two that can block on something other
    /// than the client's own work. A headless caller that never calls this gets
    /// a zeroed breakdown, which reads as "not measured" rather than as "no
    /// time spent".
    pub const fn record_phases(&mut self, phases: Phases) {
        self.last_phases = phases;
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
        self.place_aim().map(|(target, _)| target)
    }

    /// The same, with the face the placement would be made against.
    ///
    /// The face is the outward normal of the surface, which is what turns a
    /// crafted cut toward the player — or toward their feet on a wall. Only
    /// this side knows what the crosshair is on; what it MEANS is the server's
    /// (charter rule 2, and `place::oriented`).
    #[must_use]
    pub fn place_aim(&self) -> Option<(tiamot_core::SubNodePos, [i8; 3])> {
        let hit = self.looking_at()?;
        let target = self.target_of([
            hit.cell[0] + hit.normal[0],
            hit.cell[1] + hit.normal[1],
            hit.cell[2] + hit.normal[2],
        ])?;
        let face = [
            i8::try_from(hit.normal[0]).unwrap_or(0),
            i8::try_from(hit.normal[1]).unwrap_or(0),
            i8::try_from(hit.normal[2]).unwrap_or(0),
        ];
        Some((target, face))
    }

    /// A row of one block of every material the server registered, laid out
    /// from where the crosshair is pointing.
    ///
    /// # What this is for, and what it deliberately is not
    ///
    /// It is a **debug affordance for singleplayer**, and the only way it can
    /// touch the world is through the embedded server's own handle — the same
    /// `seed_block` the integration tests arrange worlds with. A client cannot
    /// edit a world it is connected to (Task 09 retired `BlockDelta` for good
    /// reasons) and this does not change that: on a remote server the caller
    /// has no handle and the key does nothing.
    ///
    /// It names no material. The client cannot know which block a mod calls a
    /// lamp — charter rule 1 puts that entirely in `game/` — so it lays out
    /// ONE OF EACH and lets whoever asked look at them. That is more useful
    /// anyway: "show me every block this server has" answers questions a
    /// hard-coded lamp cannot.
    ///
    /// Returns the blocks to write, or nothing if the crosshair is on the sky.
    #[must_use]
    pub fn debug_material_row(&self) -> Vec<(tiamot_core::BlockPos, u16)> {
        let Some(target) = self.place_target() else {
            return Vec::new();
        };
        let start = target.block();
        self.materials
            .keys()
            // Not air, which is a hole, and not the unknown placeholder, which
            // is what a world shows where a mod that once registered a block is
            // no longer loaded (charter rule 8). Neither is a sample of
            // anything.
            .filter(|id| {
                **id != tiamot_core::MaterialId::AIR.0 && **id != tiamot_core::MaterialId::UNKNOWN.0
            })
            .enumerate()
            .map(|(index, id)| {
                (
                    tiamot_core::BlockPos::new(
                        start.x + i32::try_from(index).unwrap_or(0),
                        start.y,
                        start.z,
                    ),
                    *id,
                )
            })
            .collect()
    }

    /// The entity under the crosshair, if one is nearer than the block behind
    /// it.
    ///
    /// **Picked on the client and judged on the server.** The client says which
    /// entity it believes it hit; the server checks the attacker could reach it
    /// and the mods decide what a hit means (charter rule 2 — a viewer that
    /// could assert a hit could assert every hit). So this being approximate is
    /// fine, and it being generous is not exploitable.
    ///
    /// The box is the entity's own collider, which is the same box the server
    /// collided it with — aiming at something and missing because the client
    /// drew it somewhere else is the one failure this must not have.
    #[must_use]
    pub fn punch_target(&self) -> Option<u64> {
        let predictor = self.predictor.as_ref()?;
        let origin = predictor.origin();
        let eye = predictor.body().eye();
        let forward = self.camera.forward();
        let direction = [forward.x, forward.y, forward.z];
        let now = self.since_start.elapsed();
        let span = tiamot_core::CHUNK_SUBNODES as f32;

        // How far the terrain is, so a mob behind a wall cannot be hit through
        // it. The cell's own corner rather than its centre: a half-cell either
        // way is far below what anybody can aim.
        let blocked_at = self.looking_at().map_or(phys::REACH, |hit| {
            let to = [
                hit.cell[0] as f32 - eye[0],
                hit.cell[1] as f32 - eye[1],
                hit.cell[2] as f32 - eye[2],
            ];
            (to[0] * to[0] + to[1] * to[1] + to[2] * to[2]).sqrt()
        });

        let mut nearest: Option<(f32, u64)> = None;
        for (id, entity) in self.entities.iter() {
            let Some([width, height]) = entity.collider else {
                continue;
            };
            let Some(pose) = entity.pose(now) else {
                continue;
            };
            // Into the predicted body's frame, in cells — the frame the ray is
            // already in (charter rule 7).
            let feet = [
                (pose.chunk.x - origin.x) as f32 * span + pose.local[0],
                (pose.chunk.y - origin.y) as f32 * span + pose.local[1],
                (pose.chunk.z - origin.z) as f32 * span + pose.local[2],
            ];
            let half = width / 2.0;
            let min = [feet[0] - half, feet[1], feet[2] - half];
            let max = [feet[0] + half, feet[1] + height, feet[2] + half];
            let Some(distance) = ray_box(eye, direction, min, max) else {
                continue;
            };
            if distance > phys::REACH || distance > blocked_at {
                continue;
            }
            if nearest.is_none_or(|(best, _)| distance < best) {
                nearest = Some((distance, id));
            }
        }
        nearest.map(|(_, id)| id)
    }

    /// Starts or re-aims a dig at whatever the crosshair is on — or throws a
    /// punch, if what the crosshair is on is somebody.
    ///
    /// Re-sent every frame the button is held, which is what `StartDig`'s
    /// protocol docs ask for: re-aiming at the same cell keeps its progress, so
    /// repeating is free and it means a dig follows the crosshair.
    ///
    /// **A punch is not free to repeat**, which is why it has a cooldown of its
    /// own. A dig re-aimed is the same dig; a punch re-sent is another punch,
    /// and a held button at a hundred frames a second would be a hundred of
    /// them a second. One per swing is what a swing is.
    pub fn dig(&mut self) {
        self.swing();
        if let Some(entity) = self.punch_target() {
            let now = self.since_start.elapsed();
            if now.saturating_sub(self.last_punch) >= PUNCH_INTERVAL {
                self.last_punch = now;
                self.connection.send(Command::Punch { entity });
            }
            // Nothing is being dug: the crosshair is on a person. Cancelling
            // any dig in progress, or a swing at somebody standing in front of
            // a wall keeps chipping at the wall behind them.
            self.stop_digging();
            return;
        }
        let Some(target) = self.held_dig_target() else {
            return;
        };
        self.dig_lock = Some(target);
        self.connection.send(Command::Dig {
            target: Some(target),
        });
    }

    /// What this frame's dig is aimed at, honouring the held-button lock.
    ///
    /// See [`App::dig_lock`]. The lock survives only while the block it names
    /// still has something in it — once it is empty the crosshair chooses
    /// again, so holding the button walks along a wall one whole block at a
    /// time instead of boring a tunnel through several at once.
    fn held_dig_target(&self) -> Option<tiamot_core::SubNodePos> {
        if self.locks_onto_a_block()
            && let Some(locked) = self.dig_lock
            && let Some(to_block) = self.toward(locked)
            && keeps_lock(
                to_block,
                self.camera.forward().into(),
                self.block_has_material(locked),
            )
        {
            return Some(locked);
        }
        self.dig_target()
    }

    /// From the eye to the centre of a cell's block, in cells.
    ///
    /// `None` before there is a body to look from.
    fn toward(&self, cell: tiamot_core::SubNodePos) -> Option<[f32; 3]> {
        let predictor = self.predictor.as_ref()?;
        let origin = predictor.origin();
        let span = tiamot_core::CHUNK_SUBNODES as i32;
        let eye = predictor.body().eye();
        let block = cell.block();
        // The block's middle, in the predictor's own chunk frame — the same
        // space `looking_at` casts its ray in.
        let axis = |world: i32, chunk: i32| {
            #[expect(
                clippy::cast_precision_loss,
                reason = "a cell offset within a few chunks of the player"
            )]
            let corner = (world * tiamot_core::SUBNODES_PER_AXIS as i32 - chunk * span) as f32;
            corner + 1.5
        };
        Some([
            axis(block.x, origin.x) - eye[0],
            axis(block.y, origin.y) - eye[1],
            axis(block.z, origin.z) - eye[2],
        ])
    }

    /// Whether the tool in hand takes whole blocks.
    fn locks_onto_a_block(&self) -> bool {
        self.held_tool()
            .is_none_or(|tool| tool.brush != tiamot_core::dig::Brush::SubNode.name())
    }

    /// Whether the block containing this cell still has any material in it.
    fn block_has_material(&self, cell: tiamot_core::SubNodePos) -> bool {
        let Some(predictor) = self.predictor.as_ref() else {
            return false;
        };
        let origin = predictor.origin();
        let span = tiamot_core::CHUNK_SUBNODES as i32;
        let block = cell.block();
        let voxels = phys::Voxels::new(&self.store, origin);
        let base = [
            block.x * tiamot_core::SUBNODES_PER_AXIS as i32 - origin.x * span,
            block.y * tiamot_core::SUBNODES_PER_AXIS as i32 - origin.y * span,
            block.z * tiamot_core::SUBNODES_PER_AXIS as i32 - origin.z * span,
        ];
        let span = i32::from(u8::try_from(tiamot_core::SUBNODES_PER_AXIS).unwrap_or(3));
        (0..span).any(|z| {
            (0..span).any(|y| {
                (0..span).any(|x| {
                    voxels
                        .material(base[0] + x, base[1] + y, base[2] + z)
                        .is_some_and(|material| !material.is_air())
                })
            })
        })
    }

    /// Stops digging, discarding progress.
    pub fn stop_digging(&mut self) {
        self.dig = None;
        // **The lock is the button, not the block.** Releasing frees the
        // crosshair even mid-block, which is what makes a half-dug block
        // something a player can walk away from and come back to.
        self.dig_lock = None;
        self.connection.send(Command::Dig { target: None });
    }

    /// Places the selected material against the face under the crosshair.
    ///
    /// **How much lands is the held tool's business, not this method's.** The
    /// brush decides (Sub-Node Contract §7.1): a chisel fills the single cell
    /// across the face, a whole-block tool fills the block bottom-up. The
    /// client's job is to name the cell the player is pointing at, which is
    /// [`App::place_target`] — deciding what to do with it is the server's
    /// (charter rule 2).
    ///
    /// Nothing happens with an empty inventory or nothing in reach. Anything
    /// else the server may still refuse — it owns that decision — and says why,
    /// which arrives as a warning.
    pub fn place(&mut self) {
        // **Which STACK, not merely which material.** A player placing a stair
        // must spend the stairs they crafted rather than the loose rubble
        // beside them, so the cut goes with the request and the server matches
        // the pair.
        self.swing();
        let Some(stack) = self.hotbar.get(self.selected).copied().flatten() else {
            self.warn("nothing selected to build with".to_owned());
            return;
        };
        // **Told here rather than after a round trip.** The server refuses this
        // too and owns the decision, but a client that asked would leave the
        // player watching nothing happen for the length of a round trip. See
        // `App::items`.
        if self.items.contains(&stack.material) {
            self.warn("that is not something you can build with".to_owned());
            return;
        }
        let Some((target, face)) = self.place_aim() else {
            return;
        };
        self.connection.send(Command::Place {
            target,
            material: stack.material,
            shape: stack.shape,
            face,
        });
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
        // **The outline follows the dig, not the crosshair.** While a block
        // brush is locked onto a block, that block stays highlighted even
        // though the ray now passes through the hole being made — the outline
        // is what tells a player which block their button is spending time on,
        // and having it jump to the one behind while they were still digging
        // the first was half of what "point through it" looked like.
        let Some(cell) = self.held_dig_target() else {
            return Vec::new();
        };

        let whole_block = self.locks_onto_a_block();
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

    /// Records the names and footstep sounds from a material table.
    ///
    /// Split out because `pump_network` is at clippy's line limit. The step
    /// sounds live in their own map rather than being looked up through the
    /// table: a footstep is decided every couple of blocks walked, and a scan
    /// each time would be work for nothing.
    fn adopt_materials(&mut self, table: &[tiamot_core::proto::MaterialDef]) {
        self.materials = table
            .iter()
            .map(|entry| (entry.id, entry.name.clone()))
            .collect();
        self.step_sounds = table
            .iter()
            .filter_map(|entry| entry.step_sound.clone().map(|sound| (entry.id, sound)))
            .collect();
        // **What may not be built with.** The server refuses a placement of one
        // anyway — it owns that decision (charter rule 2) — but a client that
        // sent the request would spend a round trip to be told no, and the
        // player would watch their sword not become a block with no idea why.
        self.items = table
            .iter()
            .filter(|entry| !entry.placeable)
            .map(|entry| entry.id)
            .collect();
    }

    /// Replaces the mod-registered actions with a server's.
    ///
    /// Its own method because `pump_network` is at clippy's line limit, and
    /// because the two ways this can go wrong both want explaining.
    fn adopt_actions(&mut self, actions: Vec<tiamot_core::proto::ActionDef>) {
        // A fresh server, a fresh set. `clear_mods` keeps the
        // engine's own: those are the client's and outlive any
        // connection.
        self.actions.clear_mods();
        for def in actions {
            let default = crate::input::parse_key(&def.default_key);
            if default.is_none() && !def.default_key.is_empty() {
                // **Dropped, not refused.** A server naming a key
                // this build has never heard of is a mod written
                // against a newer winit, and the action is still
                // perfectly usable once the player binds it. A join
                // that failed over a key name would be worse.
                tracing::warn!(
                    action = %def.id,
                    key = %def.default_key,
                    "a mod asked for a default key this client does not know"
                );
            }
            let action = crate::input::Action {
                id: def.id,
                description: def.description,
                source: crate::input::Source::Mod(def.mod_id),
                default,
            };
            if let Err(err) = self.actions.register(action) {
                // A server sending the same id twice, or claiming
                // `engine:`. Neither is fatal to the session.
                tracing::warn!(%err, "a server's action was refused");
            }
        }
        self.warn_about_shared_defaults();
    }

    /// Warns about two actions whose default key is the same.
    ///
    /// # Why this is worth a line on the player's screen
    ///
    /// **A mod cannot ask what is already bound**, and it should not be able
    /// to: the engine owns bindings and mods never read keys (charter rule 11).
    /// So a mod suggesting a key the engine already uses is a mistake nobody is
    /// positioned to catch — and the failure is SILENT, because
    /// [`crate::input::Bindings::action_for`] takes the FIRST action whose
    /// binding matches and the other one simply never fires.
    ///
    /// The player is the one who can fix it, on the controls screen, so the
    /// player is who is told — and told which mod, for the same reason a
    /// mod-attributed warning exists at all.
    ///
    /// Only DEFAULTS: a player who has deliberately put two actions on one key
    /// has said what they meant.
    fn warn_about_shared_defaults(&mut self) {
        for (first, second) in shared_defaults(&self.actions) {
            self.warn(format!(
                "`{second}` and `{first}` are both on the same key by default; only one of them \
                 will work until you rebind it"
            ));
        }
    }

    /// Every sound the server's mods registered.
    #[must_use]
    pub fn sounds(&self) -> &[tiamot_core::proto::SoundDef] {
        &self.sounds
    }

    /// Opens or replaces a dialog.
    ///
    /// Its own method because `pump_network` sits at clippy's line limit — the
    /// same reason `adopt_materials` and `adopt_actions` are separate.
    ///
    /// A whole tree replaces the old one rather than patching it: a dialog is
    /// small, and a patch stream that dropped a message would leave a player
    /// looking at something the server does not believe is there.
    fn adopt_dialog(&mut self, form: String, screen: Option<crate::dialog::Screen>) {
        match screen {
            Some(screen) => {
                self.dialogs.insert(form, screen);
            }
            None => {
                self.dialogs.remove(&form);
            }
        }
    }

    /// Sends every queued dialog event to the server.
    ///
    /// Called once a frame, after the dialogs have been drawn. Only for forms
    /// the server actually has open: a client that raised an event for a dialog
    /// closed in the meantime is describing something that no longer exists,
    /// and the server would refuse it anyway.
    pub fn flush_dialog_events(&mut self) {
        for raised in std::mem::take(&mut self.dialog_events) {
            let closing = matches!(raised.event, tiamot_core::proto::DialogEvent::Closed);
            if !closing && !self.dialogs.contains_key(&raised.form) {
                continue;
            }
            self.connection.send(crate::net::Command::Dialog {
                form: raised.form,
                event: raised.event,
            });
        }
    }

    /// Closes the topmost dialog, if one is open, reporting whether it did.
    ///
    /// **Escape closes what is in front of you before it opens what is not.**
    /// A dialog is a screen the player asked for, and the key that gets out of
    /// every other screen used to walk straight past it and open the pause menu
    /// on top — so the inventory needed two Escapes and the first one paused
    /// the game. Reported from the window.
    ///
    /// Topmost is the LAST of the map, which is the one drawn last and so the
    /// one on top of the pile. The event goes to the server rather than closing
    /// it here, because the dialog is the mod's: the mod hears `Closed`, drops
    /// whatever it was tracking, and the close comes back as
    /// [`Event::DialogClosed`] — the same path the dialog's own Close button
    /// takes. A client that shut its own copy would leave a mod believing a
    /// screen it can still write to is on somebody's display.
    pub fn close_top_dialog(&mut self) -> bool {
        let Some(form) = self.dialogs.keys().next_back().cloned() else {
            return false;
        };
        self.raise_dialog_events(vec![crate::dialog::Raised {
            form,
            event: tiamot_core::proto::DialogEvent::Closed,
        }]);
        true
    }

    /// Takes the dialog events raised since the last call, to send them.
    ///
    /// Drained rather than read: each one is a request the server acts on once,
    /// and a frame that read the list twice would ask twice.
    pub fn take_dialog_events(&mut self) -> Vec<crate::dialog::Raised> {
        std::mem::take(&mut self.dialog_events)
    }

    /// Records what a player did to a dialog.
    ///
    /// Bounded by [`MAX_QUEUED_DIALOG_EVENTS`]: a client that cannot reach its
    /// server must not grow this without limit while it tries.
    pub fn raise_dialog_events(&mut self, events: Vec<crate::dialog::Raised>) {
        for event in events {
            if self.dialog_events.len() >= MAX_QUEUED_DIALOG_EVENTS {
                return;
            }
            self.dialog_events.push(event);
        }
    }

    /// Records what a view holds.
    ///
    /// Its own method because `pump_network` sits at clippy's line limit — the
    /// same reason `adopt_materials`, `adopt_actions` and `adopt_dialog` are.
    fn adopt_view(
        &mut self,
        view: String,
        slots: Vec<Option<tiamot_core::proto::StackDef>>,
        held: Option<tiamot_core::proto::StackDef>,
    ) {
        let hotbar = view == tiamot_core::inventory::PLAYER_MAIN;
        self.views
            .insert(view, crate::dialog::ViewContents { slots, held });
        if hotbar {
            self.adopt_hotbar();
        }
    }

    /// Starts a swing, if one is not already running.
    ///
    /// **Not restarted mid-swing.** Digging re-aims every frame the button is
    /// held, so a swing that restarted on every one of those would be a hand
    /// vibrating rather than swinging.
    fn swing(&mut self) {
        let now = self.since_start.elapsed();
        let running = self
            .swung_at
            .is_some_and(|at| now.saturating_sub(at) < SWING_TIME);
        if !running {
            self.swung_at = Some(now);
        }
    }

    /// How far through a swing the hands are, `0.0..=1.0`.
    fn swing_phase(&self) -> f32 {
        let Some(at) = self.swung_at else {
            return 0.0;
        };
        let elapsed = self.since_start.elapsed().saturating_sub(at);
        (elapsed.as_secs_f32() / SWING_TIME.as_secs_f32()).clamp(0.0, 1.0)
    }

    /// Hands the renderer this frame's hands.
    ///
    /// **What is in them comes from the inventory and the atlas**, which is why
    /// this is here rather than in the renderer: the renderer knows what a hand
    /// looks like and nothing about what a player owns.
    fn place_hands(&mut self) {
        // Third person shows the body instead; a hand hanging in the corner of
        // a view the player is not looking out of would be nobody's.
        let pieces = if self.third_person {
            Vec::new()
        } else {
            let swing = self.swing_phase();
            // The tile AND the cut: what the hand holds is a stack, and two
            // stacks of one stone differ only by their shape.
            let held = |stack: Option<tiamot_core::proto::StackDef>| {
                let Some(stack) = stack else {
                    return crate::render::viewmodel::Held {
                        tile: None,
                        shape: 0,
                        item: false,
                        swing,
                    };
                };
                let (u0, v0, u1, v1) = self
                    .tiles
                    .uv_of(stack.material)
                    .unwrap_or((0.0, 0.0, 1.0, 1.0));
                crate::render::viewmodel::Held {
                    tile: Some([u0, v0, u1, v1]),
                    shape: stack.shape,
                    // The same set the slots and the props read, so one sword
                    // is one shape in every view (`f7f20e1` missed this one).
                    item: self.items.contains(&stack.material),
                    swing,
                }
            };
            let main = crate::render::viewmodel::pieces(
                crate::render::viewmodel::Hand::Main,
                held(self.hotbar.get(self.selected).copied().flatten()),
            );
            let off = crate::render::viewmodel::pieces(
                crate::render::viewmodel::Hand::Off,
                held(self.offhand()),
            );
            [main, off].concat()
        };
        self.renderer.set_hands(pieces);
    }

    /// Puts what the player is carrying into the hands of their own figure.
    ///
    /// **First person has a viewmodel and third person has a body**, and until
    /// now only one of the two held anything: turning around showed a figure
    /// with empty hands holding the block it had just placed. Reported from the
    /// window.
    ///
    /// # What this does not do yet
    ///
    /// **Only the local player's.** Nothing on the wire says what somebody ELSE
    /// is holding, which the entity stream carries as of protocol v32.
    ///
    /// The LOCAL player is still drawn from its own inventory rather than from
    /// the stream: its body is not in the entity list it receives (the server
    /// excludes a viewer from their own view), and its hand should follow a
    /// slot change at once rather than after a round trip.
    fn place_props(&mut self, figure: Option<&crate::render::skinned::Figure>) {
        let mut props = self.dropped_props();
        if let Some(figure) = figure.filter(|_| self.third_person) {
            let held = [
                self.hotbar.get(self.selected).copied().flatten(),
                self.offhand(),
            ];
            props.extend(self.hand_props(figure, held));
        }
        props.extend(self.other_hand_props());
        self.renderer.set_props(&props);
    }

    /// What everybody else is holding.
    ///
    /// **Reported from the window**: every other figure had empty hands. The
    /// client drew what the local player held from its own inventory and had
    /// nothing at all to draw anyone else's from, because nothing on the wire
    /// said. `EntityDef::hands` and `ServerMessage::EntityArmed` are what say.
    fn other_hand_props(&self) -> Vec<crate::render::Prop> {
        let now = self.since_start.elapsed();
        let cells = f64::from(tiamot_core::SUBNODES_PER_AXIS);
        let mut props = Vec::new();
        for (id, entity) in self.entities.iter() {
            if entity.hands.iter().all(Option::is_none)
                || entity.model.as_deref() != Some(tiamot_core::ent::HUMANOID_MODEL)
            {
                continue;
            }
            let Some(pose) = entity.pose(now) else {
                continue;
            };
            let corner = tiamot_core::BlockPos::from_chunk_corner(pose.chunk);
            let feet = [
                f64::from(corner.x) + f64::from(pose.local[0]) / cells,
                f64::from(corner.y) + f64::from(pose.local[1]) / cells,
                f64::from(corner.z) + f64::from(pose.local[2]) / cells,
            ];
            // **Rebuilt exactly as `place_entities` builds it**, because a hand
            // hangs off a joint of the drawn rig and a figure posed even
            // slightly differently would put the sword beside the arm rather
            // than in it.
            let figure = crate::render::skinned::Figure {
                offset: self.camera.position.offset_to(feet),
                yaw: pose.yaw,
                anim: pose.anim,
                phase: now.as_secs_f32() + (id % 977) as f32 * 0.037,
                carrying: [entity.hands[0].is_some(), entity.hands[1].is_some()],
            };
            props.extend(self.hand_props(&figure, entity.hands));
        }
        props
    }

    /// The boxes one figure's hands hold.
    fn hand_props(
        &self,
        figure: &crate::render::skinned::Figure,
        held: [Option<tiamot_core::proto::StackDef>; 2],
    ) -> Vec<crate::render::Prop> {
        let mut props = Vec::new();
        for (joint, stack) in [("hand.r", held[0]), ("hand.l", held[1])] {
            let Some(stack) = stack else { continue };
            let Some(joint) = self.renderer.attachment(figure, joint) else {
                continue;
            };
            props.extend(crate::render::held_boxes(
                figure,
                &joint,
                stack.shape,
                self.tile_of(stack.material),
                self.items.contains(&stack.material),
            ));
        }
        props
    }

    /// Where a material is in the atlas, or the whole sheet if it is not there.
    fn tile_of(&self, material: u16) -> [f32; 4] {
        let (u0, v0, u1, v1) = self.tiles.uv_of(material).unwrap_or((0.0, 0.0, 1.0, 1.0));
        [u0, v0, u1, v1]
    }

    /// Every item lying in view, as boxes.
    ///
    /// **An entity that IS a stack**, which is what a dropped item is. The
    /// engine has no opinion about dropping — what an item is worth, how long
    /// it lasts and who may pick it up are a mod's (charter rule 1) — but a mod
    /// cannot draw anything, so an entity says what stack it represents and
    /// this is where that becomes a picture.
    fn dropped_props(&self) -> Vec<crate::render::Prop> {
        let now = self.since_start.elapsed();
        let cells = f64::from(tiamot_core::SUBNODES_PER_AXIS);
        let mut props = Vec::new();
        for (id, entity) in self.entities.iter() {
            let Some(stack) = entity.item else { continue };
            let Some(pose) = entity.pose(now) else {
                continue;
            };
            let corner = tiamot_core::BlockPos::from_chunk_corner(pose.chunk);
            let at = self.camera.position.offset_to([
                f64::from(corner.x) + f64::from(pose.local[0]) / cells,
                f64::from(corner.y) + f64::from(pose.local[1]) / cells,
                f64::from(corner.z) + f64::from(pose.local[2]) / cells,
            ]);
            props.extend(crate::render::dropped_boxes(
                at,
                crate::render::spin(now.as_secs_f32(), id),
                stack.shape,
                self.tile_of(stack.material),
                self.items.contains(&stack.material),
            ));
        }
        props
    }

    /// Rebuilds the hotbar from the player's own slots.
    ///
    /// **The first nine slots of `player:main`, holes and all.** The hotbar
    /// used to be the CONSOLIDATED inventory — one entry per material, in id
    /// order — so a player who dug a second thing watched the row rearrange
    /// itself under their hands, and the key that had been placing stone was
    /// suddenly placing dirt. A hotbar is a place. These are the same slots the
    /// inventory screen's top row shows, because they are the same slots.
    fn adopt_hotbar(&mut self) {
        let slots = self
            .views
            .get(tiamot_core::inventory::PLAYER_MAIN)
            .map(|contents| contents.slots.as_slice())
            .unwrap_or_default();
        self.hotbar = (0..HOTBAR_SLOTS)
            .map(|index| slots.get(index).copied().flatten())
            .collect();
    }

    /// Records a chat line, keeping the most recent [`MAX_CHAT_LINES`].
    ///
    /// A bounded deque rather than a growing list: a session lasting hours on a
    /// busy server would otherwise hold every line anybody said.
    fn say(&mut self, text: String) {
        tracing::info!("{text}");
        self.chat.push_back(text);
        while self.chat.len() > MAX_CHAT_LINES {
            self.chat.pop_front();
        }
    }

    /// The chat lines to show, oldest first.
    pub fn chat(&self) -> impl Iterator<Item = &str> {
        self.chat.iter().map(String::as_str)
    }

    /// Whether the chat input is open.
    #[must_use]
    pub const fn chat_open(&self) -> bool {
        self.chat_open
    }

    /// The line being typed, for the window to render and edit.
    pub const fn chat_draft_mut(&mut self) -> &mut String {
        &mut self.chat_draft
    }

    /// Opens or closes the chat input, discarding a half-typed line on close.
    pub fn set_chat_open(&mut self, open: bool) {
        if open && !self.chat_open {
            // Only on the transition. Re-opening an already-open box would
            // otherwise steal focus back every time the key repeated.
            self.chat_focus = true;
        }
        self.chat_open = open;
        if !open {
            self.chat_draft.clear();
            self.chat_focus = false;
        }
    }

    /// Whether the input line should take keyboard focus this frame.
    ///
    /// True exactly once per opening. See [`App::chat_focus`] for what asking
    /// every frame did.
    pub const fn take_chat_focus(&mut self) -> bool {
        std::mem::replace(&mut self.chat_focus, false)
    }

    /// Sends what is typed, if anything, and closes the input.
    ///
    /// Trimmed, and an empty line sends nothing: pressing Enter twice is how a
    /// player closes the box, not how they say nothing loudly.
    pub fn send_chat(&mut self) {
        let text = self.chat_draft.trim().to_owned();
        self.chat_open = false;
        self.chat_draft.clear();
        if text.is_empty() {
            return;
        }
        // Bounded before it goes out, because the protocol bounds it on arrival
        // and a silently truncated message is worse than one refused here.
        if text.len() > tiamot_core::proto::MAX_CHAT_BYTES {
            self.warn(format!(
                "that message is {} bytes, over the {} the protocol allows",
                text.len(),
                tiamot_core::proto::MAX_CHAT_BYTES
            ));
            return;
        }
        self.connection.send(crate::net::Command::Chat(text));
    }

    /// What each inventory view holds, as the server last said.
    #[must_use]
    pub const fn views(&self) -> &std::collections::BTreeMap<String, crate::dialog::ViewContents> {
        &self.views
    }

    /// The dialogs a server has open on this screen, in a stable order.
    #[must_use]
    pub const fn dialogs(&self) -> &std::collections::BTreeMap<String, crate::dialog::Screen> {
        &self.dialogs
    }

    /// Takes the sounds heard since the last call.
    ///
    /// Drained rather than read, because each one is played exactly once and a
    /// frame that read the list twice would play everything twice.
    pub fn take_heard(&mut self) -> Vec<crate::net::Event> {
        std::mem::take(&mut self.heard)
    }

    /// Plays everything the server has said is within earshot.
    ///
    /// **Called once a frame, from the frame loop**, because a sound's place is
    /// relative to where the camera is NOW: a queue drained on the network task
    /// would spatialise every sound against wherever the player happened to be
    /// when the packet arrived.
    ///
    /// A sound whose file has not finished decoding is DROPPED rather than
    /// deferred. It has already happened — playing a block break two seconds
    /// late is worse than not playing it, and the file is ready for the next
    /// one.
    pub fn play_heard(&mut self) {
        if self.heard.is_empty() {
            return;
        }
        let (x, y, z) = self.camera.position.to_world();
        let listener = [x, y, z];
        let forward = self.camera.forward();
        let right = self.camera.right();
        let forward = [forward.x, forward.y, forward.z];
        let right = [right.x, right.y, right.z];

        for event in std::mem::take(&mut self.heard) {
            // Loops first, because they are the same spatialisation with a
            // different lifetime and share every number below.
            match event {
                crate::net::Event::StopLoop { id } => {
                    self.mixer.stop_loop(&id);
                    continue;
                }
                crate::net::Event::StartLoop {
                    id,
                    sound,
                    pos,
                    radius,
                    gain,
                    everywhere,
                } => {
                    // **Ambience is placed at the listener, not in the world.**
                    // A loop with no position is not somewhere the player can
                    // walk away from, so it takes full gain, no pan and no
                    // distance filtering — which is the difference between
                    // "night" and "a cricket over there".
                    let placement = if everywhere {
                        crate::audio::Placement {
                            gain,
                            pan: 0.0,
                            brightness: 1.0,
                        }
                    } else {
                        crate::audio::place(pos, listener, forward, right, radius, gain)
                    };
                    let bus = if everywhere {
                        crate::audio::Bus::Ambient
                    } else {
                        crate::audio::Bus::Effects
                    };
                    self.mixer.start_loop(&id, &sound, bus, placement);
                    continue;
                }
                _ => {}
            }

            let crate::net::Event::PlaySound {
                sound,
                pos,
                radius,
                gain,
                entity,
            } = event
            else {
                continue;
            };
            // A sound attached to an entity comes from wherever that entity is
            // being DRAWN, which is the interpolated position and is a thing
            // only the client has. Falling back to the position the server
            // sent when the entity is unknown — it despawned, or has not
            // arrived yet — because a sound in roughly the right place beats
            // silence.
            let at = entity
                .and_then(|id| self.entities.get(id))
                .and_then(crate::entities::Entity::latest)
                .map_or(pos, |pose| {
                    tiamot_core::ent::Transform::at(pose.chunk, pose.local).to_world()
                });
            let placement = crate::audio::place(at, listener, forward, right, radius, gain);
            // Everything a mod plays is an effect for now. Ambient, music and
            // UI are buses a mod cannot yet name — `register_sound` gains a
            // `bus` field when there is a mod that wants one, rather than
            // before.
            self.mixer
                .play(&sound, crate::audio::Bus::Effects, placement);
        }
    }

    /// Adopts the consolidated inventory the server sent.
    ///
    /// The selection is clamped rather than trusted: the list shrinks when a
    /// stack is spent, and a selection left pointing past the end would build
    /// with whatever slid into that position — the sort of bug a player reports
    /// as "it placed the wrong thing" and nobody can reproduce.
    fn adopt_inventory(&mut self, stacks: Vec<tiamot_core::proto::StackDef>) {
        self.carried = stacks;
    }

    /// Folds the cue table the server sent.
    ///
    /// **Load order decides a conflict**, so inserting in order leaves the last
    /// binding for each cue holding it — the same rule the rest of the mod
    /// system resolves a clash by.
    fn adopt_bindings(&mut self, bindings: Vec<tiamot_core::proto::SoundBinding>) {
        self.cues = bindings
            .into_iter()
            .map(|binding| (binding.cue, binding.sound))
            .collect();
    }

    /// Plays whatever a mod bound to a cue, at the listener.
    ///
    /// **This is the client half of the cue system.** Most cues are resolved by
    /// the server and arrive as an ordinary `PlaySound`; these are the ones the
    /// engine emits about the player themselves, and they must not wait for a
    /// round trip — a sound of your own action arriving 80 ms late does not
    /// read as latency, it reads as a worse sound.
    ///
    /// Placed at the listener with no pan: it is happening to YOU, so there is
    /// no direction for it to come from.
    ///
    /// Returns the sound that was played, if any. A cue nobody bound is silence
    /// and not a fault — that is the whole point of separating registering a
    /// sound from saying when it plays.
    pub fn play_cue(&mut self, cue: &str, bus: crate::audio::Bus) -> Option<String> {
        let sound = self.cues.get(cue)?.clone();
        self.mixer.play(
            &sound,
            bus,
            crate::audio::Placement {
                gain: 1.0,
                pan: 0.0,
                brightness: 1.0,
            },
        );
        Some(sound)
    }

    /// Raises `engine:jump` and `engine:land` from the player's own body.
    ///
    /// **Watched here rather than reported by the window**, because the window
    /// knows a key went down and not whether the body left the ground: a jump
    /// pressed against a ceiling makes no noise, and a fall off a ledge lands
    /// without anybody pressing anything.
    ///
    /// Called once a frame beside [`App::play_footsteps`], which is the same
    /// shape and the same reasoning.
    pub fn play_movement_cues(&mut self) {
        let Some(body) = self.predictor.as_ref().map(super::predict::Predictor::body) else {
            return;
        };
        let on_ground = body.on_ground;
        let was = std::mem::replace(&mut self.was_on_ground, on_ground);
        if was == on_ground {
            return;
        }
        let cue = if on_ground {
            "engine:land"
        } else {
            "engine:jump"
        };
        self.play_cue(cue, crate::audio::Bus::Effects);
    }

    /// Plays the player's own footsteps, chosen by what is underfoot.
    ///
    /// **Client-side, from its own movement, with no round trip.** A player's
    /// own footsteps are the one sound in the game whose lateness they would
    /// notice, and asking the server would put a round trip between the foot
    /// and the noise. Everybody else's steps come from their entity, like every
    /// other sound.
    ///
    /// Paced by DISTANCE rather than by time, so walking and sprinting sound
    /// like walking and sprinting without the interval being tuned twice. A
    /// player standing still and turning makes no noise, which a timer would
    /// get wrong.
    #[must_use]
    pub fn play_footsteps(&mut self) -> Option<String> {
        /// Blocks between footfalls. Roughly a stride.
        const STRIDE: f32 = 2.2;

        let body = self
            .predictor
            .as_ref()
            .map(super::predict::Predictor::body)?;
        if !body.on_ground {
            // In the air. The distance travelled while falling does not count,
            // or landing after a long jump would fire several steps at once.
            self.stride = 0.0;
            return None;
        }

        let moved = {
            let last = self.last_step_at;
            let now = body.position;
            self.last_step_at = now;
            let offset = [now[0] - last[0], now[2] - last[2]];
            // Horizontal only: a lift going up is not a walk.
            (offset[0] * offset[0] + offset[1] * offset[1]).sqrt()
        };
        // A teleport is not a walk either. Anything absurd resets the count
        // rather than firing a burst of steps.
        if moved > STRIDE {
            self.stride = 0.0;
            return None;
        }
        self.stride += moved;
        if self.stride < STRIDE {
            return None;
        }
        self.stride = 0.0;

        // What is under the foot, a little below it: standing ON a block means
        // the player's feet are at its top face, so sampling at the feet finds
        // the air they are standing in.
        let material = self.material_under_feet()?;
        // A material no mod gave a voice is silent, which is every material
        // until somebody says otherwise (charter rule 1).
        let sound = self.step_sounds.get(&material).cloned()?;

        // At the listener, so it is centred and unattenuated: these are the
        // player's own feet, and panning them would be strange.
        let placement = crate::audio::Placement {
            gain: 1.0,
            pan: 0.0,
            brightness: 1.0,
        };
        self.mixer
            .play(&sound, crate::audio::Bus::Effects, placement);
        Some(sound)
    }

    /// The material the player is standing on, if the chunk is loaded.
    pub fn material_under_feet(&self) -> Option<u16> {
        let predictor = self.predictor.as_ref()?;
        let body = predictor.body();
        // **The predictor's origin, because that is the frame `body.position`
        // is in.** `Predictor::origin` is defined as the chunk those local
        // coordinates are relative to, and `settle` keeps them inside it.
        //
        // This read `self.camera.position.chunk` before. That is a presentation
        // value — it follows the eye, and `follow_body` builds it in the frame
        // the world is DRAWN in rather than the one the store is keyed by. It
        // was measured to agree here in every case tried, teleport included, so
        // this is not a bug being fixed; it is an incidental agreement being
        // replaced by the frame the value actually belongs to.
        let origin = predictor.origin();
        // A quarter of a block below the feet: inside the block being stood on
        // rather than in the air above it.
        let world = tiamot_core::ent::Transform::at(origin, body.position).to_world();
        let block = tiamot_core::BlockPos::new(
            tiamot_core::detgen::floor_to_i32(world[0] as f32),
            tiamot_core::detgen::floor_to_i32(world[1] as f32 - 0.25),
            tiamot_core::detgen::floor_to_i32(world[2] as f32),
        );
        let chunk = self.store.get(block.chunk())?;
        // The CELL under the foot, not the block: a chiselled block is mostly
        // air, and somebody standing on the one cell left of it is standing on
        // that cell's material rather than on the block's nominal one.
        //
        // `rem_euclid` rather than a cast, because the fraction of a NEGATIVE
        // coordinate is negative and would index cell −2.
        let cell_of = |value: f64| ((value.rem_euclid(1.0) * 3.0) as u32).min(2);
        let view = chunk.get_block_local(block.local());
        // The TOP layer of the block below, which is the surface being walked
        // on — the two beneath it are inside the ground.
        let cell = view.subnode_at(cell_of(world[0]), 2, cell_of(world[2]));
        (!cell.is_air()).then(|| cell.get())
    }

    /// The audio backend, to ask what it holds.
    ///
    /// Read-only, and the seam a test uses to ask the only question that
    /// matters about a sound: did it reach the thing that plays it. The tables
    /// can be perfect while the mixer is empty, which is exactly how two
    /// delivery bugs stayed invisible.
    #[must_use]
    pub const fn mixer(&self) -> &crate::audio::Mixer {
        &self.mixer
    }

    /// The audio backend, for the settings screen's volume sliders.
    pub fn mixer_mut(&mut self) -> &mut crate::audio::Mixer {
        &mut self.mixer
    }

    /// The config this client started with.
    ///
    /// Handed out so the window can write a changed setting back without the
    /// `App` learning where the file is.
    #[must_use]
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Whether there is a sound device at all.
    ///
    /// Shown on the settings screen, because "the volume is up and I hear
    /// nothing" and "this machine has no audio" look identical otherwise.
    #[must_use]
    pub fn audio_available(&self) -> bool {
        self.mixer.available()
    }

    /// Notes that the volumes changed and should be written out.
    pub fn mark_volumes_dirty(&mut self) {
        self.volumes_dirty = true;
    }

    /// Whether the volumes need saving, clearing the flag.
    pub fn take_volumes_dirty(&mut self) -> bool {
        std::mem::take(&mut self.volumes_dirty)
    }

    /// Whether the settings screen is showing.
    #[must_use]
    pub fn settings_open(&self) -> bool {
        self.settings_open
    }

    /// Says whether the world is stopped, so the client stops with it.
    ///
    /// **Set by whoever paused the server**, which is only ever the window of a
    /// client that owns one. See [`App::walk`] for why running on through a
    /// pause is not merely wasted work.
    pub const fn set_world_paused(&mut self, paused: bool) {
        self.world_paused = paused;
    }

    /// Whether the pause menu is on the screen.
    #[must_use]
    pub const fn menu_open(&self) -> bool {
        self.menu_open
    }

    /// Asks the window to close the game.
    ///
    /// A flag rather than an exit call: quitting means saving the world and
    /// leaving the server cleanly, and the window owns both. Raised here
    /// because the button is here.
    pub const fn request_quit(&mut self) {
        self.quit_requested = true;
    }

    /// Packs the server's textures and hands them to everything that draws.
    ///
    /// The renderer gets the pixels, the interface gets the layout, and the
    /// packed image is dropped at the end of this call — one copy of an atlas
    /// that is several megabytes for a large mod set.
    fn adopt_atlas(
        &mut self,
        table: &[tiamot_core::proto::MaterialDef],
        images: &BTreeMap<u16, Image>,
    ) {
        self.adopt_materials(table);
        let atlas = build_atlas(table, images);
        self.tiles = atlas.tiles_only();
        self.atlas_changed = true;
        self.renderer.set_atlas(&atlas);
        // Every mesh drawn before this sampled the placeholder atlas. In
        // practice the table arrives before any chunk, but "in practice" is not
        // a guarantee the renderer should rely on.
        self.store.mark_all_dirty();
    }

    /// Where each material sits in the atlas.
    #[must_use]
    pub const fn tiles(&self) -> &crate::texture::TileMap {
        &self.tiles
    }

    /// Materials that may not be placed: the items.
    ///
    /// Read by whatever draws a slot, because an item is drawn flat and a block
    /// is drawn as a cube — see [`crate::icons::Icons::paint_stack`].
    #[must_use]
    pub const fn items(&self) -> &std::collections::BTreeSet<u16> {
        &self.items
    }

    /// Whether the atlas texture is new and needs registering with egui.
    ///
    /// True exactly once per atlas. The material table arrives mid-session,
    /// after the window and its egui renderer already exist, so the bridge
    /// cannot be built at startup — and re-registering every frame would leak
    /// a texture per frame.
    pub const fn take_atlas_change(&mut self) -> bool {
        std::mem::replace(&mut self.atlas_changed, false)
    }

    /// Takes the quit request, if one was made.
    pub const fn take_quit_request(&mut self) -> bool {
        std::mem::replace(&mut self.quit_requested, false)
    }

    /// Opens or closes the pause menu.
    ///
    /// Closing it closes the controls screen with it: the controls are a page
    /// OF the menu, and leaving them up over the world after the menu had gone
    /// would be a screen with no way back.
    pub fn set_menu_open(&mut self, open: bool) {
        self.menu_open = open;
        if !open {
            self.settings_open = false;
            self.rebinding = None;
        }
    }

    /// How large the interface is drawn.
    #[must_use]
    pub const fn ui_scale(&self) -> f32 {
        self.config.ui_scale
    }

    /// The scale the slider should be showing: the draft if one is being
    /// dragged, and what is in force otherwise.
    #[must_use]
    pub fn shown_ui_scale(&self) -> f32 {
        self.ui_scale_draft.unwrap_or(self.config.ui_scale)
    }

    /// The half-dragged scale, for the slider to update in place.
    pub const fn ui_scale_draft(&mut self) -> &mut Option<f32> {
        &mut self.ui_scale_draft
    }

    /// Sets the interface scale, clamped to what a player can recover from.
    pub fn set_ui_scale(&mut self, scale: f32) {
        let scale = if scale.is_finite() {
            scale.clamp(
                *crate::config::UI_SCALE_RANGE.start(),
                *crate::config::UI_SCALE_RANGE.end(),
            )
        } else {
            return;
        };
        if (self.config.ui_scale - scale).abs() < f32::EPSILON {
            return;
        }
        self.config.ui_scale = scale;
        self.volumes_dirty = true;
    }

    /// Whether the HUD is drawn.
    #[must_use]
    pub const fn hud_visible(&self) -> bool {
        self.config.hud_visible
    }

    /// Shows or hides the HUD, and remembers the choice.
    pub fn set_hud_visible(&mut self, visible: bool) {
        if self.config.hud_visible == visible {
            return;
        }
        self.config.hud_visible = visible;
        self.volumes_dirty = true;
    }

    /// Whether the debug overlay is being drawn.
    ///
    /// Charter rule 18's instrument, and it ships. See
    /// [`crate::config::Config::debug_overlay`] for why it is not a
    /// developer-only build.
    #[must_use]
    pub const fn debug_overlay(&self) -> bool {
        self.config.debug_overlay
    }

    /// Turns the debug overlay on or off, and remembers the choice.
    pub fn set_debug_overlay(&mut self, on: bool) {
        if self.config.debug_overlay == on {
            return;
        }
        self.config.debug_overlay = on;
        // The same flag the volume sliders use: the `App` says the settings
        // changed and the window, which is what knows the path, writes them.
        self.volumes_dirty = true;
    }

    /// Opens the controls screen, from the menu.
    pub const fn open_settings(&mut self) {
        self.settings_open = true;
    }

    /// Opens or closes the settings screen.
    ///
    /// Closing abandons a capture in progress: a player who opened the rebind
    /// prompt and changed their mind should not have the next key they press
    /// swallowed by a screen that is no longer there.
    pub fn toggle_settings(&mut self) {
        self.settings_open = !self.settings_open;
        if !self.settings_open {
            self.rebinding = None;
        }
    }

    /// The action currently waiting for a key, if any.
    #[must_use]
    pub fn rebinding(&self) -> Option<&str> {
        self.rebinding.as_deref()
    }

    /// Starts waiting for a key to bind to this action.
    pub fn begin_rebind(&mut self, id: &str) {
        self.rebinding = Some(id.to_owned());
    }

    /// Abandons a capture without binding anything.
    pub fn cancel_rebind(&mut self) {
        self.rebinding = None;
    }

    /// Offers a physical input to a capture in progress.
    ///
    /// Returns whether it was taken. The window asks this FIRST and acts on the
    /// input only if the answer is no — otherwise rebinding a key would also
    /// fire whatever that key currently does, which is at best a jump and at
    /// worst the thing being rebound away from.
    pub fn capture(&mut self, input: crate::input::Input) -> bool {
        let Some(id) = self.rebinding.take() else {
            return false;
        };
        self.bindings.bind(&id, input);
        self.bindings_dirty = true;
        true
    }

    /// Returns one action to its default.
    pub fn reset_binding(&mut self, id: &str) {
        self.bindings.reset(id);
        self.bindings_dirty = true;
    }

    /// Returns every action to its default.
    pub fn reset_all_bindings(&mut self) {
        self.bindings.reset_all();
        self.bindings_dirty = true;
    }

    /// Whether the bindings need writing out, clearing the flag.
    pub fn take_bindings_dirty(&mut self) -> bool {
        std::mem::take(&mut self.bindings_dirty)
    }

    /// Which action an input is bound to, by id.
    ///
    /// Owned rather than borrowed because the caller acts on the answer, and
    /// acting means `&mut self` — a borrow of the registry cannot still be
    /// alive at that point. One small allocation per key press.
    #[must_use]
    pub fn action_for(&self, input: crate::input::Input) -> Option<String> {
        self.bindings
            .action_for(&self.actions, input)
            .map(|action| action.id.clone())
    }

    /// Every action, for the settings screen.
    #[must_use]
    pub fn actions(&self) -> &crate::input::Actions {
        &self.actions
    }

    /// The bindings, for the settings screen to change.
    pub fn bindings_mut(&mut self) -> &mut crate::input::Bindings {
        &mut self.bindings
    }

    /// The bindings, for the settings screen to show.
    #[must_use]
    pub fn bindings(&self) -> &crate::input::Bindings {
        &self.bindings
    }

    /// Reports a mod-registered action to the server.
    ///
    /// **Engine actions never come through here.** Walking, digging and placing
    /// travel as their own messages, which the server already judges; a client
    /// that could report `engine:jump` as an action would be telling the server
    /// something it is supposed to decide for itself (charter rule 2).
    pub fn send_action(&mut self, id: &str, pressed: bool) {
        let is_mods = self
            .actions
            .get(id)
            .is_some_and(|action| matches!(action.source, crate::input::Source::Mod(_)));
        if !is_mods {
            return;
        }
        self.connection.send(Command::Action {
            id: id.to_owned(),
            pressed,
        });
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

    /// Puts the camera behind the player, or back in their eyes.
    ///
    /// Draws a box the size of the collision body while it is behind them —
    /// there is no player model until Task 12, and a third-person view of an
    /// invisible player would show the world moving around nothing.
    pub const fn toggle_third_person(&mut self) {
        self.third_person = !self.third_person;
    }

    /// Whether the camera is behind the player.
    #[must_use]
    pub const fn is_third_person(&self) -> bool {
        self.third_person
    }

    /// Steps the shadow quality: off, low, medium, high, and round again.
    ///
    /// Turns the chunk-border overlay on or off, and reports the new state.
    ///
    /// A debugging view: it draws the cage every visible chunk occupies, which is
    /// how you tell a seam that follows chunk boundaries from one that merely
    /// happens to be near one. This session spent a long evening on "it almost
    /// feels like chunk boundaries have their own collision" with no way to see
    /// where they were.
    /// Shows or hides the cage every visible chunk occupies.
    ///
    /// How you tell a seam that follows chunk boundaries from one that merely
    /// happens to be near one.
    pub fn toggle_chunk_borders(&mut self) -> bool {
        let show = !self.renderer.chunk_borders();
        self.renderer.set_chunk_borders(show);
        show
    }

    /// Live, like the lighting mode, and for the same reason — the difference
    /// between two settings is a thing you judge by looking at them one after
    /// the other, not by restarting between them.
    pub fn cycle_shadow_quality(&mut self) {
        self.config.shadow_quality = self.config.shadow_quality.next();
        self.renderer.set_shadow_quality(self.config.shadow_quality);
        // **Say so, because in two of the three modes nothing happens.**
        //
        // Shadow maps are mode 3's and only mode 3's — modes 1 and 2 do not
        // allocate them at all, which is deliberate (Task 10) and invisible.
        // Reported from the window as "the shadows seem to have gone missing
        // and pressing K does not make them show up", which is exactly what a
        // key that silently changes a setting for a mode you are not in looks
        // like.
        let mode = self.config.lighting_mode;
        let quality = self.config.shadow_quality.name();
        if mode == crate::config::LightingMode::Beautiful {
            self.warn(format!("shadows: {quality}"));
        } else {
            self.warn(format!(
                "shadows: {quality} — but lighting mode {} draws none. Press L for mode 3.",
                mode.name()
            ));
        }
    }

    /// How sharp mode 3's shadows are.
    #[must_use]
    pub const fn shadow_quality(&self) -> crate::config::ShadowQuality {
        self.config.shadow_quality
    }

    /// Moves the time of day by hand, for looking at the sky and the shadows.
    ///
    /// **A debugging affordance, and the honest kind**: it moves the CLIENT's
    /// clock, so what it shows is what that hour genuinely looks like, and it
    /// says so on the HUD rather than pretending the world moved. The server
    /// keeps its own time and keeps sending it; [`App::resync_time`] gives the
    /// clock back.
    ///
    /// There is no mod-facing version of this and there should not be one yet:
    /// charter rule 11 puts key bindings in the engine and named actions in
    /// mods, and named actions are inert until Task 13. This is a key the
    /// engine owns, for a thing only a developer needs.
    pub fn nudge_time(&mut self, delta: f32) {
        self.time_override = true;
        let time = (self.sky.time() + delta).rem_euclid(1.0);
        self.sky.set_time(time);
    }

    /// Gives the clock back to the server.
    pub const fn resync_time(&mut self) {
        self.time_override = false;
    }

    /// Whether the clock is being scrubbed by hand.
    #[must_use]
    pub const fn time_is_local(&self) -> bool {
        self.time_override
    }

    /// Where the day stands, `0.0..1.0`.
    #[must_use]
    pub fn sky_time(&self) -> f32 {
        self.sky.time()
    }

    /// Switches to the next lighting mode, live.
    ///
    /// Task 10's criterion is that this needs no restart, and it does not: the
    /// mode is a uniform on the renderer's side and an argument to the mesher on
    /// this one. Nothing is reallocated and no pipeline is rebuilt.
    ///
    /// The world *is* remeshed, and has to be. Light is baked into vertices at
    /// mesh time — that is what makes it free to draw — so the geometry a mode
    /// shows was built for that mode. The rebuild goes through the ordinary
    /// dirty queue and `REMESH_TIME_BUDGET`, so a switch spreads over a few
    /// frames near the camera outward instead of stalling one.
    pub fn cycle_lighting_mode(&mut self) {
        self.set_lighting_mode(self.config.lighting_mode.next());
    }

    /// Switches to a particular lighting mode, live.
    pub fn set_lighting_mode(&mut self, mode: crate::config::LightingMode) {
        if self.config.lighting_mode == mode {
            return;
        }
        self.config.lighting_mode = mode;
        self.renderer.set_lighting_mode(mode);
        self.store.mark_all_dirty();
    }

    /// Which lighting mode is showing.
    #[must_use]
    pub const fn lighting_mode(&self) -> crate::config::LightingMode {
        self.config.lighting_mode
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

    /// The material the hotbar is on, if that slot has anything in it.
    #[must_use]
    pub fn selected_material(&self) -> Option<u16> {
        self.hotbar
            .get(self.selected)
            .copied()
            .flatten()
            .map(|stack| stack.material)
    }

    /// Moves the hotbar selection, wrapping.
    ///
    /// **Over the slots, not over what is in them.** Wrapping because the input
    /// is a mouse wheel and a wheel that stops at the end feels broken; over
    /// the fixed nine because scrolling past an empty slot and having the
    /// selection skip it would make the row's positions unlearnable.
    pub fn select_next(&mut self, forward: bool) {
        let count = self.hotbar.len().max(1);
        let slot = if forward {
            (self.selected + 1) % count
        } else {
            (self.selected + count - 1) % count
        };
        self.hold(slot);
    }

    /// Selects a slot directly, as the number keys do.
    pub fn select_slot(&mut self, slot: usize) {
        if slot < self.hotbar.len() {
            self.hold(slot);
        }
    }

    /// Holds a slot, and tells the server which one.
    ///
    /// **Told on the CHANGE, not every tick.** The hotbar is the client's own
    /// UI and nothing about the world moves when a player looks at a different
    /// slot — but a mod cannot act on what somebody is holding unless the
    /// server knows what that is, which is the whole reason an item that is
    /// not a block is worth registering.
    fn hold(&mut self, slot: usize) {
        if self.selected == slot {
            return;
        }
        self.selected = slot;
        self.connection.send(crate::net::Command::SelectSlot {
            slot: u16::try_from(slot).unwrap_or(0),
        });
    }

    /// Asks the server to swap the selected slot with the off-hand.
    ///
    /// **A request, like every other inventory gesture.** The client says which
    /// slot the player was holding; the server does the swap against its own
    /// copy and sends back what the inventory now is. Nothing is moved here,
    /// so a client that lied about it still sees the truth a moment later.
    pub fn swap_offhand(&mut self) {
        let Ok(slot) = u16::try_from(self.selected) else {
            return;
        };
        self.connection.send(Command::SwapOffhand { slot });
    }

    /// What is in the off-hand, if anything.
    #[must_use]
    pub fn offhand(&self) -> Option<tiamot_core::proto::StackDef> {
        self.views
            .get(tiamot_core::inventory::PLAYER_MAIN)
            .and_then(|contents| {
                contents
                    .slots
                    .get(tiamot_core::inventory::PLAYER_OFFHAND_SLOT)
                    .copied()
                    .flatten()
            })
    }

    /// The hotbar's slots, holes included.
    #[must_use]
    pub fn hotbar(&self) -> &[Option<tiamot_core::proto::StackDef>] {
        &self.hotbar
    }

    /// What the player is carrying, as `(material, units)` in id order.
    #[must_use]
    pub fn carried(&self) -> &[tiamot_core::proto::StackDef] {
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

    /// Leaves the server and hands back what the window lent this world.
    ///
    /// **Because leaving a world is not leaving the game.** Quit used to end
    /// the process, which was the only thing it could mean before there was a
    /// front screen to go back to. The renderer owns the device, the pipelines
    /// and the shadow cascades and takes long enough to build that rebuilding
    /// it per world would be a visible stall — so it goes back to the window,
    /// along with the bindings, which are the player's and not this world's.
    #[must_use]
    pub fn leave(self) -> (Renderer, crate::input::Bindings, crate::config::Config) {
        let Self {
            connection,
            renderer,
            bindings,
            config,
            ..
        } = self;
        connection.shutdown();
        // **The config comes back too.** A world holds its own copy so that a
        // setting changed in it takes effect immediately, and the window kept
        // the copy it started with — so a scale or a volume set in game was
        // forgotten the moment the player left, and only reappeared on the next
        // launch once `client.toml` had been read again. Reported from the
        // window as the interface scale "only changing once the game is
        // restarted".
        (renderer, bindings, config)
    }

    /// Which fluid the camera is inside, if any.
    ///
    /// The EYE rather than the body: a swimmer floating with their head out is
    /// not looking through milk, and tinting the frame from the body's
    /// submerged fraction would put them underwater while they can plainly see
    /// the sky. `phys::swim::fluid_at` is the same surface height the physics
    /// floats them at and the mesher draws — a tint arriving a fraction of a
    /// cell early or late is the kind of mismatch nobody can debug from a
    /// screenshot.
    fn submerged_in(&self) -> Option<tiamot_core::fluid::FluidId> {
        let predictor = self.predictor.as_ref()?;
        let voxels = phys::Voxels::with_fluid(&self.store, &self.store, predictor.origin());
        let fluid = phys::swim::fluid_at(&voxels, predictor.body().eye());
        (!fluid.is_none()).then_some(fluid)
    }

    /// Records the radius the server says it is actually streaming at.
    ///
    /// **The granted value replaces what the fog is drawn from, and never the
    /// configured preference.** Overwriting the preference would make a
    /// reconnect ask for what it was last given rather than what the player
    /// chose, ratcheting the world smaller every time they joined a strict
    /// server.
    fn accept_view_distance(&mut self, horizontal: u8, vertical: u8) {
        self.granted_view = tiamot_core::interest::ViewDistance::clamped(horizontal, vertical);
        if horizontal != self.config.view_distance {
            // Worth saying out loud: a player who set 16 and is being sent 8
            // should be able to find out why the world is smaller than they
            // asked for.
            tracing::info!(
                asked = self.config.view_distance,
                granted = horizontal,
                "the server capped the view distance"
            );
        }
    }

    /// Records one entity event, stamped with when it arrived here.
    ///
    /// Its own method so `pump_network` stays inside the line limit, and
    /// because the stamping is the part worth naming: the arrival time is this
    /// machine's clock and never the server's tick. See `crate::entities`.
    fn entity_event(&mut self, event: Event) {
        let at = self.since_start.elapsed();
        match event {
            Event::EntitySpawn(entities) => self.entities.spawned(&entities, at),
            Event::EntityDespawn(ids) => self.entities.despawned(&ids),
            Event::EntityState { tick, entities } => self.entities.moved(tick, &entities, at),
            Event::EntityArmed(entities) => self.entities.rearmed(&entities),
            // Every other event is handled where it arrives; this method exists
            // for the three that share a clock.
            _ => {}
        }
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

                Event::Materials { table, images } => self.adopt_atlas(&table, &images),

                Event::Joined {
                    spawn,
                    tick,
                    may_fly,
                    ..
                } => self.joined_world(spawn, tick, may_fly),

                Event::Chunk(chunk) => self.store.insert(*chunk),

                Event::ChunkLight(pos, layer) => self.store.set_light(pos, *layer),

                event @ (Event::EntitySpawn(_)
                | Event::EntityDespawn(_)
                | Event::EntityArmed(_)
                | Event::EntityState { .. }) => self.entity_event(event),

                Event::ChunkFluid(pos, layer) => self.store.set_fluid(pos, *layer),

                Event::Fluids { fluids } => self.store.set_fluid_table(&fluids),

                // **The GRANTED radius, which is what the fog is drawn from.**
                // Using the configured one instead would end the world in clear
                // air whenever the server gave less than was asked for — and
                // the server's limit is the one that decides, so that is not a
                // rare case but the normal one on any server with a lower cap
                // than this client's config.
                Event::ViewDistance {
                    horizontal,
                    vertical,
                } => self.accept_view_distance(horizontal, vertical),

                Event::Sky(sky) => self.sky = sky,

                // Ignored while the clock is being scrubbed by hand. The server
                // is still the authority and still sending; a local override
                // that the next broadcast undid a second later would be
                // unusable for looking at anything.
                Event::TimeOfDay(time) => {
                    if !self.time_override {
                        self.sky.set_time(time);
                    }
                }

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

                Event::PlayerState(state) => self.accept_player_state(&state),

                Event::DigProgress { target, progress } => {
                    self.dig = Some((target, progress));
                }

                Event::Actions { actions } => self.adopt_actions(actions),

                // Recorded now, played later: the audio backend is the next
                // piece of Task 13, and until it lands a client knows what a
                // server's sounds ARE without being able to make one.
                Event::Sounds { sounds } => self.sounds = sounds,

                Event::HudScript { mod_id, source } => self.adopt_hud_script(&mod_id, &source),

                Event::SoundBindings { bindings } => self.adopt_bindings(bindings),

                Event::View { view, slots, held } => self.adopt_view(view, slots, held),
                Event::Dialog {
                    form,
                    tree,
                    compact,
                } => self.adopt_dialog(form, Some(crate::dialog::Screen::new(*tree, compact))),
                Event::DialogClosed { form } => self.adopt_dialog(form, None),

                // Held for the frame loop, which is the only place a sound can
                // be spatialised against where the camera is NOW. Loops travel
                // with them: same placement, different lifetime.
                Event::PlaySound { .. } | Event::StartLoop { .. } | Event::StopLoop { .. } => {
                    self.heard.push(event);
                }

                // Decoded and ready. Handed straight to the mixer, which holds
                // it even with no sound device — whether an asset decoded is a
                // property of the asset, and the tests ask on machines that
                // have no speakers.
                Event::SoundReady { id, clip, voice } => self.mixer.insert(id, clip, voice),

                Event::Tools { tools } => self.adopt_tools(tools),

                Event::Inventory { stacks } => self.adopt_inventory(stacks),

                Event::Chat { text, .. } => self.say(text),

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
        if due.positions.is_empty() {
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

        for (index, pos) in due.positions.iter().enumerate() {
            let Some(chunk) = self.store.get(*pos) else {
                continue;
            };
            let neighbours = self.store.neighbours(*pos);

            let mesh_started = std::time::Instant::now();
            // **Every mode meshes against the real light now, mode 1 included.**
            //
            // It used to take a flat daylight constant. That was not a shortcut
            // in the shader's sense — the mesher may only merge two faces whose
            // corner light agrees, so real light splits quads along every shadow
            // edge, and a constant put the merge rate and the vertex count back
            // exactly where Task 08 left them.
            //
            // What it also did was leave mode 1 unable to tell a cave from a
            // field, because "am I underground" is a question only the stored
            // sunlight answers. There is no cheaper way to ask it. See
            // `LightingMode::Simple`.
            let fluid = self.store.fluid_for(*pos);
            let light = self.store.light_for(*pos);
            let mesh = if self.config.lighting_mode.uses_propagated_light() {
                mesher::mesh_chunk(chunk, &neighbours, ABSENT_POLICY, &light, &fluid)
            } else {
                mesher::mesh_chunk(chunk, &neighbours, ABSENT_POLICY, &FLAT_DAYLIGHT, &fluid)
            };
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
            // **Never inside the urgent run.** Those are the chunks one
            // frame's edits touched, and a chunk drawn without the neighbour
            // whose face its edit exposed is a hole through the world — the
            // report this ordering exists for. Everything after them is
            // streaming and light, which can wait a frame without leaving a
            // gap because their old meshes are still right.
            if index + 1 >= due.urgent
                && started.elapsed() >= REMESH_TIME_BUDGET
                && index + 1 < due.positions.len()
            {
                self.store.requeue(&due.positions[index + 1..]);
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
    pub fn advance(&mut self, mut input: Input, dt: f32) {
        // **Whether the server allows it, not whether a key is down.** The
        // client predicts what the server will do; predicting flight it is
        // about to refuse would be a correction every tick.
        input.fly = self.flying;
        self.last_dt = dt;
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
        self.pacing.frame(dt, self.last_phases);

        // The sky moves every frame and is corrected by the server once a
        // second. Advancing locally between updates is what makes dawn a fade
        // rather than twenty steps; `set_time` snapping to the server's answer
        // is what stops it drifting.
        self.sky.advance(dt);
        // Milk's own clock, which is not the sky's and not the tick's: it is
        // how far a fluid texture has scrolled, and nothing but the fluid pass
        // reads it. Presentation, so it runs on frame time rather than being
        // pinned to the simulation.
        self.renderer.advance_clock(dt);
        let moment = self.sky.moment();
        self.renderer
            .set_sun(moment.intensity, moment.sun, moment.sun_direction);
        // **Under the milk, the milk IS the sky.**
        //
        // Being submerged is fog: dense, close, and the colour of what you are
        // in. Saying it that way rather than adding a tint pass means it works
        // in all three lighting modes without touching any of them — modes 1
        // and 2 fog in the world shader, mode 3 fogs from depth in the post
        // chain, and both take the same colour and distance. It also gets the
        // background right, which a tint over the frame would not: the sky
        // through the surface is milk, not sky.
        let (sky, far) = match self.submerged_in() {
            Some(fluid) => (self.store.fluid_colour(fluid), UNDERWATER_VISIBILITY),
            None => (
                moment.sky,
                f32::from(self.granted_view.horizontal) * tiamot_core::CHUNK_BLOCKS as f32,
            ),
        };
        self.renderer.set_sky(sky, far);
        self.renderer.set_grade(moment.grade);

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

        // **A paused world advances neither side.** Snapping the tick back when
        // the world restarts (see `resync_plan`) fixes a client that ran ahead,
        // and a correction is still a correction: the body is put right rather
        // than never having been wrong, and a player who started walking the
        // instant they closed the menu felt themselves pulled about for the
        // second and a half it took to smooth out. Reported from the window,
        // after that first fix.
        //
        // So the client does not run at all while it is the one that paused the
        // world. Only ever true for an embedded server — a hosted world has
        // other people in it and does not stop for anybody's menu.
        if self.world_paused {
            // The carry is DROPPED rather than kept. Keeping it would bank the
            // whole length of the menu and spend it on the way out, which is
            // the fast-forward `MAX_CATCH_UP` exists to prevent.
            self.tick_carry = 0.0;
            return;
        }

        self.tick_carry += dt;
        let mut spent = 0;
        while self.tick_carry >= TICK_SECONDS && spent < MAX_CATCH_UP {
            self.tick_carry -= TICK_SECONDS;
            spent += 1;
            self.tick += 1;

            let mut intent = self.intent_from(input);

            // **One hop per press.** Holding the key used to jump again the
            // instant the body touched down, which in a tunnel with a sub-node
            // of headroom is a hop every three ticks — reported from the window
            // as bouncing. Requested as "make it so only one hop per key press
            // is done".
            //
            // Edge-detected HERE, on the tick, rather than on the frame: at 1,200
            // fps a frame-level edge would let one press through on one frame
            // and the tick that consumed it might be sixty frames away. And it
            // is done on the way INTO the tick, so the input the server is sent
            // (`report_input`, below) carries the same single jump the client
            // predicted — anything else would be a disagreement by construction.
            // **One press, sent for a few ticks.**
            //
            // A press is an edge, so it is detected once — but a single tick
            // carrying it is a single packet, and `InputQueue::offer` refuses any
            // input whose tick the server has already passed. Lose that one and
            // the server never jumps while the client already has: the two part
            // company by a whole jump arc, which is the `worst correction 5.37
            // cells` reported at a landing.
            //
            // Repeating it is safe because a jump is only honoured from the
            // ground: the copies land while the body is already airborne and do
            // nothing. That idempotence is what makes redundancy free here, and
            // it is why the window must stay SHORTER than the shortest possible
            // airtime — the 0.6-cell hop under a low ceiling is three ticks, and
            // `a_hop_under_a_ceiling_is_still_one_hop` holds the two apart.
            if input.jump && !self.previous_input.jump {
                self.jump_edge = JUMP_EDGE_TICKS;
            } else if !input.jump {
                self.jump_edge = 0;
            }
            intent.jump = self.jump_edge > 0;
            self.jump_edge = self.jump_edge.saturating_sub(1);
            self.previous_input = input;
            self.previous_intent = intent;

            let mut touched_absent = false;
            let mut ground = None;
            if let Some(predictor) = self.predictor.as_mut() {
                let voxels = phys::Voxels::with_fluid(&self.store, &self.store, predictor.origin());
                predictor.predict(&voxels, self.tick, intent, &Tuning::DEFAULT);
                // **Asked on the PREDICTION path, not only on the replay.** The
                // first version of this instrument watched `reconcile` alone, so
                // "I never saw the marker" ruled out nothing about the ticks a
                // player actually lives through — which are these.
                touched_absent = voxels.touched_absent();
                ground = Some(predictor.body().on_ground);
            }
            if touched_absent {
                self.pacing.predicted_into_the_unloaded();
            }
            if let Some(ground) = ground {
                self.pacing.footing(ground);
            }
            self.report_input(
                Input {
                    jump: intent.jump,
                    ..input
                },
                intent,
            );
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
            self.pacing
                .correction(predictor.error(), predictor.vertical_share());
        }

        // Whatever time did not buy a whole tick is how far through the current
        // one this frame is, and the camera is drawn there rather than at the
        // tick boundary. Without it the camera moves 20 times a second no
        // matter how fast the client draws.
        self.follow_body(self.tick_carry / TICK_SECONDS, dt);
        // After the camera moves, so the outline is where the crosshair points
        // this frame rather than last.
        self.update_selection();
    }

    /// Turns this frame's keys into a world-space intent.
    fn intent_from(&self, input: Input) -> Intent {
        intent_at_yaw(self.camera.yaw, input)
    }

    /// Puts the camera at the predicted body's eyes.
    ///
    /// `dt` drives the step smoothing — see [`crate::predict::Predictor::smooth_step`].
    /// It is done here rather than in `walk` because it is a per-FRAME ease and
    /// `walk` runs per simulation tick: easing at 20 Hz would replace one
    /// staircase with a slower one.
    fn follow_body(&mut self, alpha: f32, dt: f32) {
        if let Some(predictor) = self.predictor.as_mut() {
            predictor.smooth_step(dt);
        }
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
        let eye = [
            f64::from(corner.x) + f64::from(local[0]) / cells,
            f64::from(corner.y) + f64::from(local[1] + phys::EYE_HEIGHT) / cells,
            f64::from(corner.z) + f64::from(local[2]) / cells,
        ];

        if self.third_person {
            // Straight back along the view, which is the simplest thing that
            // works and is deliberately not a real third-person camera: there
            // is no collision on it, so it will happily sit inside a wall. It
            // exists to look at the player from outside, not to play from.
            let back = self.camera.forward();
            self.camera.position = Position::from_world(
                eye[0] - f64::from(back.x) * THIRD_PERSON_DISTANCE,
                eye[1] - f64::from(back.y) * THIRD_PERSON_DISTANCE,
                eye[2] - f64::from(back.z) * THIRD_PERSON_DISTANCE,
            );
        } else {
            self.camera.position = Position::from_world(eye[0], eye[1], eye[2]);
        }

        self.follow_speed(dt);

        // **The body's position is set in both views; only its VISIBILITY
        // changes.**
        //
        // Reported from the window: "my single block character does not have a
        // shadow". It did not, because in first person the body was not handed
        // to the renderer at all — and a body the renderer does not know about
        // cannot be drawn into a shadow cascade either. Seeing your own shadow
        // on the ground beside you is most of what tells you where you are
        // standing, and it is the one shadow a player looks at constantly.
        //
        // So the renderer is always told where the body is, and told separately
        // whether to draw it in the world pass. In first person it casts and is
        // not drawn, which is what every game does and what a player expects
        // without being able to say why.
        //
        let feet = [
            f64::from(corner.x) + f64::from(local[0]) / cells,
            f64::from(corner.y) + f64::from(local[1]) / cells,
            f64::from(corner.z) + f64::from(local[2]) / cells,
        ];
        let at = self.camera.position.offset_to(feet);
        self.renderer.set_body(Some(at));
        self.renderer.set_body_visible(self.third_person);

        // After the camera has settled, because every offset is relative to it.
        self.place_entities();
        // And after the entities, because the player's figure goes on the end
        // of theirs — see `Renderer::set_player`.
        let figure = crate::render::skinned::Figure {
            offset: at,
            // The camera's yaw, not the server's: the body turns with the look,
            // at prediction rate, which is the whole reason the local player is
            // not simply read back off the entity stream — converted, because
            // the two count from different directions.
            yaw: figure_yaw(self.camera.yaw),
            anim: self.gait(),
            phase: self.since_start.elapsed().as_secs_f32(),
            carrying: [
                self.hotbar.get(self.selected).copied().flatten().is_some(),
                self.offhand().is_some(),
            ],
        };
        self.renderer.set_player(Some(figure));
        self.place_blobs();
        self.place_hands();
        // The same figure, not another one built from the same fields: what the
        // hands hold hangs off the clip's phase and the heading, and two copies
        // of those would be two things to keep in step.
        self.place_props(Some(&figure));
    }

    /// Places every entity in view for this frame.
    ///
    /// **Recomputed every frame, from the interpolation buffer.** That is what
    /// it means for an entity to move: the buffer holds a handful of samples
    /// and the pose between them depends on when this frame is, so caching it
    /// would cache the one thing that is supposed to change.
    ///
    /// The camera-relative offset is taken through `Position::offset_to`, which
    /// is the only correct way to subtract two positions in a floating origin
    /// (charter rule 7) — the entity and the camera may be in different chunks,
    /// and subtracting their local parts would compare nothing.
    /// Places a blob shadow under the player and under every drawn entity.
    ///
    /// # Why an engine with real shadows still wants these
    ///
    /// The cascades answer one question — is the SUN blocked — and only in
    /// lighting mode 3. Everywhere else, and for every light that is not the
    /// sun, a body has nothing anchoring it to the ground and reads as
    /// hovering. It reads that way most strongly indoors and at night, which is
    /// exactly where a player is looking hardest at where their feet are.
    ///
    /// Reported from the window: no shadow from a light block, and a request
    /// for "a generic floating ambient occlusion shadow below me" instead. This
    /// is that, and it is deliberately not an approximation of a sun shadow: it
    /// is round, it is under you, and it does not move with the sun.
    ///
    /// # The ground is found by probing, not by asking
    ///
    /// Nothing tells the client what a body is standing on — `on_ground` is a
    /// boolean and an entity has not even got that. So this walks down from the
    /// feet a few blocks looking for the first solid cell. A body over a drop
    /// gets no blob at all, which is right: there is no ground under it to mark.
    fn place_blobs(&mut self) {
        /// How far down to look for ground, in blocks. Past this a body is over
        /// a drop and casts nothing.
        const REACH: f32 = 4.0;
        /// How dark the disc is directly underfoot.
        const OPACITY: f32 = 0.45;
        /// Lifted clear of the surface, in blocks. The same lesson the
        /// shoreline taught: a quad coplanar with the face under it fights it.
        const LIFT: f32 = 0.02;
        /// How wide the disc is at the ground, as a radius in blocks.
        ///
        /// Doubled from 0.4 after a look from the window: a disc the width of
        /// the body reads as a smudge under your feet rather than as a shadow,
        /// because the soft rim eats most of it. Wider than the caster is what
        /// makes it look like light arriving from more than one direction,
        /// which is what an ambient shadow is.
        const RADIUS: f32 = 0.8;

        let Some(predictor) = self.predictor.as_ref() else {
            self.renderer.set_blobs(&[]);
            return;
        };
        let origin = predictor.origin();
        let voxels = phys::Voxels::new(&self.store, origin);
        #[expect(clippy::cast_precision_loss, reason = "three, as a float")]
        let cells = tiamot_core::SUBNODES_PER_AXIS as f32;
        let now = self.since_start.elapsed();

        // The player first, then everything drawn. Feet in cells relative to
        // the predictor's own chunk, which is the space `Voxels` reads in.
        let mut feet: Vec<([f32; 3], f32)> = Vec::with_capacity(self.entities.len() + 1);
        feet.push((predictor.body().position, RADIUS));
        for (_, entity) in self.entities.iter() {
            let Some(pose) = entity.pose(now) else {
                continue;
            };
            // Into the predictor's chunk frame, which is what `Voxels` indexes.
            let span = tiamot_core::CHUNK_SUBNODES as i32;
            let shift = |axis: usize, chunk: i32| {
                (chunk
                    - match axis {
                        0 => origin.x,
                        1 => origin.y,
                        _ => origin.z,
                    })
                    * span
            };
            feet.push((
                [
                    pose.local[0] + shift(0, pose.chunk.x) as f32,
                    pose.local[1] + shift(1, pose.chunk.y) as f32,
                    pose.local[2] + shift(2, pose.chunk.z) as f32,
                ],
                RADIUS,
            ));
        }

        // **A grid of tiles per body, not one quad.** Reported from the window:
        // the disc "floats and looks strange when I am standing on blocks of
        // sub-nodes, because it kinda just floats on top of them instead of
        // projecting down into them." One quad lies at ONE height and sub-node
        // ground is not at one height — charter rule 19 keeps that terrain, so
        // this is the common case rather than a corner of one.
        //
        // One tile per sub-node column, each probed and placed on its own
        // ground — see `render::blob_columns`, which is where that decision
        // lives and is tested.
        let mut blobs = Vec::with_capacity(feet.len() * 25);
        for (at, radius) in feet {
            // The disc's own height, from under the body, decides whether it is
            // drawn at all and how faded — ONE answer for the whole disc, so a
            // jump fades it evenly rather than tile by tile.
            let Some(ground) = ground_below(&voxels, at, REACH * cells) else {
                continue;
            };
            let above = (at[1] - ground) / cells;
            // Fading and shrinking with height is what makes a jump read as a
            // jump: the disc is the one thing on screen that says how far off
            // the ground you are.
            let closeness = 1.0 - (above / REACH).clamp(0.0, 1.0);
            if closeness <= 0.0 {
                continue;
            }
            let radius = radius * (0.6 + 0.4 * closeness);
            // In cells, which is what the probe and the offsets below speak.
            let reach = radius * cells;
            let corner = tiamot_core::BlockPos::from_chunk_corner(self.drawn_at(origin));

            for column in crate::render::blob_columns(at, reach, |probe| {
                ground_below(&voxels, probe, REACH * cells)
            }) {
                let [dx, dz] = column.offset;
                let world = [
                    f64::from(corner.x) + f64::from(at[0] + dx) / f64::from(cells),
                    f64::from(corner.y)
                        + f64::from(column.ground) / f64::from(cells)
                        + f64::from(LIFT),
                    f64::from(corner.z) + f64::from(at[2] + dz) / f64::from(cells),
                ];
                blobs.push(crate::render::BlobTile {
                    centre: self.camera.position.offset_to(world),
                    // Half a cell, in blocks: one tile covers one sub-node
                    // column, which is the resolution the ground has.
                    half: 0.5 / cells,
                    opacity: OPACITY * closeness,
                    offset: [dx / reach, dz / reach],
                    scale: 0.5 / reach,
                });
            }
        }
        self.renderer.set_blobs(&blobs);
    }

    /// Takes up residence in the world the server just admitted this client to.
    ///
    /// Its own method because `pump_network` is at clippy's line ceiling, and
    /// because joining is a thing worth naming rather than the longest arm of a
    /// match.
    fn joined_world(&mut self, spawn: tiamot_core::BlockPos, tick: u64, may_fly: bool) {
        // Kept so the fly toggle can refuse, rather than predicting a power
        // that would be ignored on arrival.
        self.may_fly = may_fly;

        let position = Position::from_world(
            f64::from(spawn.x) + 0.5,
            f64::from(spawn.y) + 2.0,
            f64::from(spawn.z) + 0.5,
        );
        self.camera.position = position;
        self.spawn = Some(position);
        self.tick = tick;
        self.joined = true;

        // **Ask for the radius this client was configured with.**
        // Sent on join rather than on connect because the server
        // only has a streamer for a player who has reached the
        // world. The answer comes back as `Event::ViewDistance`
        // carrying what was actually granted, which is what the fog
        // is then drawn from — asking is not getting, and the
        // server's own limit is the ceiling.
        self.connection.send(crate::net::Command::ViewDistance {
            horizontal: self.config.view_distance,
            vertical: self.config.vertical_view_distance,
        });

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

    /// Records that this client is hosting its world for other machines.
    pub fn set_hosting(&mut self, address: Option<String>) {
        self.hosting = address;
    }

    /// Where others can join this world, if it is open at all.
    #[must_use]
    pub fn hosting(&self) -> Option<&str> {
        self.hosting.as_deref()
    }

    /// Turns flight on or off, if the server has allowed this player any.
    ///
    /// Returns whether it is on afterwards. A player the server has not made an
    /// operator gets `false` and no change: the bit would be ignored on arrival
    /// anyway, and predicting a flight that is about to be refused is a
    /// correction every tick rather than a feature.
    pub fn toggle_fly(&mut self) -> bool {
        if !self.may_fly {
            return false;
        }
        self.flying = !self.flying;
        self.flying
    }

    /// Whether the server has told this client it may fly.
    #[must_use]
    pub const fn may_fly(&self) -> bool {
        self.may_fly
    }

    /// Whether flight is on right now.
    #[must_use]
    pub const fn flying(&self) -> bool {
        self.flying
    }

    /// The camera's field of view this frame, in radians.
    ///
    /// Exposed so a session test can watch it respond to movement: the whole
    /// point of it is a thing a player feels, and "it looked right" is not a
    /// gate anything can run.
    #[must_use]
    pub const fn fov(&self) -> f32 {
        self.camera.fov_y
    }

    /// Widens the view with speed, and eases it back.
    ///
    /// **Reported from the window**: wanting movement to read as movement —
    /// "when you start walking the camera zooms out just by a tiny bit", and
    /// sprinting "should have an even more extreme fov change... make the fov
    /// based on my speed."
    ///
    /// So it is the speed and not the gait. A gait is what you asked the server
    /// for; speed is what the world let you have, so wading, being underwater
    /// and being shoved all read correctly without any of them being special
    /// cases here.
    ///
    /// Charter rule 4 exempts presentation from the float subset, and this is
    /// as presentational as it gets: nothing here reaches the simulation, and
    /// two machines disagreeing about a camera angle by a millionth is nobody's
    /// problem.
    #[expect(
        clippy::disallowed_methods,
        reason = "charter rule 4 exempts presentation; `exp` here eases a camera angle and \
                  reaches nothing the simulation reads"
    )]
    fn follow_speed(&mut self, dt: f32) {
        /// How much wider the view gets at a full sprint, in radians.
        ///
        /// Twelve degrees at the top end. Small enough that walking is a hint
        /// rather than a lurch — the widening is proportional, so an ordinary
        /// walk gets about two thirds of it.
        const GAIN: f32 = 0.209;
        /// How fast the view catches up, as a fraction of the gap per second.
        ///
        /// Eased rather than set, because the speed itself steps at 20 Hz and a
        /// field of view that stepped with it would strobe. Slower coming back
        /// than going out: a stop should settle rather than snap.
        const OUT: f32 = 9.0;
        const BACK: f32 = 5.0;

        let base = self.config.fov_degrees.to_radians();
        let Some(predictor) = self.predictor.as_ref() else {
            self.camera.fov_y = base;
            return;
        };

        // Horizontal only, and against the SPRINT speed so the scale is the
        // fastest a body goes on foot. Falling is not travelling, and a player
        // dropping down a shaft should not have the world flare open at them.
        let velocity = predictor.body().velocity;
        let speed = (velocity[0] * velocity[0] + velocity[2] * velocity[2]).sqrt();
        let top = phys::Tuning::DEFAULT.sprint_speed;
        let share = if top > 0.0 {
            (speed / top).min(1.0)
        } else {
            0.0
        };

        let want = base + GAIN * share;
        let rate = if want > self.camera.fov_y { OUT } else { BACK };
        // Frame-rate independent easing: the fraction of the gap closed in one
        // second is the same whatever the frame rate, which is the whole reason
        // this is not `+= gap * 0.1`.
        let eased = 1.0 - (-rate * dt.max(0.0)).exp();
        self.camera.fov_y += (want - self.camera.fov_y) * eased;
    }

    /// Which clip the player's own figure is playing.
    ///
    /// **Derived here rather than sent.** The server tags an entity's animation
    /// and the client plays it, but the local player's body moves at prediction
    /// rate — a gait arriving at twenty hertz would lag every start and stop by
    /// up to a frame and a half of walking.
    fn gait(&self) -> u8 {
        use tiamot_core::ent::AnimTag;
        let Some(predictor) = self.predictor.as_ref() else {
            return AnimTag::IDLE.0;
        };
        let velocity = predictor.body().velocity;
        // Horizontal only, and squared: falling is not walking, a player
        // stepping off a ledge should not break into a run on the way down, and
        // `hypot` is on the determinism ban list even here where it would be
        // harmless — one rule, everywhere, is what keeps it a rule.
        let speed = velocity[0] * velocity[0] + velocity[2] * velocity[2];
        if speed < IDLE_SPEED * IDLE_SPEED {
            AnimTag::IDLE.0
        } else if speed > RUN_SPEED * RUN_SPEED {
            AnimTag::RUN.0
        } else {
            AnimTag::WALK.0
        }
    }

    fn place_entities(&mut self) {
        let now = self.since_start.elapsed();
        let cells = f64::from(tiamot_core::SUBNODES_PER_AXIS);

        let mut placed = Vec::with_capacity(self.entities.len());
        for (id, entity) in self.entities.iter() {
            // **The engine's rig, or nothing.** A model is a canonical string
            // id and the only one the client has is its own; a server naming
            // another is naming something it has not pushed yet, and drawing a
            // humanoid for it would put a person where a mod meant a crate.
            // An entity with no model at all is a marker and is meant to be
            // invisible.
            if entity.model.as_deref() != Some(tiamot_core::ent::HUMANOID_MODEL) {
                continue;
            }
            let Some(pose) = entity.pose(now) else {
                continue;
            };
            let corner = tiamot_core::BlockPos::from_chunk_corner(pose.chunk);
            // The figure stands ON its feet, not centred in a box: the rig's
            // origin is between them, which is where the server's position is.
            let feet = [
                f64::from(corner.x) + f64::from(pose.local[0]) / cells,
                f64::from(corner.y) + f64::from(pose.local[1]) / cells,
                f64::from(corner.z) + f64::from(pose.local[2]) / cells,
            ];
            placed.push(crate::render::skinned::Figure {
                offset: self.camera.position.offset_to(feet),
                yaw: pose.yaw,
                anim: pose.anim,
                // **Each figure keeps its own clock**, offset by its id. Two
                // hundred mobs sharing one march in step, which reads as a
                // chorus line rather than as a crowd — and the offset has to be
                // stable across frames or the phase jitters, which is why it
                // comes from the id rather than from anything about the frame.
                phase: now.as_secs_f32() + (id % 977) as f32 * 0.037,
                // What the entity stream says it is holding, so the arm is out
                // for something that is actually drawn — see `place_props`.
                carrying: [entity.hands[0].is_some(), entity.hands[1].is_some()],
            });
        }
        self.renderer.set_entities(placed);
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
    /// Sends one input per tick, and **deliberately not the last three**.
    ///
    /// Task 09's design says inputs are "sent unreliably with redundancy (last
    /// 3 inputs per packet)". That is the right design for a datagram
    /// transport, where an input that is lost is simply gone. This engine does
    /// not have one: `client::net` opens a **bidirectional QUIC stream**, which
    /// is reliable and ordered, so an input either arrives or the connection is
    /// over. Sending each one three times would triple the input bandwidth to
    /// insure against a loss the transport has already ruled out.
    ///
    /// The server-side machinery for redundancy exists anyway and is not
    /// wasted: `phys::InputQueue` ignores duplicates, which is what makes a
    /// change of transport a one-line change here rather than a protocol
    /// question. If inputs ever move to datagrams — the reason to, one day,
    /// being that a lost input should not delay every input behind it — this is
    /// where the last three start being sent.
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

    /// The time of day, as a line a person can read.
    ///
    /// A 24-hour clock rather than the raw fraction, because "0.5" means
    /// nothing at a glance and "12:00" means noon to everybody. The fraction is
    /// kept beside it for anyone comparing against a server log.
    fn clock_line(&self) -> String {
        if !self.sky.has_day() {
            return "no sky mod: permanent daylight".to_owned();
        }
        let time = self.sky.time();
        let minutes = (time * 24.0 * 60.0) as u32;
        // The sun's height above the horizon, which is what decides how long a
        // shadow is — the intensity alone says how bright it is and not where
        // it is, and "where is the sun" is the question somebody looking at
        // shadows is actually asking.
        let down = self.sky.sun_direction()[1];
        format!(
            "{:02}:{:02} · sun {:.0}% up, {:.0}% bright{}",
            minutes / 60,
            minutes % 60,
            -down * 100.0,
            self.renderer.sun_intensity() * 100.0,
            if self.time_override { " · LOCAL" } else { "" }
        )
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
        let phases = self.pacing.worst_frame_phases();
        let (created, reused) = self.renderer.buffer_stats();
        let correction = self.pacing.worst_correction_cells();

        vec![
            self.frame_rate_line(),
            // The average above is the reassuring number; this is the honest
            // one. Charter rule 18 measures pacing, and a 900 fps average with
            // an 11 ms worst frame is a hitch the average cannot express.
            format!(
                "worst frame {worst:.1} ms ({:.0} fps) · worst remesh {remesh_ms:.1} ms over \
                 {remesh_chunks} chunks ({meshing:.1} mesh + {upload:.1} upload)",
                if worst > 0.0 { 1000.0 / worst } else { 0.0 }
            ),
            // What that worst frame was actually doing. `acquire` and `present`
            // are the swapchain, so time there is the GPU or the compositor
            // holding the frame rather than the client working; `rest` is
            // whatever the phases did not account for, which is winit, event
            // handling, and the gap between frames.
            format!(
                "  = net {:.1} + remesh {:.1} + advance {:.1} + acquire {:.1} + world {:.1} + \
                 hud {:.1} + present {:.1} + rest {:.1}",
                phases.network,
                phases.remesh,
                phases.advance,
                phases.acquire,
                phases.world,
                phases.hud,
                phases.present,
                (worst - phases.total()).max(0.0),
            ),
            self.prediction_line(created, reused, correction),
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
                "{} of meshes, {material_count} materials",
                human_bytes(self.renderer.mesh_bytes())
            ),
            // Vsync sits here because a worst-frame figure cannot be read
            // without it. An unsynchronised loop is back-pressured by the
            // swapchain, which lands in `Phases::acquire` and looks exactly
            // like a hitch — and working out which mode a reading came from
            // otherwise costs a round trip with whoever took it.
            self.clock_line(),
            // The debug keys, on the screen rather than in a commit message.
            // Every one of these was added for somebody to look at something
            // with, and a key nobody can find is a feature nobody has.
            self.keys_line(),
            format!(
                "{} on {} / {} · vsync {} · light {} · shadows {}",
                self.server_label,
                self.renderer.gpu().adapter,
                self.renderer.gpu().backend,
                self.present_mode
                    .unwrap_or(if self.config.vsync { "on" } else { "OFF" }),
                self.config.lighting_mode.name(),
                self.config.shadow_quality.name()
            ),
            // The hotbar, such as it is. Names rather than ids: a player
            // debugging a placement needs to know it is stone, not that it is 2.
            if self.carried.is_empty() {
                "carrying nothing — dig something".to_owned()
            } else {
                self.carried
                    .iter()
                    .enumerate()
                    .map(|(slot, stack)| {
                        let (id, units) = (&stack.material, &stack.units);
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

    /// Adopts the tool table a server's mods registered.
    ///
    /// **Recorded, not announced.** The table arrives while the session is
    /// still `Authenticated`, and `SelectTool` is only valid in world —
    /// replying here got the client disconnected with "`SelectTool` is not
    /// valid in phase Authenticated".
    ///
    /// There is nothing to announce anyway: a player who has selected nothing
    /// digs with the server's own default, which is the same tool this picks.
    /// The first `next_tool` is the first time the two could disagree, and that
    /// is when it is sent.
    fn adopt_tools(&mut self, tools: Vec<tiamot_core::proto::ToolDef>) {
        // The default first, so a player who never touches the tool key is
        // holding whatever the mods call a bare hand.
        self.held_tool = tools.iter().position(|tool| tool.default).unwrap_or(0);
        self.tools = tools;
    }

    /// Loads a HUD script a server pushed.
    ///
    /// **Nothing about this is optional for the client.** A refused script is a
    /// warning naming the mod, not a refused connection: a server whose HUD
    /// will not load is a server with a worse HUD, and a client that dropped
    /// the world over it would be unplayable on the day a mod shipped a typo.
    fn adopt_hud_script(&mut self, mod_id: &str, source: &str) {
        let Some(vm) = self.hud_vm.as_mut() else {
            self.warn(format!(
                "`{mod_id}` pushed a HUD script, but this client has no script runtime to run it                  in"
            ));
            return;
        };
        if let Err(err) = vm.load(mod_id, source) {
            self.warn(format!("`{mod_id}`'s HUD script would not load: {err}"));
        }
    }

    /// Runs every pushed HUD script for this frame.
    ///
    /// Called once a frame, from the renderer, because that is what "immediate
    /// mode" means: nothing a script drew last frame survives into this one.
    /// Faults come back here and become warnings, which is the only place the
    /// player sees them.
    pub fn run_hud_scripts(&mut self) {
        let Some(state) = self.hud_state() else {
            return;
        };
        let Some(vm) = self.hud_vm.as_mut() else {
            return;
        };
        let faults = vm.draw(&state);
        for fault in faults {
            self.warn(fault.message);
        }
    }

    /// What the pushed scripts drew, for the renderer.
    ///
    /// `None` when there is no runtime at all. An empty frame is not the same
    /// thing and is the ordinary case on a server that pushes no HUD.
    pub fn hud_frame<T>(&self, visit: impl FnOnce(&tiamot_core::hud::Frame) -> T) -> Option<T> {
        self.hud_vm.as_ref().and_then(|vm| vm.with_frame(visit))
    }

    /// Everything a HUD script is allowed to know, this frame.
    ///
    /// `None` before the player exists — there is no situation to describe yet,
    /// and a script asked about one would draw a hotbar of zeroes.
    fn hud_state(&self) -> Option<tiamot_core::hud::State> {
        let predictor = self.predictor.as_ref()?;
        let (x, y, z) = self.camera.position.to_world();
        let carried = self
            .hotbar
            .iter()
            .map(|slot| {
                slot.map(|stack| tiamot_core::hud::Carried {
                    material: tiamot_core::MaterialId(stack.material),
                    // The string id, because that is what is canonical (charter
                    // rule 8) and the number is per-session. A script showing a
                    // name shows this one.
                    name: self
                        .materials
                        .get(&stack.material)
                        .cloned()
                        .unwrap_or_else(|| format!("#{}", stack.material)),
                    units: stack.units,
                    shape: stack.shape,
                })
            })
            .collect();
        let offhand = self.offhand().map(|stack| tiamot_core::hud::Carried {
            material: tiamot_core::MaterialId(stack.material),
            name: self
                .materials
                .get(&stack.material)
                .cloned()
                .unwrap_or_else(|| format!("#{}", stack.material)),
            units: stack.units,
            shape: stack.shape,
        });
        let voxels = phys::Voxels::new(&self.store, predictor.origin());
        let looking_at = self.looking_at().map(|hit| {
            let material = voxels
                .material(hit.cell[0], hit.cell[1], hit.cell[2])
                .unwrap_or(tiamot_core::MaterialId::AIR);
            tiamot_core::hud::Look {
                cell: hit.cell,
                material,
                name: self
                    .materials
                    .get(&material.0)
                    .cloned()
                    .unwrap_or_else(|| format!("#{}", material.0)),
            }
        });
        Some(tiamot_core::hud::State {
            position: [x, y, z],
            yaw: self.camera.yaw,
            pitch: self.camera.pitch,
            time_of_day: self.sky_time(),
            selected: self.selected,
            carried,
            offhand,
            looking_at,
            // Per-mille, and the cast saturates on a non-finite progress the
            // way `Fill` expects — a dig progress is a server number and this
            // is the last place it is a float.
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a saturating cast into a value `Fill` then clamps"
            )]
            dig: self
                .dig
                .map(|(_, progress)| tiamot_core::hud::Fill::per_mille((progress * 1000.0) as i32)),
            tool: self.held_tool().map(|tool| tiamot_core::hud::HeldTool {
                id: tool.id.clone(),
                name: tool.name.clone(),
                brush: tool.brush.clone(),
            }),
        })
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

/// A byte count at a readable scale.
///
/// KiB below a mebibyte. A flat world greedy-meshes to about forty kilobytes
/// across two hundred chunks, so a MiB-only readout reads "0.0 MiB" for every
/// scene anyone actually tests on and answers nothing.
fn human_bytes(bytes: u64) -> String {
    let bytes = bytes as f64;
    if bytes < 1024.0 * 1024.0 {
        format!("{:.0} KiB", bytes / 1024.0)
    } else {
        format!("{:.1} MiB", bytes / (1024.0 * 1024.0))
    }
}

/// The compass direction a yaw points, for the HUD.
///
/// Yaw 0 looks along +z, and the axes are named the way the world is: +x east,
/// The height of the first solid cell below `at`, in cells, or `None`.
///
/// Walks down a cell at a time from just under the feet. Cheap — a handful of
/// lookups per body per frame — and the alternative is a ray cast answering the
/// same question with more arithmetic. `None` means a body over a drop, which
/// correctly casts no blob: there is no ground under it to mark.
fn ground_below<S: tiamot_core::phys::ChunkLookup>(
    voxels: &phys::Voxels<'_, S>,
    at: [f32; 3],
    reach: f32,
) -> Option<f32> {
    // `detgen::floor_to_i32` rather than `f32::floor`: the lint that says so is
    // scoped to determinism and this is presentation, but there is one spelling
    // of a floor in this workspace and using the other one here would be a
    // reader's question every time.
    let (x, z) = (
        tiamot_core::detgen::floor_to_i32(at[0]),
        tiamot_core::detgen::floor_to_i32(at[2]),
    );
    let top = tiamot_core::detgen::floor_to_i32(at[1]);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a reach of a few blocks is a small integer"
    )]
    let steps = reach.max(0.0) as i32;
    for step in 0..=steps {
        let y = top - step;
        if voxels
            .material(x, y, z)
            .is_some_and(|material| material != tiamot_core::MaterialId::AIR)
        {
            // The TOP of that cell is the surface the disc lies on.
            #[expect(
                clippy::cast_precision_loss,
                reason = "cell coordinates are far inside f32's exact integer range"
            )]
            return Some((y + 1) as f32);
        }
    }
    None
}

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

/// What a client's tick counter should do about the server's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resync {
    /// Close enough. The client leads by roughly [`INPUT_LEAD`], as it should.
    Settled,
    /// So far ahead that the server would refuse its inputs. Snap back.
    Ahead,
    /// Behind: simulate this many skipped ticks, then take the label.
    Behind {
        /// Ticks to step through, bounded by [`MAX_RESYNC_CATCH_UP`].
        steps: u64,
    },
}

/// Which of those a client at `client` should do, given a server at `server`.
///
/// # Why ahead is a case at all
///
/// **A client that runs ahead of its server never recovers on its own.**
/// `InputQueue::offer` refuses any tick more than
/// [`tiamot_core::phys::input::MAX_LOOKAHEAD`] past the one being applied, and
/// both ends then advance at the same 20 Hz — so a gap opened once stays open
/// and every input from then on is thrown away. Reported from the window as
/// walking putting the player back where they started after using the pause
/// menu, which stops the SERVER's tick and not the client's.
///
/// The trigger is half the server's own bound rather than a number of this
/// module's, so it fires before an input would be refused rather than after.
const fn resync_plan(client: u64, server: u64) -> Resync {
    let want = server + INPUT_LEAD;
    if client > want + tiamot_core::phys::input::MAX_LOOKAHEAD / 2 {
        return Resync::Ahead;
    }
    if client >= want {
        return Resync::Settled;
    }
    let gap = want - client;
    Resync::Behind {
        steps: if gap < MAX_RESYNC_CATCH_UP as u64 {
            gap
        } else {
            MAX_RESYNC_CATCH_UP as u64
        },
    }
}

/// Pairs of actions whose DEFAULT key is the same, as `(held, clashing)`.
///
/// Defaults only: a player who has deliberately put two actions on one key has
/// said what they meant. See [`App::warn_about_shared_defaults`], which is the
/// only caller and does nothing but phrase this.
fn shared_defaults(actions: &crate::input::Actions) -> Vec<(String, String)> {
    let mut seen: std::collections::BTreeMap<crate::input::Input, String> =
        std::collections::BTreeMap::new();
    let mut clashes = Vec::new();
    for action in actions.iter() {
        let Some(default) = action.default else {
            continue;
        };
        match seen.entry(default) {
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(action.id.clone());
            }
            std::collections::btree_map::Entry::Occupied(held) => {
                clashes.push((held.get().clone(), action.id.clone()));
            }
        }
    }
    clashes
}
/// Whether a held dig keeps chewing the block it started on.
///
/// # What the lock is for, and what it was doing wrong
///
/// Holding the button re-aims every frame, so without a lock a block that
/// finished being dug would hand the crosshair straight to the one behind it and
/// a held button would bore a tunnel. The lock keeps a dig on ONE block until
/// that block is gone.
///
/// It used to hold on that condition alone — "the block still has something in
/// it" — which is why walking forward with the button down kept eating the
/// block behind you instead of taking the next one. Reported from the window:
/// *"the previous block gets deleted before the next one; it should grab each
/// next block as I walk."*
///
/// # Why it is not "the crosshair is still on it"
///
/// That was the first fix and it broke the case the lock exists for. A half-dug
/// block lets the ray through, so the crosshair lands on the block BEHIND while
/// the locked one still has material, and the dig tunnels —
/// `a_held_dig_finishes_its_block_before_looking_through_the_hole` failed
/// immediately and said so.
///
/// So the question is where the block IS, not what the ray hits: a dig holds
/// while the block is still in FRONT of the player. Looking through a hole you
/// made keeps it ahead of you; walking past it puts it behind, and the next
/// block is taken.
///
/// `to_block` runs from the eye to the locked block's centre and `forward` is
/// where the camera looks. Neither needs normalising, because only the SIGN of
/// the dot product is read.
fn keeps_lock(to_block: [f32; 3], forward: [f32; 3], block_has_material: bool) -> bool {
    if !block_has_material {
        return false;
    }
    let ahead = to_block[0] * forward[0] + to_block[1] * forward[1] + to_block[2] * forward[2];
    ahead > 0.0
}

#[cfg(test)]
mod dig_lock_tests {
    use super::*;

    /// Looking north, which is `+z`.
    const NORTH: [f32; 3] = [0.0, 0.0, 1.0];

    #[test]
    fn a_dig_follows_you_to_the_next_block_as_you_walk() {
        // **The reported bug.** The lock held on "the block still has something
        // in it" alone, so walking forward with the button down kept eating the
        // block behind you until it was gone. Walking past it puts it behind.
        assert!(
            !keeps_lock([0.0, 0.0, -4.0], NORTH, true),
            "the block is behind the player and the dig stayed on it"
        );
    }

    #[test]
    fn looking_through_the_hole_you_made_keeps_the_block() {
        // The case the lock exists for, and the one a "crosshair is still on
        // it" rule broke: a half-dug block lets the ray through to the one
        // behind, and dropping the lock there bores a tunnel.
        assert!(keeps_lock([0.0, 0.0, 3.0], NORTH, true));
    }

    #[test]
    fn an_empty_block_releases_the_crosshair() {
        // What the lock exists to end: once the block is gone the next one is
        // chosen normally, however squarely it is still being looked at.
        assert!(!keeps_lock([0.0, 0.0, 3.0], NORTH, false));
    }

    #[test]
    fn a_block_beside_you_is_already_let_go() {
        // Exactly abeam is not in front. Walking along a wall reaches this the
        // moment the block passes the shoulder, which is when the next one
        // should be taken.
        assert!(!keeps_lock([4.0, 0.0, 0.0], NORTH, true));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiamot_core::proto::MaterialDef;

    #[test]
    fn a_client_that_ran_ahead_of_its_server_snaps_back() {
        use tiamot_core::phys::input::MAX_LOOKAHEAD;

        // **Reported from the window**: after the pause menu, walking put the
        // player straight back where they started, for ever, and their own body
        // stopped animating. A paused world stops the SERVER's tick and not the
        // client's, so the client counted on through the menu — and the server
        // refuses any input more than `MAX_LOOKAHEAD` past the tick it is
        // applying. Both ends then run at the same rate, so the gap never
        // closes: every input after that menu was thrown away.
        let server = 1_000;
        let settled = server + INPUT_LEAD;

        // A menu held open for ten seconds is 200 ticks of divergence.
        assert_eq!(resync_plan(settled + 200, server), Resync::Ahead);

        // The trigger is BEFORE the server would refuse, not after: a client
        // exactly at the refusal bound is already broken.
        assert_eq!(resync_plan(settled + MAX_LOOKAHEAD, server), Resync::Ahead);

        // And a healthy lead is left alone — this must not fire on the normal
        // case, which is a client leading by its own latency.
        assert_eq!(resync_plan(settled, server), Resync::Settled);
        assert_eq!(resync_plan(settled + 1, server), Resync::Settled);
        assert_eq!(
            resync_plan(settled + MAX_LOOKAHEAD / 2, server),
            Resync::Settled,
            "a client inside the server's own tolerance was snapped back"
        );

        // Behind is unchanged: simulate the gap, bounded, rather than renumber.
        assert_eq!(
            resync_plan(settled - 2, server),
            Resync::Behind { steps: 2 }
        );
        assert_eq!(
            resync_plan(settled - 500, server),
            Resync::Behind {
                steps: u64::from(MAX_RESYNC_CATCH_UP)
            },
            "a client that was away must not replay a minute of movement"
        );
    }

    #[test]
    fn two_actions_on_one_default_key_are_reported_to_the_player() {
        // **A mod cannot ask what is already bound**, and should not be able to
        // — the engine owns bindings and mods never read keys (charter rule
        // 11). So a mod suggesting a key the engine already uses is a mistake
        // nobody is positioned to catch, and the failure is SILENT:
        // `Bindings::action_for` takes the first match and the other action
        // never fires again.
        //
        // Found writing `game/core_gear`, whose first draft put its screen on
        // `KeyR` — which is `engine:next_tool`.
        let mut actions = crate::input::Actions::engine();
        let taken = actions
            .iter()
            .find_map(|action| action.default.map(|input| (action.id.clone(), input)));
        let (engine_action, key) = taken.expect("the engine binds something by default");

        actions
            .register(crate::input::Action {
                id: "a_mod:thing".to_owned(),
                description: "does a thing".to_owned(),
                source: crate::input::Source::Mod("a_mod".to_owned()),
                default: Some(key),
            })
            .expect("register");

        // The clash-finder on its own, without a whole App: it is a pure walk
        // over the action list, which is the part that can be wrong.
        let mut seen: std::collections::BTreeMap<crate::input::Input, String> =
            std::collections::BTreeMap::new();
        let mut clashes = Vec::new();
        for action in actions.iter() {
            let Some(default) = action.default else {
                continue;
            };
            match seen.entry(default) {
                std::collections::btree_map::Entry::Vacant(slot) => {
                    slot.insert(action.id.clone());
                }
                std::collections::btree_map::Entry::Occupied(held) => {
                    clashes.push((held.get().clone(), action.id.clone()));
                }
            }
        }
        assert_eq!(
            clashes,
            vec![(engine_action, "a_mod:thing".to_owned())],
            "a mod landing on a key the engine already uses went unreported"
        );

        // The counter-example: the engine's own defaults do not clash with each
        // other, so this reports a real thing rather than always reporting.
        let clean = crate::input::Actions::engine();
        let mut seen: std::collections::BTreeSet<crate::input::Input> =
            std::collections::BTreeSet::new();
        for action in clean.iter() {
            if let Some(default) = action.default {
                assert!(
                    seen.insert(default),
                    "two ENGINE actions share `{default:?}` by default"
                );
            }
        }
    }

    #[test]
    fn a_figure_faces_the_way_the_camera_looks() {
        // **The two conventions, checked against each other rather than
        // asserted.** A mod writes a heading as `atan2(dx, dz)`, so a figure
        // faces `(sin θ, cos θ)`; the camera's forward is `(−sin θ, cos θ)`
        // because east is `−x`. Reported from the window as a body that did not
        // face the way it was walking.
        //
        // Zero is deliberately not the only case: at yaw zero both point north
        // and a missing conversion is invisible.
        for degrees in [0.0_f32, 45.0, 90.0, 180.0, 270.0, -30.0] {
            let camera = crate::camera::Camera {
                yaw: degrees.to_radians(),
                pitch: 0.0,
                ..crate::camera::Camera::default()
            };
            let looking = camera.forward();
            let heading = figure_yaw(camera.yaw);
            #[expect(
                clippy::disallowed_methods,
                reason = "charter rule 4 exempts presentation; this is a camera heading"
            )]
            let (facing_x, facing_z) = (heading.sin(), heading.cos());
            assert!(
                (facing_x - looking.x).abs() < 1e-5 && (facing_z - looking.z).abs() < 1e-5,
                "at {degrees}° the camera looks ({}, {}) and the figure faces ({facing_x}, \
                 {facing_z})",
                looking.x,
                looking.z
            );
        }

        // And the counter-example: passing the camera's yaw straight through —
        // which is what it did — disagrees the moment you turn.
        let camera = crate::camera::Camera {
            yaw: std::f32::consts::FRAC_PI_2,
            pitch: 0.0,
            ..crate::camera::Camera::default()
        };
        #[expect(
            clippy::disallowed_methods,
            reason = "charter rule 4 exempts presentation; this is a camera heading"
        )]
        let unconverted = camera.yaw.sin();
        assert!(
            (unconverted - camera.forward().x).abs() > 1.0,
            "the unconverted heading happens to agree, so this proves nothing"
        );
    }

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
        let quiet = Phases::default();
        // The hitching frame spent its time waiting on the swapchain, which is
        // the case the breakdown exists to distinguish from the client's own
        // work.
        let stalled = Phases {
            present: 10.5,
            ..Phases::default()
        };
        for _ in 0..899 {
            pacing.frame(1.0 / 900.0, quiet);
        }
        pacing.remesh(11.0, 0.4, 4);
        pacing.frame(0.011, stalled);
        // One more frame to close the window and publish it.
        pacing.frame(1.0 / 900.0, quiet);

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
        assert_eq!(
            pacing.worst_frame_phases(),
            stalled,
            "the breakdown reported belongs to some other frame; the 899 quiet frames each \
             had their own, and reporting one of those would describe the wrong frame"
        );
    }

    #[test]
    fn a_new_pacing_window_forgets_the_last_one() {
        // Otherwise the worst frame is the worst frame EVER, and a single stall
        // during startup would sit on the HUD for the rest of the session
        // claiming the client still hitches.
        let mut pacing = Pacing::default();
        let quiet = Phases::default();
        let stalled = Phases {
            present: 10.5,
            ..Phases::default()
        };
        pacing.remesh(11.0, 0.4, 4);
        pacing.frame(0.011, stalled);
        pacing.frame(1.0, stalled);
        assert!((pacing.worst_frame_ms() - 1000.0).abs() < 0.01);

        // A quiet second after it.
        for _ in 0..60 {
            pacing.frame(1.0 / 60.0, quiet);
        }
        pacing.frame(1.0 / 60.0, quiet);
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
        assert_eq!(
            pacing.worst_frame_phases(),
            quiet,
            "the stalled frame's breakdown outlived its window, so the HUD would still be \
             blaming the swapchain a second after the stall"
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
                placeable: true,
                step_sound: None,
            },
            MaterialDef {
                id: 5,
                name: "core:white".to_owned(),
                texture: Some([0u8; 32]),
                placeable: true,
                step_sound: None,
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
            placeable: true,
            step_sound: None,
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

/// Pushes every renderer-owned setting from `config` into `renderer`.
///
/// The single place a [`Config`] becomes renderer state. It exists because the
/// alternative — each setting applied only by the key that toggles it — makes a
/// renderer built at one moment silently ignore every choice made after it.
///
/// Fog far is here too: it reaches the sky exactly where the loaded world
/// stops, so terrain dissolves into the horizon rather than ending at a visible
/// edge, and until a sky mod says otherwise the fog colour IS the clear colour.
///
/// [`crate::config::RenderMode`] is not here and cannot be: it selects the
/// pipelines at construction, so changing it rebuilds the renderer.
pub fn apply_to_renderer(config: &Config, renderer: &mut Renderer) {
    renderer.set_lighting_mode(config.lighting_mode);
    renderer.set_shadow_quality(config.shadow_quality);
    renderer.set_sky(
        crate::render::sky_colour(),
        f32::from(config.view_distance) * tiamot_core::CHUNK_BLOCKS as f32,
    );
}
