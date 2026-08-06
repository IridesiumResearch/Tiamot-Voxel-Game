// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Which materials glow, and what a block made of them emits.

use crate::block::BlockView;
use crate::material::MaterialId;

use super::Light;

/// What each material emits, by numeric id.
///
/// Built once when the registries freeze and read constantly afterwards, so it
/// is a flat `Vec` indexed by id rather than a map: material ids are dense and
/// assigned in registration order, and a hash lookup per block visited during a
/// flood would cost more than the flood.
///
/// **Both ends hold one.** The server builds it from `register_block`, and the
/// client is told the same thing in the material table it already receives —
/// otherwise client-side propagation would agree with the server about geometry
/// and disagree about lamps, which is worse than not predicting light at all.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Emissions {
    by_id: Vec<Light>,
}

impl Emissions {
    /// Builds a table from `(id, emission)` pairs.
    ///
    /// Ids need not be contiguous or sorted; anything unnamed emits nothing.
    #[must_use]
    pub fn new(entries: impl IntoIterator<Item = (MaterialId, Light)>) -> Self {
        let mut by_id: Vec<Light> = Vec::new();
        for (id, level) in entries {
            let index = id.0 as usize;
            if by_id.len() <= index {
                by_id.resize(index + 1, Light::DARK);
            }
            by_id[index] = level;
        }
        Self { by_id }
    }

    /// What one material emits.
    #[must_use]
    pub fn of(&self, material: MaterialId) -> Light {
        self.by_id
            .get(material.0 as usize)
            .copied()
            .unwrap_or(Light::DARK)
    }

    /// Whether anything in the table emits at all.
    ///
    /// A world whose mods registered no lamp can skip the emissive seeding pass
    /// entirely, and that is the common case for a mod set that only defines
    /// terrain.
    #[must_use]
    pub fn any(&self) -> bool {
        self.by_id.iter().any(|level| !level.is_dark())
    }

    /// What a whole block emits.
    ///
    /// **The brightest of its materials, per channel.** A block is one light
    /// source however many cells it has, so a lamp chiselled down to a single
    /// sub-node still glows — dimming it in proportion to what was carved away
    /// would make a chisel a dimmer switch, which is a game design decision the
    /// engine has no business making.
    ///
    /// Per channel rather than per material, for the same reason [`Light::max`]
    /// is: a block holding a red lamp cell and a green one glows yellow.
    #[must_use]
    pub fn block(&self, block: &BlockView<'_>) -> Light {
        match block {
            // One material, so occupancy plays no part: a lamp chiselled to a
            // sliver is the same lamp.
            BlockView::Uniform(material) | BlockView::Partial { material, .. } => {
                self.of(*material)
            }
            BlockView::Mixed(cells) => {
                let mut out = Light::DARK;
                for material in *cells {
                    let level = self.of(*material);
                    if !level.is_dark() {
                        out = out.max(level);
                    }
                }
                out
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::block::{Cells, OCCUPANCY_FULL, SUBNODES_PER_BLOCK};
    use crate::light::MAX_LEVEL;

    const STONE: MaterialId = MaterialId(2);
    const RED_LAMP: MaterialId = MaterialId(7);
    const GREEN_LAMP: MaterialId = MaterialId(9);

    fn table() -> Emissions {
        Emissions::new([
            (RED_LAMP, Light::new(0, MAX_LEVEL, 0, 0)),
            (GREEN_LAMP, Light::new(0, 0, MAX_LEVEL, 0)),
        ])
    }

    #[test]
    fn an_unregistered_material_emits_nothing() {
        let table = table();
        assert!(table.of(STONE).is_dark());
        assert!(table.of(MaterialId::AIR).is_dark());
        // Including an id past the end of the table, which is what a client
        // sees if it is told about fewer materials than the ids in a chunk.
        assert!(table.of(MaterialId(60_000)).is_dark());
    }

    #[test]
    fn an_empty_table_knows_it_has_nothing_to_do() {
        assert!(!Emissions::default().any());
        assert!(table().any());
    }

    #[test]
    fn a_lamp_chiselled_to_one_cell_still_glows_at_full_strength() {
        // The alternative — scaling emission by occupancy — turns a chisel into
        // a dimmer switch. That may be a fine rule for some game, and it is a
        // mod's rule to make, not the engine's.
        let table = table();
        let whole = BlockView::Uniform(RED_LAMP);
        let sliver = BlockView::Partial {
            material: RED_LAMP,
            occupancy: 1,
        };
        assert_eq!(table.block(&whole), table.block(&sliver));
        assert_eq!(table.block(&sliver).red(), MAX_LEVEL);
        // And a full mask is the same block again, which pins that occupancy
        // plays no part at all.
        let full = BlockView::Partial {
            material: RED_LAMP,
            occupancy: OCCUPANCY_FULL,
        };
        assert_eq!(table.block(&full), table.block(&whole));
    }

    #[test]
    fn a_mixed_block_glows_with_every_lamp_in_it() {
        let table = table();
        let mut cells: Cells = [STONE; SUBNODES_PER_BLOCK];
        cells[0] = RED_LAMP;
        cells[26] = GREEN_LAMP;

        let level = table.block(&BlockView::Mixed(&cells));
        assert_eq!(level.red(), MAX_LEVEL);
        assert_eq!(level.green(), MAX_LEVEL, "the second lamp was ignored");
        assert_eq!(level.blue(), 0);

        // Counter-example: the same block with no lamp in it is dark, so the
        // assertion above is about the lamps and not about Mixed blocks.
        let plain: Cells = [STONE; SUBNODES_PER_BLOCK];
        assert!(table.block(&BlockView::Mixed(&plain)).is_dark());
    }
}
