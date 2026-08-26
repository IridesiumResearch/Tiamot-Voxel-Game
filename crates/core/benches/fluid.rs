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
//! - `spring_field` is the worst case the task names: a hundred sources opened
//!   at once, all spreading, all competing for the same processing cap.
//! - `one_spring` is what a player actually does — pour one thing, watch it
//!   run — and is the figure to compare against fifty players doing it.
//! - `draining` is the other half of a spring's life, and is not free: every
//!   block that loses its parent has to be visited to find that out.
//!
//! # The hole preference is the expensive part
//!
//! `Tuning::hole_search` makes a spring steer towards a drop, and paying for it
//! means a bounded search from every block that might feed another. `flat` and
//! `pitted` are the same scene with and without somewhere to fall, so the
//! difference between them IS the cost of that behaviour.

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

    /// The same, with a grid of holes in the floor to steer towards.
    fn pitted(span: i32) -> Self {
        let mut scene = Self::flat(span);
        let mut x = -span + 3;
        while x <= span - 3 {
            let mut z = -span + 3;
            while z <= span - 3 {
                scene.solid.remove(&(x, 0, z));
                scene.solid.insert((x, -2, z));
                z += 7;
            }
            x += 7;
        }
        scene
    }

    fn spring(&mut self, solver: &mut Solver, x: i32, z: i32) {
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
    scene.spring(&mut solver, 0, 0);
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

    // One spring, from placement to standstill. What a player does.
    group.bench_function("one_spring_to_standstill", |b| {
        b.iter(|| {
            let mut scene = Scene::flat(20);
            let mut solver = Solver::new();
            scene.spring(&mut solver, 0, 0);
            settle(&mut scene, &mut solver);
        });
    });

    // A hundred at once — the task's worst case.
    group.bench_function("spring_field_to_standstill", |b| {
        b.iter(|| {
            let mut scene = Scene::flat(40);
            let mut solver = Solver::new();
            for index in 0..100 {
                let x = (index % 10) * 8 - 36;
                let z = (index / 10) * 8 - 36;
                scene.spring(&mut solver, x, z);
            }
            settle(&mut scene, &mut solver);
        });
    });

    // The same field, one CAPPED tick at a time, which is what the server
    // actually runs. This is the number the processing cap exists to bound.
    group.bench_function("spring_field_one_capped_tick", |b| {
        let mut scene = Scene::flat(40);
        let mut solver = Solver::new();
        for index in 0..100 {
            let x = (index % 10) * 8 - 36;
            let z = (index / 10) * 8 - 36;
            scene.spring(&mut solver, x, z);
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

fn bench_hole_preference(c: &mut Criterion) {
    let mut group = c.benchmark_group("fluid_hole_preference");

    // The same spring on a floor with nothing to fall into, and on one with
    // holes in it. The gap between these two IS what steering costs.
    group.bench_function("flat", |b| {
        b.iter(|| {
            let mut scene = Scene::flat(20);
            let mut solver = Solver::new();
            scene.spring(&mut solver, 0, 0);
            settle(&mut scene, &mut solver);
        });
    });

    group.bench_function("pitted", |b| {
        b.iter(|| {
            let mut scene = Scene::pitted(20);
            let mut solver = Solver::new();
            scene.spring(&mut solver, 0, 0);
            settle(&mut scene, &mut solver);
        });
    });

    // And with the preference off, which is what `hole_search = 0` buys a mod
    // that would rather have the speed.
    let even = Tuning { ..Tuning::DEFAULT };
    group.bench_function("pitted_without_steering", |b| {
        b.iter(|| {
            let mut scene = Scene::pitted(20);
            let mut solver = Solver::new();
            scene.spring(&mut solver, 0, 0);
            for _ in 0..500 {
                if solver.is_settled() {
                    break;
                }
                solver.tick(&mut scene, even, usize::MAX, SEED, 0);
            }
        });
    });

    group.finish();
}

fn bench_draining(c: &mut Criterion) {
    // Taking a source away is not free: every block that lost its parent has to
    // be visited to find that out, and the pond drains a level per tick.
    c.bench_function("fluid_tick/draining", |b| {
        b.iter(|| {
            let mut scene = Scene::flat(20);
            let mut solver = Solver::new();
            scene.spring(&mut solver, 0, 0);
            settle(&mut scene, &mut solver);

            scene.set_fluid(BlockPos::new(0, 1, 0), Fluid::EMPTY);
            solver.touch(BlockPos::new(0, 1, 0));
            settle(&mut scene, &mut solver);
            assert!(scene.fluid.is_empty(), "milk survived its source");
        });
    });
}

criterion_group!(
    benches,
    bench_settled,
    bench_spreading,
    bench_hole_preference,
    bench_draining
);
criterion_main!(benches);
