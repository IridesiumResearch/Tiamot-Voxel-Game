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

use tiamot_core::{ChunkPos, MaterialId};

use crate::camera::{Camera, Position};
use crate::config::Config;
use crate::mesher;
use crate::net::{Command, Connection, Event};
use crate::render::Renderer;
use crate::texture::{Atlas, Image};
use crate::world::{ABSENT_POLICY, ChunkStore};

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
    /// A one-shot debug teleport.
    pub teleport: Option<Teleport>,
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
    /// The server's tick when it last said so.
    tick: u64,
    /// What the connection reported about the server's certificate.
    server_label: String,
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
            tick: 0,
            server_label: "connecting…".to_owned(),
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

        for pos in &due {
            let Some(chunk) = self.store.get(*pos) else {
                continue;
            };
            let neighbours = self.store.neighbours(*pos);
            let mesh = mesher::mesh_chunk(chunk, &neighbours, ABSENT_POLICY);
            self.renderer.set_chunk(self.drawn_at(*pos), &mesh);
        }
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

        let sensitivity = self.config.mouse_sensitivity;
        self.camera
            .look(input.look.0 * sensitivity, -input.look.1 * sensitivity);

        let speed = self.config.fly_speed * if input.sprint { 4.0 } else { 1.0 } * dt;
        if input.forward != 0.0 || input.right != 0.0 || input.up != 0.0 {
            self.camera
                .fly(input.forward * speed, input.right * speed, input.up * speed);
        }

        if let Some(teleport) = input.teleport {
            self.teleport(teleport);
        }
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
                "teleported to chunk {:?} — the world came too, so this frame should be \
                 identical to the one at spawn. Any shimmer is a world coordinate that \
                 survived {TELEPORT_DISTANCE} blocks out",
                self.camera.position.chunk
            ),
            Teleport::Home => "back at the origin".to_owned(),
        });
    }

    /// Sends the frame's input to the server.
    ///
    /// Movement is reported rather than applied: the server owns the
    /// simulation (charter rule 2), and Task 09's physics is what consumes
    /// this. Sending it now means the wire format is exercised from the first
    /// visible build rather than first used on the day it starts to matter.
    pub fn report_input(&self, input: Input) {
        let turn = std::f32::consts::TAU;
        self.connection.send(Command::Input {
            tick: self.tick,
            movement: [input.right, input.up, input.forward],
            look: [self.camera.yaw / turn, self.camera.pitch / turn],
            actions: 0,
        });
    }

    /// The HUD lines, top to bottom.
    #[must_use]
    pub fn hud(&self) -> Vec<String> {
        let (x, y, z) = self.camera.position.to_world();
        let facing = compass(self.camera.yaw);
        let material_count = self.materials.len();

        vec![
            format!("{:.0} fps", self.fps),
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
}
