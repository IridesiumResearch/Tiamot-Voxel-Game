// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Flood-filling light through blocks.
//!
//! # One implementation, two places
//!
//! The server is authoritative for light, and the client runs the same code on
//! the edits it receives — otherwise a chunk could not be remeshed until the
//! server got round to telling it what the new light was, and every block broken
//! would flash dark for a round trip. Charter rule 2 is not in tension with
//! this: the server's answer is the truth, and the client's is a prediction of
//! it that happens to be exact because it runs the same function over the same
//! data.
//!
//! That is why propagation is written against [`Neighbourhood`] rather than
//! against a chunk store. The server has a world database behind a cache and the
//! client has a map of streamed chunks; neither is the other, and the algorithm
//! does not care.
//!
//! # An unloaded block is opaque
//!
//! The same choice physics makes for collision, for the same reason: absence is
//! not air. Treating what has not arrived as transparent would let sunlight pour
//! in through the side of the loaded region and light the inside of a mountain
//! whose middle had not been streamed yet. It also gives propagation its bound —
//! a flood stops at the edge of what is loaded rather than walking to the edge
//! of a 120,000-block world.
//!
//! # Determinism
//!
//! Charter rule 4. The queue is a `VecDeque`, faces are visited in a fixed
//! order, and nothing here iterates a `HashMap`. Light is integer arithmetic
//! throughout, so there is no float subset to stay inside — but the *order* of
//! writes still has to be identical on every platform, because the result is
//! hashed by the determinism gate.

use std::collections::VecDeque;

use crate::coords::BlockPos;

use super::{ATTENUATION, CHANNELS, FACE_COUNT, Faces, Light, MAX_LEVEL, face_offset, opposite};

/// The world, as light propagation needs to see it.
///
/// Implemented over whatever holds chunks — a server world, a client's streamed
/// store, or a test fixture.
pub trait Neighbourhood {
    /// Which faces of the block at `pos` let light through.
    ///
    /// `None` for a block that is not loaded, which is treated as opaque. This
    /// should be the **cached** answer ([`crate::chunk::Chunk::faces`]) and
    /// never a recomputed face test — charter rule 19.
    fn faces(&self, pos: BlockPos) -> Option<Faces>;

    /// Light the block at `pos` emits of its own accord.
    ///
    /// [`Light::DARK`] for anything that is not a lamp, which is almost
    /// everything.
    fn emission(&self, pos: BlockPos) -> Light;

    /// The light currently recorded at `pos`.
    fn light(&self, pos: BlockPos) -> Light;

    /// Records light at `pos`.
    ///
    /// Positions outside what the implementation holds are dropped rather than
    /// being an error: a flood reaching the edge of the loaded region has
    /// nowhere to write, and that is the bound working rather than a failure.
    fn set_light(&mut self, pos: BlockPos, level: Light);
}

/// An inclusive box of blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Region {
    /// Lowest corner.
    pub min: BlockPos,
    /// Highest corner, inclusive.
    pub max: BlockPos,
}

impl Region {
    /// The blocks just outside the region, in a fixed order.
    ///
    /// **The boundary condition for a bounded relight.** A region is relit by
    /// clearing it and re-seeding, but the light coming *into* it from outside
    /// has sources that are not in it — daylight descending from the chunk
    /// above, a lamp in the chunk next door. Those blocks keep their light and
    /// flood inward, which is what makes relighting one chunk give the same
    /// answer as relighting the world.
    ///
    /// Six slabs, one per face, walked in a fixed order. Corners and edges
    /// appear once each because the slabs are trimmed to be disjoint — a
    /// duplicate would only cost a wasted queue entry, but the order has to be
    /// deterministic and "sometimes twice" is harder to reason about than not.
    fn border(self) -> impl Iterator<Item = BlockPos> {
        let Self { min, max } = self;
        let low = BlockPos::new(min.x - 1, min.y - 1, min.z - 1);
        let high = BlockPos::new(max.x + 1, max.y + 1, max.z + 1);

        // The two y slabs take the full x/z extent; the z slabs take the full
        // x extent but only the interior y; the x slabs take only interior y
        // and z. That covers the shell exactly once.
        let y_slabs = [low.y, high.y].into_iter().flat_map(move |y| {
            (low.z..=high.z)
                .flat_map(move |z| (low.x..=high.x).map(move |x| BlockPos::new(x, y, z)))
        });
        let z_slabs = [low.z, high.z].into_iter().flat_map(move |z| {
            (min.y..=max.y).flat_map(move |y| (low.x..=high.x).map(move |x| BlockPos::new(x, y, z)))
        });
        let x_slabs = [low.x, high.x].into_iter().flat_map(move |x| {
            (min.y..=max.y).flat_map(move |y| (min.z..=max.z).map(move |z| BlockPos::new(x, y, z)))
        });
        y_slabs.chain(z_slabs).chain(x_slabs)
    }

    /// Every block in the region, in a fixed order.
    ///
    /// Y outermost so that a caller walking columns sees each column's blocks
    /// consecutively from the top down, which is the order sunlight wants.
    fn blocks(self) -> impl Iterator<Item = BlockPos> {
        let Self { min, max } = self;
        (min.y..=max.y).flat_map(move |y| {
            (min.z..=max.z).flat_map(move |z| (min.x..=max.x).map(move |x| BlockPos::new(x, y, z)))
        })
    }

    /// Whether a position is inside.
    #[must_use]
    pub const fn contains(self, pos: BlockPos) -> bool {
        pos.x >= self.min.x
            && pos.x <= self.max.x
            && pos.y >= self.min.y
            && pos.y <= self.max.y
            && pos.z >= self.min.z
            && pos.z <= self.max.z
    }
}

/// Whether the sky shines directly onto this block.
///
/// **The sky is wherever the loaded world ends.** Nothing loaded above a block
/// means nothing between it and space, so it gets full daylight. That follows
/// from "an unloaded block is opaque" being about *blocking* rather than about
/// what is above the top of the world, and it is what makes the incremental
/// path and a full relight agree: both ask this one question rather than one of
/// them seeding a region's top layer and the other not knowing where the top
/// was. A divergence there is a seam of permanently dim blocks along whichever
/// edge happened to be edited.
fn sky_reaches(world: &impl Neighbourhood, pos: BlockPos) -> bool {
    let above = BlockPos::new(pos.x, pos.y + 1, pos.z);
    if world.faces(above).is_some() {
        return false;
    }
    world
        .faces(pos)
        .is_some_and(|faces| faces.passes(super::face_positive(1)))
}

/// Whether light at `level` crossing from `from` into its neighbour survives.
///
/// **Both facing layers are tested** — Sub-Node Contract §3 says so explicitly,
/// and the asymmetric case is real: a block open on its `+x` face against a
/// neighbour sealed on its `-x` face passes nothing.
fn crosses(world: &impl Neighbourhood, from: BlockPos, face: usize) -> Option<BlockPos> {
    let here = world.faces(from)?;
    if !here.passes(face) {
        return None;
    }
    let offset = face_offset(face);
    let there = BlockPos::new(from.x + offset[0], from.y + offset[1], from.z + offset[2]);
    let neighbour = world.faces(there)?;
    neighbour.passes(opposite(face)).then_some(there)
}

/// The level one channel arrives at across a face.
///
/// **Sunlight falling straight down does not attenuate**, which is what makes an
/// open shaft lit to its floor rather than fading out after fifteen blocks. Every
/// other direction, and every other channel, loses [`ATTENUATION`] per block.
fn arriving(channel: usize, level: u8, face: usize) -> u8 {
    const SUN: usize = 0;
    const DOWN: usize = 2; // face_negative(1)

    if channel == SUN && face == DOWN && level == MAX_LEVEL {
        return MAX_LEVEL;
    }
    level.saturating_sub(ATTENUATION)
}

/// Seeds a block's own emission, and pushes it into its neighbours.
///
/// **An emitter's light ignores the emitter's own faces and respects its
/// neighbours'.** Without this a solid lamp lights nothing at all: the
/// permeability rule makes a full block opaque on every face (Contract §3), so
/// its own glow would be sealed inside it, and `light_emit` would only ever
/// work on blocks somebody had chiselled first.
///
/// The physical reading is that a block glows on its *surface* rather than in
/// its middle. A lamp against open air lights the air; a lamp walled in on every
/// side still lights nothing, because the neighbours' facing layers stop it.
fn seed_emission(world: &mut impl Neighbourhood, pos: BlockPos, queue: &mut VecDeque<BlockPos>) {
    let emission = world.emission(pos);
    if emission.is_dark() {
        return;
    }

    let here = world.light(pos).max(emission);
    world.set_light(pos, here);
    queue.push_back(pos);

    for face in 0..FACE_COUNT {
        let Some(next) = removal_step(world, pos, face) else {
            continue;
        };
        let current = world.light(next);
        let mut updated = current;
        for channel in 0..CHANNELS {
            let arrives = arriving(channel, emission.channel(channel), face);
            if arrives > updated.channel(channel) {
                updated = updated.with_channel(channel, arrives);
            }
        }
        if updated != current {
            world.set_light(next, updated);
            queue.push_back(next);
        }
    }
}

/// Floods light outward from everything already in `queue`.
///
/// The queue holds positions whose light has just increased and whose
/// neighbours may therefore be too dim. Each is compared against its six
/// neighbours channel by channel; a neighbour that would be brighter is updated
/// and queued in turn. A position can be queued more than once — that is normal
/// for a BFS over four independent channels, and the level comparison makes
/// re-visits cheap and idempotent.
pub fn flood(world: &mut impl Neighbourhood, queue: &mut VecDeque<BlockPos>) {
    while let Some(pos) = queue.pop_front() {
        let level = world.light(pos);
        if level.is_dark() {
            continue;
        }

        for face in 0..FACE_COUNT {
            let Some(next) = crosses(world, pos, face) else {
                continue;
            };

            let current = world.light(next);
            let mut updated = current;
            for channel in 0..CHANNELS {
                let arrives = arriving(channel, level.channel(channel), face);
                if arrives > updated.channel(channel) {
                    updated = updated.with_channel(channel, arrives);
                }
            }

            if updated != current {
                world.set_light(next, updated);
                queue.push_back(next);
            }
        }
    }
}

/// Recomputes every light level in `region` from scratch.
///
/// Clears the region, seeds it, and floods. Seeds are of two kinds:
///
/// - **emissive blocks**, anywhere in the region;
/// - **sunlight**, entering through the top face of the region's topmost layer.
///
/// Sunlight is seeded at the top rather than descended from the sky because the
/// region is what is loaded, and the caller decides how far up that reaches. A
/// server relighting a column that genuinely reaches open sky passes a region
/// whose top is above the terrain; one relighting a slice deep underground
/// passes a region whose top is solid, and no sunlight is seeded at all —
/// which is the correct answer for a cave either way.
///
/// **The stored sunlight is full daylight, not the current time of day.** Time
/// of day scales it at draw time (Task 10's day/night design), so dusk does not
/// dirty a single chunk — otherwise every chunk in the world would need
/// relighting twenty times a second, which is a fair description of how not to
/// build this.
pub fn relight(world: &mut impl Neighbourhood, region: Region) {
    let mut queue = VecDeque::new();

    for pos in region.blocks() {
        world.set_light(pos, Light::DARK);
    }

    // **The blocks around the region are sources, not part of it.** They keep
    // whatever light they have and flood inward. Clearing them too — which is
    // what relighting a region with a margin amounts to — destroys the daylight
    // descending from the chunk above and the light from the lamp next door,
    // and the region comes back lit only by whatever it contains itself. The
    // symptom is a freshly loaded chunk under open sky that is almost, but not
    // quite, dark.
    for pos in region.border() {
        if !world.light(pos).is_dark() {
            queue.push_back(pos);
        }
    }

    for pos in region.blocks() {
        seed_emission(world, pos, &mut queue);
    }

    for pos in region.blocks() {
        if sky_reaches(world, pos) {
            let level = world.light(pos).max(Light::DAYLIGHT);
            world.set_light(pos, level);
            queue.push_back(pos);
        }
    }

    flood(world, &mut queue);
}

/// Re-lights the neighbourhood of a block whose content just changed.
///
/// The incremental half of lighting, and the one that runs during play: a
/// player breaks a block or places a lamp, and the world has to be right again
/// this tick without relighting a region.
///
/// # The two queues
///
/// Adding light is easy — set it and flood. **Removing it is the hard half**,
/// and the reason for the standard two-queue algorithm. When a lamp goes out,
/// the levels around it are still there and nothing in a plain flood will lower
/// them: a flood only ever brightens. So removal walks outward clearing
/// everything that could only have come from the source, collecting as it goes
/// any block that is brighter than this source could have made it — those have
/// another source, and they seed the flood that fills the hole back in.
///
/// # Per channel, not per block
///
/// A block can dim in red and keep its green, so removal runs per channel.
/// Doing it per block would clear a whole colour because one component of it
/// went away, and the symptom is a room turning grey when one of two lamps is
/// broken.
pub fn edited(world: &mut impl Neighbourhood, pos: BlockPos) {
    let before = world.light(pos);
    // Whatever it emits now — nothing, if a lamp just became rubble — plus the
    // sky, if the edit opened this block to it. Asking the same question
    // `relight` asks is what keeps the two agreeing; an incremental path that
    // only ever refilled from neighbours would leave a newly opened block one
    // level short of the daylight a relight gives it.
    let mut after = world.emission(pos);
    if sky_reaches(world, pos) {
        after = after.max(Light::DAYLIGHT);
    }
    world.set_light(pos, after);
    let became_emissive = !after.is_dark();

    let mut refill = VecDeque::new();
    for channel in 0..CHANNELS {
        if before.channel(channel) > after.channel(channel) {
            remove(world, pos, channel, before.channel(channel), &mut refill);
        }
    }

    // The block itself may now let light in where it did not before — a broken
    // block is a hole — so its neighbours are seeds too. Cheap, and it is the
    // difference between a mined tunnel lighting up and staying black.
    for face in 0..FACE_COUNT {
        if let Some(next) = removal_step(world, pos, face) {
            refill.push_back(next);
        }
    }
    if became_emissive {
        // Through the same door emission uses in `relight`: a lamp placed as a
        // solid block would otherwise light nothing, because its own faces are
        // shut.
        seed_emission(world, pos, &mut refill);
    }
    if !world.light(pos).is_dark() {
        refill.push_back(pos);
    }

    flood(world, &mut refill);
}

/// The neighbour across a face, for a removal walk.
///
/// **Deliberately does not test the source's own permeability, only the
/// neighbour's.** Removal starts from a block that has just changed, and the
/// commonest change is becoming solid — a roof being built. Testing the new
/// state would find every face closed, so the walk would never leave the block
/// and the light it used to pass would stay exactly where it was. The symptom
/// is a shaft still lit to the floor under a roof you just placed.
///
/// Interior steps are unaffected: only lit blocks are queued, and an opaque
/// block is never lit, so nothing walks *through* solid rock by this route.
fn removal_step(world: &impl Neighbourhood, from: BlockPos, face: usize) -> Option<BlockPos> {
    let offset = face_offset(face);
    let there = BlockPos::new(from.x + offset[0], from.y + offset[1], from.z + offset[2]);
    world.faces(there)?.passes(opposite(face)).then_some(there)
}

/// Clears one channel outward from a source that has dimmed.
///
/// `had` is the level the source used to be. Anything a neighbour could have
/// received from it is cleared and followed; anything brighter has its own
/// source and is queued for the flood that follows.
fn remove(
    world: &mut impl Neighbourhood,
    from: BlockPos,
    channel: usize,
    had: u8,
    refill: &mut VecDeque<BlockPos>,
) {
    let mut queue = VecDeque::from([(from, had)]);

    while let Some((pos, had)) = queue.pop_front() {
        for face in 0..FACE_COUNT {
            let Some(next) = removal_step(world, pos, face) else {
                continue;
            };
            let level = world.light(next).channel(channel);
            if level == 0 {
                continue;
            }

            // Compared against what this source WOULD have given it, not merely
            // against being dimmer. Sunlight falling straight down does not
            // attenuate, so a block below a removed sky source has the same
            // level rather than a lower one — a "strictly dimmer" test leaves
            // the whole column lit under a roof that was just built.
            if level <= arriving(channel, had, face) {
                world.set_light(next, world.light(next).with_channel(channel, 0));
                queue.push_back((next, level));
            } else {
                // Brighter than this source could explain, so something else is
                // lighting it. It fills the hole back in.
                refill.push_back(next);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;
    use crate::block::BlockView;
    use crate::material::MaterialId;

    const STONE: MaterialId = MaterialId(2);

    /// A dense box of blocks, for exercising propagation without a world.
    struct Box3 {
        region: Region,
        solid: BTreeMap<(i32, i32, i32), bool>,
        emitters: BTreeMap<(i32, i32, i32), Light>,
        light: BTreeMap<(i32, i32, i32), Light>,
        /// Blocks whose faces are only partly open, by face mask.
        masked: BTreeMap<(i32, i32, i32), Faces>,
    }

    impl Box3 {
        fn new(min: BlockPos, max: BlockPos) -> Self {
            Self {
                region: Region { min, max },
                solid: BTreeMap::new(),
                emitters: BTreeMap::new(),
                light: BTreeMap::new(),
                masked: BTreeMap::new(),
            }
        }

        fn fill(&mut self, min: BlockPos, max: BlockPos) {
            for y in min.y..=max.y {
                for z in min.z..=max.z {
                    for x in min.x..=max.x {
                        self.solid.insert((x, y, z), true);
                    }
                }
            }
        }

        fn set_solid(&mut self, pos: BlockPos, solid: bool) {
            self.solid.insert((pos.x, pos.y, pos.z), solid);
        }

        fn lamp(&mut self, pos: BlockPos, level: Light) {
            self.emitters.insert((pos.x, pos.y, pos.z), level);
        }

        fn at(&self, pos: BlockPos) -> Light {
            self.light
                .get(&(pos.x, pos.y, pos.z))
                .copied()
                .unwrap_or(Light::DARK)
        }
    }

    impl Neighbourhood for Box3 {
        fn faces(&self, pos: BlockPos) -> Option<Faces> {
            if !self.region.contains(pos) {
                return None;
            }
            if let Some(faces) = self.masked.get(&(pos.x, pos.y, pos.z)) {
                return Some(*faces);
            }
            let solid = self
                .solid
                .get(&(pos.x, pos.y, pos.z))
                .copied()
                .unwrap_or(false);
            Some(if solid { Faces::OPAQUE } else { Faces::OPEN })
        }

        fn emission(&self, pos: BlockPos) -> Light {
            self.emitters
                .get(&(pos.x, pos.y, pos.z))
                .copied()
                .unwrap_or(Light::DARK)
        }

        fn light(&self, pos: BlockPos) -> Light {
            self.at(pos)
        }

        fn set_light(&mut self, pos: BlockPos, level: Light) {
            if self.region.contains(pos) {
                self.light.insert((pos.x, pos.y, pos.z), level);
            }
        }
    }

    fn open_box(size: i32) -> Box3 {
        Box3::new(BlockPos::new(0, 0, 0), BlockPos::new(size, size, size))
    }

    #[test]
    fn open_sky_lights_a_column_all_the_way_down_without_fading() {
        // The rule that makes daylight look like daylight. Attenuating downward
        // would leave the floor of an open pit dimmer than its rim, which is
        // not how the sky works.
        let mut world = open_box(20);
        let region = world.region;
        relight(&mut world, region);

        for y in 0..=20 {
            assert_eq!(
                world.at(BlockPos::new(5, y, 5)).sun(),
                MAX_LEVEL,
                "sunlight faded at y = {y} in an open column"
            );
        }
    }

    #[test]
    fn a_roof_puts_everything_under_it_in_the_dark() {
        // The counter-example to the test above: without it, "full sunlight
        // everywhere" would pass both.
        let mut world = open_box(20);
        world.fill(BlockPos::new(0, 10, 0), BlockPos::new(20, 10, 20));
        let region = world.region;
        relight(&mut world, region);

        assert_eq!(world.at(BlockPos::new(5, 11, 5)).sun(), MAX_LEVEL);
        assert_eq!(
            world.at(BlockPos::new(5, 9, 5)).sun(),
            0,
            "sunlight came through a solid roof"
        );
        assert_eq!(world.at(BlockPos::new(5, 0, 5)).sun(), 0);
    }

    #[test]
    fn sunlight_reaches_sideways_under_an_overhang_but_attenuates() {
        // Lateral spread IS attenuated, which is what gives the shaded side of
        // an overhang a gradient rather than a hard edge.
        let mut world = Box3::new(BlockPos::new(0, 0, 0), BlockPos::new(40, 20, 20));
        // A roof over most of the box, open at x > 30.
        world.fill(BlockPos::new(0, 10, 0), BlockPos::new(30, 10, 20));
        let region = world.region;
        relight(&mut world, region);

        // Directly under the open half: full daylight.
        assert_eq!(world.at(BlockPos::new(32, 9, 5)).sun(), MAX_LEVEL);
        // One block under the roof: dimmer, but lit.
        let just_inside = world.at(BlockPos::new(30, 9, 5)).sun();
        assert!(
            just_inside > 0 && just_inside < MAX_LEVEL,
            "under the lip of an overhang should be partly lit, got {just_inside}"
        );
        // The gradient is one level per block, so daylight reaches exactly
        // MAX_LEVEL blocks in and no further. Deep under the roof is dark.
        assert_eq!(
            world
                .at(BlockPos::new(30 - i32::from(MAX_LEVEL), 9, 5))
                .sun(),
            0,
            "sunlight travelled further sideways than one level per block allows"
        );
        assert_eq!(world.at(BlockPos::new(0, 9, 5)).sun(), 0);
    }

    #[test]
    fn a_lamp_lights_its_surroundings_in_its_own_colour() {
        let mut world = open_box(20);
        // Sealed room so sunlight does not drown the lamp.
        world.fill(BlockPos::new(0, 0, 0), BlockPos::new(20, 20, 20));
        for y in 2..=8 {
            for z in 2..=8 {
                for x in 2..=8 {
                    world.set_solid(BlockPos::new(x, y, z), false);
                }
            }
        }
        world.lamp(BlockPos::new(5, 5, 5), Light::new(0, MAX_LEVEL, 0, 0));
        let region = world.region;
        relight(&mut world, region);

        let source = world.at(BlockPos::new(5, 5, 5));
        assert_eq!(source.red(), MAX_LEVEL);
        assert_eq!(source.green(), 0, "a red lamp emitted green");
        assert_eq!(source.sun(), 0, "a sealed room saw sunlight");

        // One block away, one level down.
        assert_eq!(world.at(BlockPos::new(6, 5, 5)).red(), MAX_LEVEL - 1);
        assert_eq!(world.at(BlockPos::new(7, 5, 5)).red(), MAX_LEVEL - 2);
    }

    #[test]
    fn a_solid_lamp_lights_the_air_beside_it() {
        // **The rule a strict reading of Contract §3 would get wrong.** A full
        // block is opaque on every face, so an emitter's own glow would be
        // sealed inside it and `light_emit` would only work on blocks somebody
        // had chiselled first. An emitter ignores its own faces and respects
        // its neighbours'.
        let mut world = open_box(12);
        world.fill(BlockPos::new(0, 0, 0), BlockPos::new(12, 12, 12));
        for x in 4..=10 {
            world.set_solid(BlockPos::new(x, 6, 6), false);
        }
        // The lamp itself stays SOLID, which is the whole point.
        let lamp = BlockPos::new(4, 6, 6);
        world.set_solid(lamp, true);
        world.lamp(lamp, Light::new(0, MAX_LEVEL, 0, 0));
        let region = world.region;
        relight(&mut world, region);

        assert_eq!(
            world.at(BlockPos::new(5, 6, 6)).red(),
            MAX_LEVEL - 1,
            "a solid lamp lit nothing, so its glow was sealed inside it"
        );
        assert_eq!(world.at(BlockPos::new(6, 6, 6)).red(), MAX_LEVEL - 2);
    }

    #[test]
    fn a_lamp_walled_in_on_every_side_lights_nothing() {
        // The counter-example that keeps the rule above honest: an emitter
        // ignores its OWN faces, not its neighbours'. Otherwise a lamp buried
        // in rock would glow through it.
        let mut world = open_box(12);
        world.fill(BlockPos::new(0, 0, 0), BlockPos::new(12, 12, 12));
        let lamp = BlockPos::new(6, 6, 6);
        world.lamp(lamp, Light::new(0, MAX_LEVEL, 0, 0));
        let region = world.region;
        relight(&mut world, region);

        assert_eq!(
            world.at(BlockPos::new(7, 6, 6)).red(),
            0,
            "a lamp buried in solid rock lit the rock next to it"
        );
    }

    #[test]
    fn two_lamps_of_different_colours_mix_rather_than_replace() {
        let mut world = open_box(20);
        world.fill(BlockPos::new(0, 0, 0), BlockPos::new(20, 20, 20));
        for x in 2..=10 {
            world.set_solid(BlockPos::new(x, 5, 5), false);
        }
        world.lamp(BlockPos::new(2, 5, 5), Light::new(0, MAX_LEVEL, 0, 0));
        world.lamp(BlockPos::new(10, 5, 5), Light::new(0, 0, MAX_LEVEL, 0));
        let region = world.region;
        relight(&mut world, region);

        let middle = world.at(BlockPos::new(6, 5, 5));
        assert!(
            middle.red() > 0 && middle.green() > 0,
            "between a red lamp and a green one should be both, got {middle:?}"
        );
    }

    #[test]
    fn light_stops_at_a_sealed_face_and_passes_a_chiselled_one() {
        // Contract §3 through the propagation loop rather than in isolation:
        // the permeability byte is what decides, so a block that is solid to
        // its neighbour blocks light even though the neighbour is open.
        let mut world = open_box(10);
        world.fill(BlockPos::new(0, 0, 0), BlockPos::new(10, 10, 10));
        for x in 1..=8 {
            world.set_solid(BlockPos::new(x, 5, 5), false);
        }
        // A wall at x = 5 that is open on every face EXCEPT the one facing the
        // lamp. Light must not get through it.
        world.set_solid(BlockPos::new(5, 5, 5), true);
        world.masked.insert(
            (5, 5, 5),
            Faces(Faces::OPEN.0 & !(1 << super::super::face_negative(0))),
        );
        world.lamp(BlockPos::new(1, 5, 5), Light::new(0, MAX_LEVEL, 0, 0));
        let region = world.region;
        relight(&mut world, region);

        assert!(
            world.at(BlockPos::new(4, 5, 5)).red() > 0,
            "light did not reach the near side of the wall"
        );
        assert_eq!(
            world.at(BlockPos::new(6, 5, 5)).red(),
            0,
            "light crossed a face the block seals"
        );
    }

    #[test]
    fn relighting_part_of_a_world_keeps_the_light_coming_into_it() {
        // **The boundary condition, and a bug that reached an integration test
        // before it was caught here.** Relighting a region clears it — but the
        // light entering it has sources outside it, and if those are cleared
        // too the region comes back lit only by whatever it contains. A chunk
        // relit under open sky was coming back almost, but not quite, dark:
        // the daylight descending into it belonged to the region above.
        let mut world = open_box(20);
        let whole = world.region;
        relight(&mut world, whole);
        assert_eq!(world.at(BlockPos::new(10, 0, 10)).sun(), MAX_LEVEL);

        // Now relight only the bottom slab, whose own top is not open to the
        // sky — everything above it is loaded.
        let slab = Region {
            min: BlockPos::new(0, 0, 0),
            max: BlockPos::new(20, 5, 20),
        };
        relight(&mut world, slab);

        assert_eq!(
            world.at(BlockPos::new(10, 0, 10)).sun(),
            MAX_LEVEL,
            "relighting a slab lost the daylight falling into it from above"
        );
        assert_eq!(
            world.at(BlockPos::new(10, 5, 10)).sun(),
            MAX_LEVEL,
            "the top of the slab should be lit by the block above it"
        );
    }

    #[test]
    fn a_relight_of_the_same_world_gives_the_same_answer_twice() {
        // Charter rule 4 in miniature: the determinism gate hashes this, and a
        // BFS whose result depended on queue order would drift between runs
        // long before it drifted between platforms.
        let build = || {
            let mut world = open_box(16);
            world.fill(BlockPos::new(0, 8, 0), BlockPos::new(16, 8, 16));
            world.set_solid(BlockPos::new(4, 8, 4), false);
            world.lamp(BlockPos::new(9, 3, 9), Light::new(0, 12, 5, 15));
            world
        };

        let mut first = build();
        let region = first.region;
        relight(&mut first, region);
        let mut second = build();
        relight(&mut second, region);

        assert_eq!(
            first.light, second.light,
            "two relights of the same world disagreed"
        );
    }

    #[test]
    fn no_level_exceeds_what_a_source_could_have_produced() {
        // The invariant that catches an attenuation sign error, which otherwise
        // shows up as light getting brighter the further it travels.
        let mut world = open_box(16);
        world.fill(BlockPos::new(0, 8, 0), BlockPos::new(16, 8, 16));
        world.lamp(BlockPos::new(8, 4, 8), Light::new(0, 9, 0, 0));
        let region = world.region;
        relight(&mut world, region);

        for pos in region.blocks() {
            let level = world.at(pos);
            assert!(level.red() <= 9, "{pos:?} is redder than the only red lamp");
            assert!(level.sun() <= MAX_LEVEL);
            assert_eq!(level.green(), 0, "{pos:?} is green with no green source");
        }
    }

    #[test]
    fn the_permeability_used_is_the_cached_one() {
        // Not a behavioural assertion — a structural one. `Neighbourhood::faces`
        // is the only way propagation can learn what blocks light, so a future
        // change that recomputed the 3×3 test inside the loop would have to go
        // through this trait to do it. The test pins the shape by proving a
        // block's cached answer overrides what its cells say: here the cells are
        // air and the cache says opaque, and light stops.
        let mut world = open_box(8);
        // Solid throughout, then a single-block corridor carved along x — so
        // the only route from the lamp to the far side goes through the block
        // whose cache is being tested. Without sealing the box, light simply
        // walks around it and the test passes for the wrong reason.
        world.fill(BlockPos::new(0, 0, 0), BlockPos::new(8, 8, 8));
        for x in 0..=8 {
            world.set_solid(BlockPos::new(x, 4, 4), false);
        }
        world.masked.insert((4, 4, 4), Faces::OPAQUE);
        world.lamp(BlockPos::new(1, 4, 4), Light::new(0, MAX_LEVEL, 0, 0));
        let region = world.region;
        relight(&mut world, region);

        assert!(world.at(BlockPos::new(3, 4, 4)).red() > 0);
        assert_eq!(
            world.at(BlockPos::new(5, 4, 4)).red(),
            0,
            "propagation ignored the cached permeability and looked at the cells"
        );
        // And the block itself really is made of air, so the cache is the only
        // thing that could have stopped the light.
        assert_eq!(
            BlockView::Uniform(MaterialId::AIR).subnode(0),
            MaterialId::AIR
        );
        let _ = STONE;
    }

    #[test]
    fn breaking_a_lamp_takes_its_light_with_it() {
        // The case a plain flood cannot do: a flood only brightens, so without
        // the removal pass the light stays after the lamp is gone.
        let mut world = open_box(20);
        world.fill(BlockPos::new(0, 0, 0), BlockPos::new(20, 20, 20));
        for y in 2..=8 {
            for z in 2..=8 {
                for x in 2..=8 {
                    world.set_solid(BlockPos::new(x, y, z), false);
                }
            }
        }
        let lamp = BlockPos::new(5, 5, 5);
        world.lamp(lamp, Light::new(0, MAX_LEVEL, 0, 0));
        let region = world.region;
        relight(&mut world, region);
        assert!(
            world.at(BlockPos::new(7, 5, 5)).red() > 0,
            "the lamp never lit anything"
        );

        world.emitters.remove(&(lamp.x, lamp.y, lamp.z));
        edited(&mut world, lamp);

        for pos in region.blocks() {
            assert_eq!(
                world.at(pos).red(),
                0,
                "{pos:?} kept red light after the only red lamp was removed"
            );
        }
    }

    #[test]
    fn removing_one_of_two_lamps_leaves_the_others_light_intact() {
        // The case that catches over-removal: clearing everything the dead lamp
        // touched would take the surviving lamp's light with it, and the room
        // would go dark until something else happened to relight it.
        let mut world = open_box(20);
        world.fill(BlockPos::new(0, 0, 0), BlockPos::new(20, 20, 20));
        for x in 2..=18 {
            world.set_solid(BlockPos::new(x, 5, 5), false);
        }
        let dying = BlockPos::new(3, 5, 5);
        let surviving = BlockPos::new(17, 5, 5);
        world.lamp(dying, Light::new(0, MAX_LEVEL, 0, 0));
        world.lamp(surviving, Light::new(0, MAX_LEVEL, 0, 0));
        let region = world.region;
        relight(&mut world, region);

        world.emitters.remove(&(dying.x, dying.y, dying.z));
        edited(&mut world, dying);

        assert_eq!(
            world.at(surviving).red(),
            MAX_LEVEL,
            "the surviving lamp went out"
        );
        assert_eq!(world.at(BlockPos::new(16, 5, 5)).red(), MAX_LEVEL - 1);
        // The dead lamp's own block is fourteen blocks from the survivor, so it
        // is left at exactly the one level that reaches that far — not at zero.
        // Asserting zero would be asserting the surviving lamp went out too.
        assert_eq!(
            world.at(dying).red(),
            1,
            "the dead lamp's block should be lit by the surviving lamp and nothing else"
        );
    }

    #[test]
    fn roofing_over_a_column_puts_it_out_all_the_way_down() {
        // **The case a "strictly dimmer" removal test gets wrong.** Sunlight
        // does not attenuate downward, so every block under the sky has the
        // same level as the one above it; a removal that only followed dimmer
        // neighbours would stop at the first one and leave a lit shaft under a
        // solid roof.
        let mut world = open_box(20);
        let region = world.region;
        relight(&mut world, region);
        assert_eq!(world.at(BlockPos::new(5, 0, 5)).sun(), MAX_LEVEL);

        // Roof the whole box so no light sneaks in from the sides.
        for z in 0..=20 {
            for x in 0..=20 {
                let roof = BlockPos::new(x, 20, z);
                world.set_solid(roof, true);
                edited(&mut world, roof);
            }
        }

        for y in 0..20 {
            assert_eq!(
                world.at(BlockPos::new(10, y, 10)).sun(),
                0,
                "y = {y} is still lit under a solid roof"
            );
        }
    }

    #[test]
    fn breaking_a_hole_in_a_roof_lets_the_light_back_in() {
        // The other direction, and the reason `edited` seeds its neighbours: a
        // block that becomes air is not itself a source, so nothing would flood
        // into the hole it left.
        let mut world = open_box(20);
        world.fill(BlockPos::new(0, 10, 0), BlockPos::new(20, 10, 20));
        let region = world.region;
        relight(&mut world, region);
        assert_eq!(world.at(BlockPos::new(10, 9, 10)).sun(), 0);

        let hole = BlockPos::new(10, 10, 10);
        world.set_solid(hole, false);
        edited(&mut world, hole);

        assert_eq!(
            world.at(BlockPos::new(10, 9, 10)).sun(),
            MAX_LEVEL,
            "sunlight did not come through a hole punched in the roof"
        );
        assert_eq!(
            world.at(BlockPos::new(10, 0, 10)).sun(),
            MAX_LEVEL,
            "the shaft under the hole should be lit to the floor"
        );
    }

    #[test]
    fn incremental_relighting_agrees_with_relighting_from_scratch() {
        // **The invariant the whole incremental path exists to satisfy**, and
        // the one that catches every subtle removal bug at once: after any
        // sequence of edits, the world must hold exactly the light a full
        // relight would produce. A disagreement here is a lighting artefact
        // that survives until something else happens to dirty that chunk.
        let mut seed = 0x00C0_FFEE_u64;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };

        for trial in 0..12 {
            let mut world = open_box(12);
            // A roof with terrain under it, so there is both sunlight and
            // shadow to disturb.
            world.fill(BlockPos::new(0, 6, 0), BlockPos::new(12, 6, 12));
            let region = world.region;
            relight(&mut world, region);

            let mut script = Vec::new();
            for _ in 0..25 {
                let value = next();
                let pos = BlockPos::new(
                    (value % 13) as i32,
                    ((value >> 8) % 13) as i32,
                    ((value >> 16) % 13) as i32,
                );
                let kind = (value >> 24) % 3;
                script.push((pos, kind));

                match kind {
                    0 => {
                        world.set_solid(pos, true);
                        world.emitters.remove(&(pos.x, pos.y, pos.z));
                    }
                    1 => {
                        world.set_solid(pos, false);
                        world.emitters.remove(&(pos.x, pos.y, pos.z));
                    }
                    _ => {
                        world.set_solid(pos, false);
                        world.lamp(pos, Light::new(0, 12, 7, MAX_LEVEL));
                    }
                }
                edited(&mut world, pos);
            }

            // The same world, same edits, relit from scratch at the end.
            let mut oracle = open_box(12);
            oracle.fill(BlockPos::new(0, 6, 0), BlockPos::new(12, 6, 12));
            for (pos, kind) in &script {
                match kind {
                    0 => {
                        oracle.set_solid(*pos, true);
                        oracle.emitters.remove(&(pos.x, pos.y, pos.z));
                    }
                    1 => {
                        oracle.set_solid(*pos, false);
                        oracle.emitters.remove(&(pos.x, pos.y, pos.z));
                    }
                    _ => {
                        oracle.set_solid(*pos, false);
                        oracle.lamp(*pos, Light::new(0, 12, 7, MAX_LEVEL));
                    }
                }
            }
            relight(&mut oracle, region);

            for pos in region.blocks() {
                assert_eq!(
                    world.at(pos),
                    oracle.at(pos),
                    "trial {trial}: incremental light at {pos:?} disagrees with a full relight"
                );
            }
        }
    }

    /// One scripted edit, as a property test generates them.
    #[derive(Debug, Clone, Copy)]
    struct Edit {
        pos: BlockPos,
        kind: u8,
    }

    /// Applies an edit to a world without relighting it.
    fn apply(world: &mut Box3, edit: Edit) {
        let key = (edit.pos.x, edit.pos.y, edit.pos.z);
        match edit.kind % 3 {
            0 => {
                world.set_solid(edit.pos, true);
                world.emitters.remove(&key);
            }
            1 => {
                world.set_solid(edit.pos, false);
                world.emitters.remove(&key);
            }
            _ => {
                world.set_solid(edit.pos, false);
                world.lamp(edit.pos, Light::new(0, 12, 7, MAX_LEVEL));
            }
        }
    }

    /// A world with a roof over its middle, so a scene has both sky and shadow.
    fn scene(size: i32) -> Box3 {
        let mut world = open_box(size);
        world.fill(
            BlockPos::new(0, size / 2, 0),
            BlockPos::new(size, size / 2, size),
        );
        world
    }

    proptest::proptest! {
        #![proptest_config(proptest::prelude::ProptestConfig::with_cases(200))]

        /// **The invariant the incremental path exists to satisfy.** After any
        /// sequence of edits, the world holds exactly the light a relight from
        /// scratch would produce.
        ///
        /// Every subtle removal bug fails this and almost nothing else: an
        /// over-removal leaves a dark patch, an under-removal leaves light
        /// hanging in the air where its source used to be, and both survive
        /// until something unrelated happens to dirty that chunk.
        #[test]
        fn incremental_light_equals_a_full_relight(
            edits in proptest::collection::vec(
                (0i32..10, 0i32..10, 0i32..10, 0u8..3),
                1..25,
            ),
        ) {
            let edits: Vec<Edit> = edits
                .into_iter()
                .map(|(x, y, z, kind)| Edit { pos: BlockPos::new(x, y, z), kind })
                .collect();

            let mut incremental = scene(10);
            let region = incremental.region;
            relight(&mut incremental, region);
            for edit in &edits {
                apply(&mut incremental, *edit);
                edited(&mut incremental, edit.pos);
            }

            let mut oracle = scene(10);
            for edit in &edits {
                apply(&mut oracle, *edit);
            }
            relight(&mut oracle, region);

            for pos in region.blocks() {
                proptest::prop_assert_eq!(
                    incremental.at(pos),
                    oracle.at(pos),
                    "{:?} disagrees after {} edits",
                    pos,
                    edits.len()
                );
            }
        }

        /// No block is brighter than something could have made it.
        ///
        /// Sunlight is capped by the sky and every colour by the brightest lamp
        /// of that colour. A sign error in attenuation shows up here as light
        /// getting stronger the further it travels.
        #[test]
        fn light_never_exceeds_its_sources(
            edits in proptest::collection::vec(
                (0i32..10, 0i32..10, 0i32..10, 0u8..3),
                1..25,
            ),
        ) {
            let mut world = scene(10);
            let region = world.region;
            for (x, y, z, kind) in edits {
                apply(&mut world, Edit { pos: BlockPos::new(x, y, z), kind });
            }
            relight(&mut world, region);

            let brightest = world
                .emitters
                .values()
                .fold(Light::DARK, |acc, level| acc.max(*level));

            for pos in region.blocks() {
                let level = world.at(pos);
                proptest::prop_assert!(level.sun() <= MAX_LEVEL);
                for channel in 1..CHANNELS {
                    proptest::prop_assert!(
                        level.channel(channel) <= brightest.channel(channel),
                        "{:?} channel {} is {} with the brightest source at {}",
                        pos,
                        channel,
                        level.channel(channel),
                        brightest.channel(channel)
                    );
                }
            }
        }

        /// Every lit block can explain where its light came from.
        ///
        /// **The no-orphan-light invariant.** A block is allowed to be lit only
        /// if it emits, or the sky reaches it, or a neighbour is bright enough
        /// to have supplied exactly that level. Light left behind by an
        /// incomplete removal fails this: it sits in mid-air with every
        /// neighbour too dim to account for it.
        #[test]
        fn no_lit_block_is_without_a_source(
            edits in proptest::collection::vec(
                (0i32..10, 0i32..10, 0i32..10, 0u8..3),
                1..25,
            ),
        ) {
            let mut world = scene(10);
            let region = world.region;
            relight(&mut world, region);
            for (x, y, z, kind) in edits {
                let edit = Edit { pos: BlockPos::new(x, y, z), kind };
                apply(&mut world, edit);
                edited(&mut world, edit.pos);
            }

            for pos in region.blocks() {
                let level = world.at(pos);
                let emission = world.emission(pos);
                let sky = sky_reaches(&world, pos);

                for channel in 0..CHANNELS {
                    let here = level.channel(channel);
                    if here == 0 || here <= emission.channel(channel) {
                        continue;
                    }
                    if channel == 0 && sky && here == MAX_LEVEL {
                        continue;
                    }

                    let explained = (0..FACE_COUNT).any(|face| {
                        crosses(&world, pos, face).is_some_and(|next| {
                            arriving(channel, world.at(next).channel(channel), opposite(face))
                                >= here
                        })
                    });
                    proptest::prop_assert!(
                        explained,
                        "{:?} has channel {} at {} with nothing to account for it",
                        pos,
                        channel,
                        here
                    );
                }
            }
        }
    }
}
