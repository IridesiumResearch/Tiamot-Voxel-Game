// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Light levels, and what lets light through.
//!
//! **Sub-Node Contract §3 is the authority here**, and it decides the two facts
//! that shape this module: light is stored **per block**, and sub-nodes affect
//! only whether light crosses a face.
//!
//! # Why block resolution, when everything else is sub-node
//!
//! A sub-node light field would be 27× the storage and 27× the propagation work
//! for a difference nobody can see: light is smooth, and the mesher interpolates
//! between block samples at sub-node vertices anyway (Task 10's mode 2). The
//! shape of a chiselled block still matters — it decides whether light gets
//! *through* — which is the permeability rule below.
//!
//! # The permeability cache is a measured requirement
//!
//! Charter rule 19 and Contract §3: the face test **must** be cached rather than
//! recomputed per BFS visit. Task 02b measured the uncached test at ≈50%
//! overhead on a chiselled chunk. [`Faces`] is that cache.
//!
//! It is cached **per palette entry rather than per block**, which the contract
//! allows and which is strictly cheaper: permeability is a pure function of a
//! block's content, the chunk already stores content once per distinct value,
//! and the BFS already resolves a block index to its palette entry in order to
//! read it at all. A uniform chunk carries one byte instead of 4,096.

use crate::block::{BlockView, subnode_index};
use crate::material::MaterialId;

pub mod codec;
mod emission;
mod layer;
pub mod propagate;

pub use emission::Emissions;
pub use layer::LightLayer;
pub use propagate::{Neighbourhood, Region, edited, flood, relight};

/// The brightest a channel can be.
///
/// Four bits per channel, so 0..=15. Sunlight uses the same range and is scaled
/// by time of day at draw time rather than stored per level — see [`LightLayer`].
pub const MAX_LEVEL: u8 = 15;

/// How much a channel loses crossing one block.
pub const ATTENUATION: u8 = 1;

/// The six faces of a block, indexed `axis * 2 + positive`.
///
/// The same order the client's mesher uses. Two independent conventions for
/// "which face is 3" is a bug that only shows up as light leaking through one
/// side of the world, so there is one convention and this is it.
pub const FACE_COUNT: usize = 6;

/// Face index for the negative side of an axis.
#[must_use]
pub const fn face_negative(axis: usize) -> usize {
    axis * 2
}

/// Face index for the positive side of an axis.
#[must_use]
pub const fn face_positive(axis: usize) -> usize {
    axis * 2 + 1
}

/// The offset from a block to the neighbour across a face.
#[must_use]
pub const fn face_offset(face: usize) -> [i32; 3] {
    let axis = face / 2;
    let step = if face.is_multiple_of(2) { -1 } else { 1 };
    let mut offset = [0; 3];
    offset[axis] = step;
    offset
}

/// The face on the other side of the one given.
///
/// Light crossing between two blocks is tested from both sides (Contract §3),
/// and the neighbour's facing side is this one's opposite.
#[must_use]
pub const fn opposite(face: usize) -> usize {
    face ^ 1
}

/// A light level: sunlight plus a colour, packed into 16 bits.
///
/// # Layout
///
/// | bits | channel |
/// |---|---|
/// | 12..16 | sunlight |
/// | 8..12 | red |
/// | 4..8 | green |
/// | 0..4 | blue |
///
/// Sunlight is separate from the colour channels rather than being white block
/// light, because the two behave differently: sunlight is scaled by time of day
/// at draw time (a cave stays dark at noon and the surface dims at dusk, from
/// one stored field), and mode 3's shadow maps blend against the sunlight
/// channel alone.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Light(pub u16);

impl Light {
    /// Pitch black.
    pub const DARK: Self = Self(0);

    /// Full sunlight, no block light.
    pub const DAYLIGHT: Self = Self::new(MAX_LEVEL, 0, 0, 0);

    /// Packs four channels, each clamped to [`MAX_LEVEL`].
    #[must_use]
    pub const fn new(sun: u8, red: u8, green: u8, blue: u8) -> Self {
        Self(
            ((clamp(sun) as u16) << 12)
                | ((clamp(red) as u16) << 8)
                | ((clamp(green) as u16) << 4)
                | clamp(blue) as u16,
        )
    }

    /// The sunlight channel.
    #[must_use]
    pub const fn sun(self) -> u8 {
        (self.0 >> 12) as u8
    }

    /// The red channel.
    #[must_use]
    pub const fn red(self) -> u8 {
        ((self.0 >> 8) & 0xF) as u8
    }

    /// The green channel.
    #[must_use]
    pub const fn green(self) -> u8 {
        ((self.0 >> 4) & 0xF) as u8
    }

    /// The blue channel.
    #[must_use]
    pub const fn blue(self) -> u8 {
        (self.0 & 0xF) as u8
    }

    /// One channel by index: 0 sun, 1 red, 2 green, 3 blue.
    ///
    /// # Panics
    ///
    /// In debug builds, if `channel` is 4 or greater.
    #[must_use]
    pub const fn channel(self, channel: usize) -> u8 {
        debug_assert!(channel < CHANNELS);
        ((self.0 >> (12 - channel * 4)) & 0xF) as u8
    }

    /// This level with one channel replaced.
    ///
    /// # Panics
    ///
    /// In debug builds, if `channel` is 4 or greater.
    #[must_use]
    pub const fn with_channel(self, channel: usize, level: u8) -> Self {
        debug_assert!(channel < CHANNELS);
        let level = if level > MAX_LEVEL { MAX_LEVEL } else { level };
        let shift = 12 - channel * 4;
        Self((self.0 & !(0xF << shift)) | ((level as u16) << shift))
    }

    /// Whether every channel is zero.
    #[must_use]
    pub const fn is_dark(self) -> bool {
        self.0 == 0
    }

    /// The brighter of each channel, taken independently.
    ///
    /// Channel-wise rather than whole-value: a red lamp and a green lamp
    /// meeting produce yellow, not whichever `u16` happened to compare larger.
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        let mut out = Self::DARK;
        for channel in 0..CHANNELS {
            out = out.with_channel(channel, self.channel(channel).max(other.channel(channel)));
        }
        out
    }
}

/// How many channels a [`Light`] carries: sun, red, green, blue.
pub const CHANNELS: usize = 4;

/// A level held to [`MAX_LEVEL`].
///
/// A free function rather than a closure because [`Light::new`] is `const` and
/// const functions cannot call closures.
const fn clamp(level: u8) -> u8 {
    if level > MAX_LEVEL { MAX_LEVEL } else { level }
}

/// Which of a block's six faces let light through.
///
/// One bit per face, indexed `axis * 2 + positive`. **Computed when a block's
/// content is written and never during propagation** — charter rule 19.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Faces(pub u8);

impl Faces {
    /// Nothing gets through. A solid block.
    pub const OPAQUE: Self = Self(0);

    /// Every face passes light. Air.
    pub const OPEN: Self = Self(0b0011_1111);

    /// Whether light crosses this face.
    #[must_use]
    pub const fn passes(self, face: usize) -> bool {
        self.0 & (1 << face) != 0
    }

    /// Whether any face passes light.
    ///
    /// The cheap rejection the BFS leans on: a solid block is the common case
    /// and answering it costs one comparison.
    #[must_use]
    pub const fn any(self) -> bool {
        self.0 != 0
    }
}

/// Which faces of a block let light through, per Sub-Node Contract §3.
///
/// **The rule: light passes a face iff that face's 3×3 sub-node layer is not
/// fully occupied.** Only the nine cells adjacent to the face are considered,
/// so a block hollowed out in the middle but sealed on every side is correctly
/// opaque — and a block with one cell chiselled out of a face is not.
///
/// `Uniform(AIR)` is open on all six faces and any other `Uniform` is opaque on
/// all six, which are the two cases worth short-circuiting: between them they
/// are almost every block in a world.
#[must_use]
pub fn permeability(block: &BlockView<'_>) -> Faces {
    if let BlockView::Uniform(material) = block {
        return if material.is_air() {
            Faces::OPEN
        } else {
            Faces::OPAQUE
        };
    }

    let mut bits = 0u8;
    for face in 0..FACE_COUNT {
        if !layer_is_full(block, face) {
            bits |= 1 << face;
        }
    }
    Faces(bits)
}

/// Whether every cell of a face's 3×3 layer is occupied.
fn layer_is_full(block: &BlockView<'_>, face: usize) -> bool {
    let axis = face / 2;
    // The layer touching this face: 0 on a negative face, 2 on a positive one.
    let fixed = if face.is_multiple_of(2) { 0 } else { 2 };

    for a in 0..3u32 {
        for b in 0..3u32 {
            let (x, y, z) = match axis {
                0 => (fixed, a, b),
                1 => (a, fixed, b),
                _ => (a, b, fixed),
            };
            if block.subnode(subnode_index(x, y, z)) == MaterialId::AIR {
                return false;
            }
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{Cells, OCCUPANCY_FULL, SUBNODES_PER_BLOCK};

    const STONE: MaterialId = MaterialId(2);
    const GLASS: MaterialId = MaterialId(3);

    fn partial(occupancy: u32) -> BlockView<'static> {
        BlockView::Partial {
            material: STONE,
            occupancy,
        }
    }

    #[test]
    fn a_channel_survives_a_round_trip_at_every_level() {
        for level in 0..=MAX_LEVEL {
            let light = Light::new(level, MAX_LEVEL - level, level, MAX_LEVEL - level);
            assert_eq!(light.sun(), level);
            assert_eq!(light.red(), MAX_LEVEL - level);
            assert_eq!(light.green(), level);
            assert_eq!(light.blue(), MAX_LEVEL - level);
        }
    }

    #[test]
    fn the_channels_do_not_bleed_into_each_other() {
        // The bug this catches is a shift or a mask off by four bits, which
        // looks like a lamp tinting the sky.
        let only_blue = Light::new(0, 0, 0, MAX_LEVEL);
        assert_eq!(only_blue.sun(), 0);
        assert_eq!(only_blue.red(), 0);
        assert_eq!(only_blue.green(), 0);
        assert_eq!(only_blue.blue(), MAX_LEVEL);

        let only_sun = Light::new(MAX_LEVEL, 0, 0, 0);
        assert_eq!(only_sun.0 & 0x0FFF, 0, "sunlight leaked into a colour");
    }

    #[test]
    fn channel_indexing_agrees_with_the_named_accessors() {
        let light = Light::new(1, 2, 3, 4);
        assert_eq!(light.channel(0), light.sun());
        assert_eq!(light.channel(1), light.red());
        assert_eq!(light.channel(2), light.green());
        assert_eq!(light.channel(3), light.blue());
    }

    #[test]
    fn brightest_is_taken_per_channel_and_not_per_word() {
        // A red lamp meeting a green one makes yellow. Comparing the packed
        // u16 would pick one lamp and discard the other, which reads as a
        // light that switches colour as you walk between two sources.
        let red = Light::new(0, MAX_LEVEL, 0, 0);
        let green = Light::new(0, 0, MAX_LEVEL, 0);
        let both = red.max(green);
        assert_eq!(both.red(), MAX_LEVEL);
        assert_eq!(both.green(), MAX_LEVEL);
        assert_eq!(both.blue(), 0);
    }

    #[test]
    fn air_is_open_and_solid_is_opaque() {
        assert_eq!(
            permeability(&BlockView::Uniform(MaterialId::AIR)),
            Faces::OPEN
        );
        assert_eq!(permeability(&BlockView::Uniform(STONE)), Faces::OPAQUE);
        assert!(!permeability(&BlockView::Uniform(STONE)).any());
    }

    #[test]
    fn a_full_partial_mask_is_as_opaque_as_a_solid_block() {
        // `Partial` with every bit set is the same geometry as `Uniform`, and
        // the two must agree — otherwise light leaks through blocks depending
        // on how they were built rather than on their shape.
        let full = partial(OCCUPANCY_FULL);
        assert_eq!(permeability(&full), Faces::OPAQUE);
    }

    #[test]
    fn one_cell_missing_from_a_face_opens_that_face_and_no_other() {
        // Contract §3: the test is per face, on that face's nine cells. Cell
        // (0, 0, 0) is in the negative layer of all three axes, so exactly the
        // three negative faces open.
        let holed = partial(OCCUPANCY_FULL & !1);
        let faces = permeability(&holed);

        assert!(faces.passes(face_negative(0)), "-x should pass");
        assert!(faces.passes(face_negative(1)), "-y should pass");
        assert!(faces.passes(face_negative(2)), "-z should pass");
        assert!(!faces.passes(face_positive(0)), "+x should not");
        assert!(!faces.passes(face_positive(1)), "+y should not");
        assert!(!faces.passes(face_positive(2)), "+z should not");
    }

    #[test]
    fn a_block_hollow_in_the_middle_but_sealed_outside_is_opaque() {
        // The case the contract calls out by name, and the reason the test
        // looks at nine cells rather than at the occupancy count. Cell
        // (1, 1, 1) is the centre and touches no face.
        let centre = subnode_index(1, 1, 1);
        let hollow = partial(OCCUPANCY_FULL & !(1 << centre));
        assert_eq!(
            permeability(&hollow),
            Faces::OPAQUE,
            "a sealed block leaked light because its middle was empty"
        );
    }

    #[test]
    fn a_mixed_block_is_judged_by_occupancy_and_not_by_material() {
        // Two materials, no air: opaque. The rule is about occupancy, so a
        // block of stone and glass is as opaque as a block of stone — glass
        // being transparent is a rendering matter, not a light-propagation one,
        // and pretending otherwise here would make light depend on a material
        // property the contract does not give it.
        let mut cells: Cells = [STONE; SUBNODES_PER_BLOCK];
        cells[0] = GLASS;
        assert_eq!(permeability(&BlockView::Mixed(&cells)), Faces::OPAQUE);

        // And the same block with one cell of air is open on the faces that
        // cell touches, which is what makes the assertion above non-vacuous.
        let mut holed = cells;
        holed[0] = MaterialId::AIR;
        assert!(permeability(&BlockView::Mixed(&holed)).any());
    }

    #[test]
    fn faces_and_their_offsets_agree_with_opposites() {
        for face in 0..FACE_COUNT {
            let offset = face_offset(face);
            let back = face_offset(opposite(face));
            assert_eq!(
                [-offset[0], -offset[1], -offset[2]],
                back,
                "face {face} and its opposite do not point opposite ways"
            );
        }
        assert_eq!(face_offset(face_negative(1)), [0, -1, 0]);
        assert_eq!(face_offset(face_positive(1)), [0, 1, 0]);
    }
}
