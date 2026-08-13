// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Engine core: voxel data, simulation, scripting runtime, physics,
//! persistence, and protocol types.
//!
//! This crate is the whole simulation. The server is a thin binary over it and
//! the client is a viewer onto it — there is exactly one simulation code path
//! (charter rule 2).
//!
//! # Dependency firewall
//!
//! This crate must never depend on `wgpu`, `winit`, `kira`, or `egui`, whether
//! directly or transitively (charter rule 3). The rule exists so the headless
//! server can build and run on a machine with no display server, no GPU, and no
//! audio device. `scripts/check-dep-firewall.sh` enforces it in CI across every
//! target platform, so a target-specific dependency cannot smuggle one in.
//!
//! # Units
//!
//! One block is one yard cubed, subdivided 3×3×3 into 27 sub-node units
//! (charter rule 5). Quantities are stored in units as `u32` throughout; there
//! are no special cases for partial blocks anywhere in the engine. See
//! [`inventory`] for the arithmetic and [`block::subnode_index`] for the
//! canonical sub-node addressing convention.
//!
//! # Determinism
//!
//! Simulation code in this crate is restricted to the Deterministic Float
//! Subset (charter rule 4): the IEEE-exact operations `+ - * / %`, `sqrt`,
//! `abs`, `copysign`, and comparisons. Transcendentals, `mul_add`, NaN
//! production in simulation state, and float accumulation over non-deterministic
//! iteration order are banned. Task 04 populates the `clippy.toml`
//! `disallowed-methods` list that enforces this and writes
//! `docs/float-determinism.md`, the authoritative reference.
//!
//! Nothing in the voxel data model uses floats at all, and no map in it is a
//! `HashMap`: ordered containers are used throughout so iteration order is a
//! property of the data rather than of the process's hash seed.

#![warn(missing_docs)]
#![warn(clippy::pedantic)]
// Voxel code converts between unsigned counts and signed coordinates on almost
// every line: array indices and sizes are `usize`/`u32`, world coordinates are
// `i32`, and sub-node offsets are 0..3. Every such conversion in this crate is
// bounded by values that fit comfortably — the widest is the world extent at
// ±180,000 sub-nodes, five orders of magnitude inside `i32` — and the real
// guard is the world-bound check in `coords`, not a cast annotation. Marking
// each site individually would add hundreds of attributes and make the genuine
// ones invisible.
// `cast_precision_loss` joins them for the same reason with one extra
// guarantee: every `i32 as f32` in the noise path is on a lattice coordinate
// already clamped to 2^22, which f32 represents exactly. The conversion is
// provably lossless there, and the lint cannot see the clamp.
#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss
)]

pub mod bitpack;
pub mod block;
pub mod chunk;
pub mod content;
pub mod coords;
pub mod detgen;
pub mod dig;
pub mod fluid;
pub mod identity;
pub mod interest;
pub mod inventory;
pub mod light;
pub mod material;
pub mod modload;
pub mod persist;
pub mod phys;
pub mod place;
pub mod proto;
#[cfg(feature = "script")]
pub mod script;
pub mod session;
pub mod sky;
pub mod tick;

pub use block::{BlockContent, BlockValue, BlockView, Cells, SlotIndex};
pub use chunk::Chunk;
pub use coords::{BlockPos, ChunkPos, CoordError, SubNodePos};
pub use detgen::{ChunkBuffer, StreamRng, assert_ieee_mode, fingerprint};
pub use identity::{Identity, PlayerUuid};
pub use inventory::Stack;
pub use material::{MaterialId, MaterialRegistry, Registry};
pub use persist::{WorldDb, WorldError};

/// Sub-node subdivisions along each axis of a block.
///
/// One block is `SUBNODES_PER_AXIS` cubed units (charter rule 5).
pub const SUBNODES_PER_AXIS: u32 = 3;

/// Sub-node units in one whole block.
///
/// All inventory and world quantities are stored in units. Display splits them
/// as `units / UNITS_PER_BLOCK` blocks plus `units % UNITS_PER_BLOCK` nodes —
/// see [`inventory::display`].
pub const UNITS_PER_BLOCK: u32 = SUBNODES_PER_AXIS * SUBNODES_PER_AXIS * SUBNODES_PER_AXIS;

/// Blocks along each axis of a chunk.
///
/// This number is load-bearing, not arbitrary (charter rule 6): a chunk is
/// `CHUNK_BLOCKS * SUBNODES_PER_AXIS` = 48 sub-node cells per axis, and 48 cells
/// plus the 2 padding bits that binary greedy meshing needs for neighbour-face
/// culling is 50 bits — one `u64` column. A 32-block chunk would need 96 bits
/// and lose the technique entirely. Do not change this without redesigning the
/// mesher.
pub const CHUNK_BLOCKS: u32 = 16;

/// Sub-node cells along each axis of a chunk.
pub const CHUNK_SUBNODES: u32 = CHUNK_BLOCKS * SUBNODES_PER_AXIS;

/// Blocks in one chunk.
pub const BLOCKS_PER_CHUNK: usize = (CHUNK_BLOCKS * CHUNK_BLOCKS * CHUNK_BLOCKS) as usize;

/// Padding bits binary greedy meshing needs on a sub-node column for
/// neighbour-face culling — one at each end.
pub const MESHING_PADDING_BITS: u32 = 2;

// Charter rule 6, enforced at compile time rather than in a test: the whole
// reason CHUNK_BLOCKS is 16 is that a padded sub-node column fits in one u64.
// Changing CHUNK_BLOCKS to 32 should stop the build here, at the constant that
// explains why, rather than in the mesher months later.
const _: () = assert!(CHUNK_SUBNODES + MESHING_PADDING_BITS <= u64::BITS);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_block_is_twenty_seven_units() {
        assert_eq!(UNITS_PER_BLOCK, 27);
    }

    #[test]
    fn a_chunk_is_forty_eight_subnodes_per_axis() {
        // Charter rule 6. The u64-column invariant this exists to protect is
        // asserted at compile time above; this pins the concrete number that
        // the persistence format and the mesher both encode.
        assert_eq!(CHUNK_SUBNODES, 48);
        assert_eq!(BLOCKS_PER_CHUNK, 4096);
    }
}
