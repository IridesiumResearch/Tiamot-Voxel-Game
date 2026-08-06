// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Benchmarks for light propagation.
//!
//! # Read these as a share of a tick, never on their own
//!
//! Charter rule 18: the budget is **50 ms for all simulation for all players**,
//! and a number in isolation says nothing. The two that matter:
//!
//! - **A full-chunk relight** happens when a chunk enters memory. The server
//!   caps these at `RELIGHTS_PER_TICK` (32), so this figure times 32 is the
//!   worst a tick can spend filling in newly loaded terrain.
//! - **An incremental relight** happens on every block a player changes.
//!   Fifty players digging as fast as they can is fifty of these a tick, so
//!   this figure times fifty is the steady-state cost of a busy server.
//!
//! Task 02b measured a full-chunk relight at about 30 µs on the reference
//! machine and used it to require the cached permeability byte (charter rule
//! 19). These are the same measurement taken against the real implementation
//! rather than a spike.
//!
//! # The scenes
//!
//! - `open_sky` is the cheap case and most of a world: a chunk of air under
//!   daylight, where the flood walks every cell exactly once.
//! - `sealed` is the other common case: solid rock, where the flood is
//!   rejected at the first face and the layer never leaves its uniform form.
//! - `chiselled` is the pathological one Task 02b built the permeability
//!   requirement around — every block partly carved, so no face test is
//!   trivially answerable and the layer goes dense.

use criterion::{Criterion, criterion_group, criterion_main};
use std::collections::BTreeMap;
use std::hint::black_box;

use tiamot_core::block::OCCUPANCY_FULL;
use tiamot_core::coords::BlockPos;
use tiamot_core::light::propagate::{Neighbourhood, Region};
use tiamot_core::light::{Faces, Light, LightLayer, MAX_LEVEL, edited, permeability, relight};
use tiamot_core::{BlockValue, CHUNK_BLOCKS, MaterialId};

const STONE: MaterialId = MaterialId(2);

/// One chunk's worth of blocks, with the light and permeability propagation
/// needs.
///
/// Deliberately holds the cached [`Faces`] per block rather than recomputing
/// it: that is what the real chunk does (charter rule 19), and a benchmark that
/// recomputed the face test would measure a design the engine does not have.
struct Scene {
    region: Region,
    faces: Vec<Faces>,
    lamps: BTreeMap<(i32, i32, i32), Light>,
    light: LightLayer,
}

impl Scene {
    fn index(&self, pos: BlockPos) -> Option<usize> {
        if !self.region.contains(pos) {
            return None;
        }
        let span = CHUNK_BLOCKS as i32;
        Some((pos.y * span * span + pos.z * span + pos.x) as usize)
    }

    fn build(block: impl Fn(i32, i32, i32) -> BlockValue) -> Self {
        let span = CHUNK_BLOCKS as i32;
        let region = Region {
            min: BlockPos::new(0, 0, 0),
            max: BlockPos::new(span - 1, span - 1, span - 1),
        };
        let mut faces = Vec::with_capacity((span * span * span) as usize);
        for y in 0..span {
            for z in 0..span {
                for x in 0..span {
                    faces.push(permeability(&block(x, y, z).view()));
                }
            }
        }
        Self {
            region,
            faces,
            lamps: BTreeMap::new(),
            light: LightLayer::dark(),
        }
    }
}

impl Neighbourhood for Scene {
    fn faces(&self, pos: BlockPos) -> Option<Faces> {
        self.index(pos).map(|index| self.faces[index])
    }

    fn emission(&self, pos: BlockPos) -> Light {
        self.lamps
            .get(&(pos.x, pos.y, pos.z))
            .copied()
            .unwrap_or(Light::DARK)
    }

    fn light(&self, pos: BlockPos) -> Light {
        self.index(pos)
            .map_or(Light::DARK, |_| self.light.get(local(pos)))
    }

    fn set_light(&mut self, pos: BlockPos, level: Light) {
        if self.index(pos).is_some() {
            self.light.set(local(pos), level);
        }
    }
}

/// The chunk-local block address of a position inside the scene.
fn local(pos: BlockPos) -> tiamot_core::coords::LocalBlock {
    tiamot_core::coords::LocalBlock::new(pos.x as u32, pos.y as u32, pos.z as u32)
}

fn open_sky() -> Scene {
    Scene::build(|_, _, _| BlockValue::AIR)
}

fn sealed() -> Scene {
    Scene::build(|_, _, _| BlockValue::Uniform(STONE))
}

fn chiselled() -> Scene {
    // Every block partly carved and no two the same, which is the shape that
    // made the uncached face test cost 50% in Task 02b.
    Scene::build(|x, y, z| BlockValue::Partial {
        material: STONE,
        occupancy: ((x * 7 + y * 13 + z * 31) as u32 | 1) & OCCUPANCY_FULL & !(1 << 13),
    })
}

/// A chunk relit from scratch, as happens when one enters memory.
fn bench_full_relight(c: &mut Criterion) {
    let mut group = c.benchmark_group("relight_chunk");

    for (name, build) in [
        ("open_sky", open_sky as fn() -> Scene),
        ("sealed", sealed as fn() -> Scene),
        ("chiselled", chiselled as fn() -> Scene),
    ] {
        group.bench_function(name, |b| {
            b.iter_batched_ref(
                build,
                |scene| {
                    let region = scene.region;
                    relight(scene, region);
                    black_box(scene.light.get(local(BlockPos::new(0, 0, 0))))
                },
                criterion::BatchSize::SmallInput,
            );
        });
    }
    group.finish();
}

/// One block changed, as happens on every dig and every placement.
///
/// **The figure that scales with players**, so it is the one to watch: fifty
/// players digging is fifty of these per tick.
fn bench_incremental(c: &mut Criterion) {
    let mut group = c.benchmark_group("relight_after_edit");

    // Opening a hole in a lit chunk: the flood has somewhere to go, which is
    // the expensive direction.
    //
    // **The scene is open sky with a solid core, not solid rock.** A sealed
    // chunk holds no light at all, so opening a hole in one floods nothing and
    // the benchmark measured 27 ns of doing nothing. Check what a suspiciously
    // fast number is actually exercising before believing it.
    group.bench_function("open_a_hole", |b| {
        b.iter_batched_ref(
            || {
                let mut scene = Scene::build(|x, y, z| {
                    if (4..12).contains(&x) && (4..12).contains(&y) && (4..12).contains(&z) {
                        BlockValue::Uniform(STONE)
                    } else {
                        BlockValue::AIR
                    }
                });
                let region = scene.region;
                relight(&mut scene, region);
                scene
            },
            |scene| {
                // On the FACE of the solid core, not in its middle. A block
                // opened deep inside solid rock has nothing but more rock
                // around it, so nothing floods and the benchmark measures the
                // queue being empty — which is what the first two attempts at
                // this one did.
                let pos = BlockPos::new(4, 8, 8);
                if let Some(index) = scene.index(pos) {
                    scene.faces[index] = Faces::OPEN;
                }
                edited(scene, pos);
                black_box(scene.light.get(local(pos)))
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // Placing a lamp in the dark: a full-strength flood outward in every
    // direction, which is the worst an ordinary player action can cost.
    group.bench_function("place_a_lamp", |b| {
        b.iter_batched_ref(
            || {
                let mut scene = open_sky();
                let region = scene.region;
                relight(&mut scene, region);
                scene
            },
            |scene| {
                let pos = BlockPos::new(8, 8, 8);
                scene.lamps.insert(
                    (pos.x, pos.y, pos.z),
                    Light::new(0, MAX_LEVEL, MAX_LEVEL, 0),
                );
                edited(scene, pos);
                black_box(scene.light.get(local(pos)))
            },
            criterion::BatchSize::SmallInput,
        );
    });

    // And taking it away again, which runs the removal walk and then refills.
    group.bench_function("break_a_lamp", |b| {
        b.iter_batched_ref(
            || {
                let mut scene = open_sky();
                let pos = BlockPos::new(8, 8, 8);
                scene.lamps.insert(
                    (pos.x, pos.y, pos.z),
                    Light::new(0, MAX_LEVEL, MAX_LEVEL, 0),
                );
                let region = scene.region;
                relight(&mut scene, region);
                scene
            },
            |scene| {
                let pos = BlockPos::new(8, 8, 8);
                scene.lamps.remove(&(pos.x, pos.y, pos.z));
                edited(scene, pos);
                black_box(scene.light.get(local(pos)))
            },
            criterion::BatchSize::SmallInput,
        );
    });

    group.finish();
}

criterion_group!(benches, bench_full_relight, bench_incremental);
criterion_main!(benches);
