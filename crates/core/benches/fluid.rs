// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! What a fluid tick costs.
//!
//! # Read these as a share of the tick, never on their own
//!
//! Charter rule 18: 20 Hz, so **50 ms shared by all simulation for fifty
//! players**. Fluid runs at half that (`server::fluid::TICKS_PER_FLUID_TICK`),
//! so a fluid tick has 50 ms of budget available to it every other tick — but
//! it is sharing that with physics, lighting, entities and every mod callback,
//! and the figure that matters is what share of the whole it takes.
//!
//! Criterion reports times, not shares, so the arithmetic is: **1 ms is 2% of a
//! tick**. A figure here that reads 0.5 ms is a fiftieth of everything the
//! server has, for one system, while a player watches one pond.
//!
//! # The scenes
//!
//! - `settled` is the one that has to be free, and is the whole reason the
//!   solver is a work queue rather than a sweep. A world where nothing is
//!   flowing must cost nothing at all, and this measures the claim rather than
//!   asserting it in a comment.
//! - `pour_field` is the worst case the task names: a hundred buckets emptied
//!   at once, all spreading, all competing for the same processing cap.
//! - `one_pour` is what a player actually does — pour one thing, watch it
//!   run — and is the figure to compare against fifty players doing it.
//! - `relevelling` is the workload conservation creates that the old model had
//!   no equivalent of: take a wall out of a settled pond and every block on
//!   both sides has to move until the two halves agree.
//! - `sinks` is what absorption and evaporation cost, measured against the same
//!   pour with neither, so the price of each is readable rather than baked into
//!   one number.
//!
//! # What used to be here and is not
//!
//! `hole_preference` measured `Tuning::hole_search`, which steered a spring
//! towards a drop. Conserved fluid has no such rule and needs none — water goes
//! downhill because volume falls, not because the solver went looking. A
//! benchmark for a mechanism that no longer exists is worse than none: it runs,
//! reports a number, and the number means nothing.

use std::collections::{BTreeMap, BTreeSet};

use criterion::{Criterion, criterion_group, criterion_main};
use tiamot_core::coords::BlockPos;
use tiamot_core::fluid::{Fluid, FluidId, MAX_VOLUME, Neighbourhood, Solver, Tuning};

/// The world seed the benchmark's fluid runs under.
///
/// Fixed, because evaporation is seeded (charter rule 4) and a benchmark whose
/// workload varied run to run would be measuring the seed.
const SEED: u64 = 0x7B1A_3F2E_9C4D_5E60;

const MILK: FluidId = FluidId(1);

/// The server's per-tick visit cap, mirrored.
///
/// Not imported, because `crates/core` cannot depend on the server — so it is
/// written here and `the_bench_and_the_server_agree_on_the_cap` in
/// `server::fluid` fails if the two ever drift.
const VISITS: usize = 512;

#[derive(Default, Clone)]
struct Scene {
    solid: BTreeSet<(i32, i32, i32)>,
    fluid: BTreeMap<(i32, i32, i32), Fluid>,
    /// Whether every solid block drinks, for the sink benchmark.
    absorbent: bool,
}

impl Scene {
    /// A floor at y = 0, walled at `span` so nothing escapes the measurement.
    fn flat(span: i32) -> Self {
        let mut scene = Self::default();
        for x in -span..=span {
            for z in -span..=span {
                scene.solid.insert((x, 0, z));
            }
        }
        scene
    }

    /// Empties one bucket into a block, and wakes it.
    fn pour(&mut self, solver: &mut Solver, x: i32, z: i32) {
        let at = BlockPos::new(x, 1, z);
        self.fluid.insert((x, 1, z), Fluid::new(MILK, MAX_VOLUME));
        solver.touch(at);
    }
}

impl Neighbourhood for Scene {
    fn occupancy(&self, pos: BlockPos) -> Option<u32> {
        // Bounded, or a benchmark measures milk falling into an infinite void.
        if pos.x.abs() > 40 || pos.z.abs() > 40 || pos.y < -4 || pos.y > 8 {
            return None;
        }
        Some(if self.solid.contains(&(pos.x, pos.y, pos.z)) {
            tiamot_core::UNITS_PER_BLOCK
        } else {
            0
        })
    }

    fn absorbency(&self, pos: BlockPos) -> u32 {
        if self.absorbent && self.solid.contains(&(pos.x, pos.y, pos.z)) {
            1
        } else {
            0
        }
    }

    fn fluid(&self, pos: BlockPos) -> Fluid {
        self.fluid
            .get(&(pos.x, pos.y, pos.z))
            .copied()
            .unwrap_or(Fluid::EMPTY)
    }

    fn set_fluid(&mut self, pos: BlockPos, value: Fluid) {
        if value.is_empty() {
            self.fluid.remove(&(pos.x, pos.y, pos.z));
        } else {
            self.fluid.insert((pos.x, pos.y, pos.z), value);
        }
    }
}

/// Runs to a standstill, so a case starts from a settled world rather than from
/// whatever the previous iteration left behind.
fn settle(scene: &mut Scene, solver: &mut Solver) {
    for _ in 0..500 {
        if solver.is_settled() {
            return;
        }
        solver.tick(scene, Tuning::DEFAULT, usize::MAX, SEED, 0);
    }
}

fn bench_settled(c: &mut Criterion) {
    // **The assertion the perf criterion makes, measured.** A world where
    // nothing is flowing costs one comparison against an empty set.
    let mut scene = Scene::flat(20);
    let mut solver = Solver::new();
    scene.pour(&mut solver, 0, 0);
    settle(&mut scene, &mut solver);
    assert!(solver.is_settled(), "the scene never settled");

    c.bench_function("fluid_tick/settled", |b| {
        b.iter(|| {
            let changes = solver.tick(&mut scene, Tuning::DEFAULT, VISITS, SEED, 0);
            assert!(changes.is_empty());
        });
    });
}

fn bench_spreading(c: &mut Criterion) {
    let mut group = c.benchmark_group("fluid_spreading");

    // One bucket, from the click to standstill. What a player does.
    group.bench_function("one_pour_to_standstill", |b| {
        b.iter(|| {
            let mut scene = Scene::flat(20);
            let mut solver = Solver::new();
            scene.pour(&mut solver, 0, 0);
            settle(&mut scene, &mut solver);
        });
    });

    // A hundred at once — the task's worst case.
    group.bench_function("pour_field_to_standstill", |b| {
        b.iter(|| {
            let mut scene = Scene::flat(40);
            let mut solver = Solver::new();
            for index in 0..100 {
                let x = (index % 10) * 8 - 36;
                let z = (index / 10) * 8 - 36;
                scene.pour(&mut solver, x, z);
            }
            settle(&mut scene, &mut solver);
        });
    });

    // The same field, one CAPPED tick at a time, which is what the server
    // actually runs. This is the number the processing cap exists to bound.
    group.bench_function("pour_field_one_capped_tick", |b| {
        let mut scene = Scene::flat(40);
        let mut solver = Solver::new();
        for index in 0..100 {
            let x = (index % 10) * 8 - 36;
            let z = (index / 10) * 8 - 36;
            scene.pour(&mut solver, x, z);
        }
        b.iter(|| {
            if solver.is_settled() {
                for index in 0..100 {
                    let x = (index % 10) * 8 - 36;
                    let z = (index / 10) * 8 - 36;
                    solver.touch(BlockPos::new(x, 1, z));
                }
            }
            solver.tick(&mut scene, Tuning::DEFAULT, VISITS, SEED, 0);
        });
    });

    group.finish();
}

fn bench_relevelling(c: &mut Criterion) {
    // **The workload conservation creates.** Under the old model, taking a
    // wall out let a source feed somewhere new and the level fell away from it
    // one block at a time. Under this one, two settled bodies at different
    // heights have to exchange volume until they agree — every block on both
    // sides moves, and none of them is finished until all of them are.
    //
    // This replaces the old `draining` case, which measured a source being
    // taken away. There are no sources, and nothing drains: milk that has
    // nowhere to go stays exactly where it is.
    c.bench_function("fluid_tick/relevelling", |b| {
        b.iter(|| {
            let mut scene = Scene::flat(20);
            let mut solver = Solver::new();
            // A sealed pool on the left, kept apart from open floor by a wall.
            for z in -6..=6 {
                scene.solid.insert((0, 1, z));
            }
            for x in -6..0 {
                for z in -6..=6 {
                    scene.fluid.insert((x, 1, z), Fluid::new(MILK, MAX_VOLUME));
                    solver.touch(BlockPos::new(x, 1, z));
                }
            }
            settle(&mut scene, &mut solver);

            // Out comes the wall.
            for z in -6..=6 {
                scene.solid.remove(&(0, 1, z));
                solver.touch(BlockPos::new(0, 1, z));
            }
            settle(&mut scene, &mut solver);
        });
    });
}

fn bench_sinks(c: &mut Criterion) {
    let mut group = c.benchmark_group("fluid_sinks");

    // The same pour three ways, so the price of each sink is readable rather
    // than buried in one figure. Absorption is a lookup per neighbour on every
    // visit; evaporation is a hash per block open to the air.
    group.bench_function("no_sinks", |b| {
        b.iter(|| {
            let mut scene = Scene::flat(20);
            let mut solver = Solver::new();
            scene.pour(&mut solver, 0, 0);
            settle(&mut scene, &mut solver);
        });
    });

    let thirsty = Tuning {
        evaporates: 8,
        ..Tuning::DEFAULT
    };
    group.bench_function("evaporating", |b| {
        b.iter(|| {
            let mut scene = Scene::flat(20);
            let mut solver = Solver::new();
            scene.pour(&mut solver, 0, 0);
            for tick in 0..500u64 {
                if solver.is_settled() {
                    break;
                }
                solver.tick(&mut scene, thirsty, usize::MAX, SEED, tick);
            }
        });
    });

    group.bench_function("absorbing", |b| {
        b.iter(|| {
            let mut scene = Scene::flat(20);
            scene.absorbent = true;
            let mut solver = Solver::new();
            scene.pour(&mut solver, 0, 0);
            settle(&mut scene, &mut solver);
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_settled,
    bench_spreading,
    bench_relevelling,
    bench_sinks
);
criterion_main!(benches);
