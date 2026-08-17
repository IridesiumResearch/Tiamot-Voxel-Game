// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Task 12 decision spike: which entity store `crates/core::ent` is built on.
//!
//! The task names three candidates — `bevy_ecs`, `hecs`, and a minimal
//! hand-rolled store — and says to weigh **deterministic iteration order and
//! dependency weight in `core` at least as heavily as raw query speed**, since
//! entity counts here are small. This measures all three on the workloads the
//! engine actually runs, and tests the determinism claim rather than assuming
//! it.
//!
//! Run with `cargo run --release -p ecs-spike`. Output goes in
//! `docs/ecs-verdict.md`.
//!
//! # The workloads, and why these
//!
//! - **step** — integrate every entity's position from its velocity. This is
//!   the per-tick cost and the only one that runs 20 times a second forever.
//! - **spawn / despawn** — a chunk loading or unloading thaws or freezes the
//!   entities in it, so churn is bursty and tied to movement, not to the tick.
//! - **radius query** — `game.entities_in_radius`, which a mod's `on_step`
//!   calls and which therefore multiplies by the number of scripted mobs.
//! - **attach / detach** — a mod adding its own component to an entity. This is
//!   the operation an archetype ECS is slowest at and a struct is fastest at,
//!   so leaving it out would flatter two of the three candidates.
//!
//! # The determinism test is the gate, not a tiebreak
//!
//! Charter rule 4 bans float accumulation over non-deterministic iteration
//! order. Two worlds built by the identical sequence of calls, in one process,
//! must iterate in the identical order — and the reason to test it rather than
//! read the docs is that Rust's `RandomState` is seeded per process and
//! **advances per instance**, so a store that iterates a std `HashMap`
//! internally gives two worlds in ONE run different orders. That is exactly the
//! failure the rule exists to catch, and it is invisible in a single world.

use std::time::{Duration, Instant};

/// Entities in the perf gate: "200 scripted wandering entities in one area".
const GATE: usize = 200;
/// And an order of magnitude of headroom, to see how each one scales.
const HEADROOM: usize = 2_000;

/// The engine's Transform, cut down to what a benchmark needs.
///
/// Chunk-anchored, because charter rule 7 forbids accumulating world-space f32.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Transform {
    chunk: [i32; 3],
    local: [f32; 3],
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Velocity([f32; 3]);

/// A mod's own component: a handle into the script VM's registry.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Script(u32);

impl Transform {
    fn seeded(index: usize) -> Self {
        // Deterministic and spread out, with no RNG to argue about.
        let value = index as f32;
        Self {
            chunk: [(index / 64) as i32, 0, (index % 64) as i32],
            local: [value * 0.017, value * 0.011, value * 0.023],
        }
    }

    /// Squared distance to a point in the same chunk frame, in blocks.
    fn distance_squared(&self, chunk: [i32; 3], local: [f32; 3]) -> f32 {
        let blocks = tiamot_core::CHUNK_BLOCKS as f32;
        let mut total = 0.0;
        for axis in 0..3 {
            let offset =
                (self.chunk[axis] - chunk[axis]) as f32 * blocks + self.local[axis] - local[axis];
            total += offset * offset;
        }
        total
    }
}

fn step_one(transform: &mut Transform, velocity: &Velocity) {
    for axis in 0..3 {
        transform.local[axis] += velocity.0[axis];
    }
}

/// What each candidate has to be able to do.
///
/// A trait rather than three loose functions so the measured work is provably
/// the same shape in all three, and so the determinism test can run over each
/// without knowing which it holds.
trait Store {
    /// Spawns `count` entities with a transform and a velocity.
    fn populate(&mut self, count: usize);
    /// Integrates every entity's position. The per-tick cost.
    fn step(&mut self);
    /// How many entities lie within `radius` blocks of the origin chunk.
    ///
    /// **`&mut self` because `bevy_ecs` leaves no choice.** `World::query`
    /// needs `&mut World` to cache the query's state, so an uncached read is
    /// an exclusive borrow — which matters beyond a benchmark: a mod calling
    /// `game.entities_in_radius` from inside `on_step` would be asking for the
    /// world mutably while the engine is already stepping it. The other two
    /// would be happy with `&self`, and that difference is a finding rather
    /// than a benchmarking inconvenience.
    fn within(&mut self, radius: f32) -> usize;
    /// Attaches a script component to every second entity.
    fn attach_every_other(&mut self);
    /// Removes every script component again.
    fn detach_all(&mut self);
    /// Despawns every second entity.
    fn despawn_every_other(&mut self);
    /// The order iteration visits entities in, as their seeded indices.
    ///
    /// Read back from the transform rather than from any id, so the three
    /// candidates are comparable: an id means something different in each.
    fn order(&mut self) -> Vec<i32>;
}

// --- the hand-rolled store ------------------------------------------------

/// A generational arena with the engine's components as fields.
///
/// **This is the shape the engine would actually ship**, not a strawman. The
/// engine-defined component set is fixed and known at compile time — Transform,
/// Velocity, collider, ModelRef, AnimState, Health, Nametag, Owner — so the
/// thing an ECS is for, arbitrary combinations discovered at runtime, is not
/// what this engine has. Mods attach Lua tables, which are one handle, not a
/// type per mod.
#[derive(Default)]
struct HandRolled {
    records: Vec<Record>,
    free: Vec<usize>,
}

#[derive(Clone)]
struct Record {
    generation: u32,
    alive: bool,
    transform: Transform,
    velocity: Velocity,
    script: Option<Script>,
}

impl Store for HandRolled {
    fn populate(&mut self, count: usize) {
        for index in 0..count {
            let record = Record {
                generation: 0,
                alive: true,
                transform: Transform::seeded(index),
                velocity: Velocity([0.001, 0.002, 0.003]),
                script: None,
            };
            match self.free.pop() {
                Some(slot) => {
                    let generation = self.records[slot].generation + 1;
                    self.records[slot] = Record {
                        generation,
                        ..record
                    };
                }
                None => self.records.push(record),
            }
        }
    }

    fn step(&mut self) {
        for record in &mut self.records {
            if record.alive {
                step_one(&mut record.transform, &record.velocity);
            }
        }
    }

    fn within(&mut self, radius: f32) -> usize {
        let bound = radius * radius;
        self.records
            .iter()
            .filter(|record| {
                record.alive && record.transform.distance_squared([0, 0, 0], [0.0; 3]) <= bound
            })
            .count()
    }

    fn attach_every_other(&mut self) {
        for (index, record) in self.records.iter_mut().enumerate() {
            if record.alive && index % 2 == 0 {
                record.script = Some(Script(index as u32));
            }
        }
    }

    fn detach_all(&mut self) {
        for record in &mut self.records {
            record.script = None;
        }
    }

    fn despawn_every_other(&mut self) {
        for (index, record) in self.records.iter_mut().enumerate() {
            if record.alive && index % 2 == 0 {
                record.alive = false;
                self.free.push(index);
            }
        }
        // Freed slots are reused newest-first by `populate`; sorting keeps that
        // choice a decision rather than an accident of the loop above.
        self.free.sort_unstable();
    }

    fn order(&mut self) -> Vec<i32> {
        self.records
            .iter()
            .filter(|record| record.alive)
            .map(|record| record.transform.chunk[2])
            .collect()
    }
}

// --- hecs -----------------------------------------------------------------

#[derive(Default)]
struct Hecs {
    world: hecs::World,
    spawned: Vec<hecs::Entity>,
}

impl Store for Hecs {
    fn populate(&mut self, count: usize) {
        for index in 0..count {
            let entity = self
                .world
                .spawn((Transform::seeded(index), Velocity([0.001, 0.002, 0.003])));
            self.spawned.push(entity);
        }
    }

    fn step(&mut self) {
        for (transform, velocity) in self.world.query_mut::<(&mut Transform, &Velocity)>() {
            step_one(transform, velocity);
        }
    }

    fn within(&mut self, radius: f32) -> usize {
        let bound = radius * radius;
        self.world
            .query::<&Transform>()
            .iter()
            .filter(|transform| transform.distance_squared([0, 0, 0], [0.0; 3]) <= bound)
            .count()
    }

    fn attach_every_other(&mut self) {
        for (index, entity) in self.spawned.clone().into_iter().enumerate() {
            if index % 2 == 0 {
                let _ = self.world.insert_one(entity, Script(index as u32));
            }
        }
    }

    fn detach_all(&mut self) {
        for entity in self.spawned.clone() {
            let _ = self.world.remove_one::<Script>(entity);
        }
    }

    fn despawn_every_other(&mut self) {
        let spawned = std::mem::take(&mut self.spawned);
        for (index, entity) in spawned.into_iter().enumerate() {
            if index % 2 == 0 {
                let _ = self.world.despawn(entity);
            } else {
                self.spawned.push(entity);
            }
        }
    }

    fn order(&mut self) -> Vec<i32> {
        self.world
            .query::<&Transform>()
            .iter()
            .map(|transform| transform.chunk[2])
            .collect()
    }
}

// --- bevy_ecs -------------------------------------------------------------

impl bevy_ecs::component::Component for Transform {
    const STORAGE_TYPE: bevy_ecs::component::StorageType = bevy_ecs::component::StorageType::Table;
    type Mutability = bevy_ecs::component::Mutable;
}

impl bevy_ecs::component::Component for Velocity {
    const STORAGE_TYPE: bevy_ecs::component::StorageType = bevy_ecs::component::StorageType::Table;
    type Mutability = bevy_ecs::component::Mutable;
}

impl bevy_ecs::component::Component for Script {
    const STORAGE_TYPE: bevy_ecs::component::StorageType = bevy_ecs::component::StorageType::Table;
    type Mutability = bevy_ecs::component::Mutable;
}

#[derive(Default)]
struct Bevy {
    world: bevy_ecs::world::World,
    spawned: Vec<bevy_ecs::entity::Entity>,
}

impl Store for Bevy {
    fn populate(&mut self, count: usize) {
        for index in 0..count {
            let entity = self
                .world
                .spawn((Transform::seeded(index), Velocity([0.001, 0.002, 0.003])))
                .id();
            self.spawned.push(entity);
        }
    }

    fn step(&mut self) {
        let mut query = self.world.query::<(&mut Transform, &Velocity)>();
        for (mut transform, velocity) in query.iter_mut(&mut self.world) {
            step_one(&mut transform, velocity);
        }
    }

    fn within(&mut self, radius: f32) -> usize {
        let bound = radius * radius;
        let mut query = self.world.query::<&Transform>();
        query
            .iter(&self.world)
            .filter(|transform| transform.distance_squared([0, 0, 0], [0.0; 3]) <= bound)
            .count()
    }

    fn attach_every_other(&mut self) {
        for (index, entity) in self.spawned.clone().into_iter().enumerate() {
            if index % 2 == 0 {
                self.world.entity_mut(entity).insert(Script(index as u32));
            }
        }
    }

    fn detach_all(&mut self) {
        for entity in self.spawned.clone() {
            self.world.entity_mut(entity).remove::<Script>();
        }
    }

    fn despawn_every_other(&mut self) {
        let spawned = std::mem::take(&mut self.spawned);
        for (index, entity) in spawned.into_iter().enumerate() {
            if index % 2 == 0 {
                self.world.despawn(entity);
            } else {
                self.spawned.push(entity);
            }
        }
    }

    fn order(&mut self) -> Vec<i32> {
        let mut query = self.world.query::<&Transform>();
        query.iter(&self.world).map(|t| t.chunk[2]).collect()
    }
}

// --- measurement ----------------------------------------------------------

/// Runs `body` enough times to be worth timing, and returns the per-run cost.
fn measure(mut body: impl FnMut()) -> Duration {
    // Warm, so the first run's allocation does not land in the measurement.
    for _ in 0..8 {
        body();
    }
    let mut runs = 16;
    loop {
        let start = Instant::now();
        for _ in 0..runs {
            body();
        }
        let elapsed = start.elapsed();
        if elapsed >= Duration::from_millis(50) || runs >= 1 << 20 {
            return elapsed / runs;
        }
        runs *= 4;
    }
}

fn microseconds(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1e6
}

/// Every measurement for one candidate at one entity count.
struct Row {
    step: Duration,
    spawn: Duration,
    despawn: Duration,
    within: Duration,
    attach: Duration,
    detach: Duration,
}

fn bench<S: Store + Default>(count: usize) -> Row {
    let mut resident = S::default();
    resident.populate(count);

    let step = measure(|| resident.step());
    let within = measure(|| {
        std::hint::black_box(resident.within(64.0));
    });

    let attach = measure(|| resident.attach_every_other());
    let detach = measure(|| resident.detach_all());

    let spawn = measure(|| {
        let mut store = S::default();
        store.populate(count);
        std::hint::black_box(store.order().len());
    });

    let despawn = measure(|| {
        let mut store = S::default();
        store.populate(count);
        store.despawn_every_other();
        std::hint::black_box(store.order().len());
    });

    Row {
        step,
        spawn,
        despawn,
        within,
        attach,
        detach,
    }
}

/// Whether two worlds built by the identical sequence iterate identically.
///
/// Returns the two orders when they differ, so the report can say HOW rather
/// than only that.
fn deterministic<S: Store + Default>() -> Result<(), (Vec<i32>, Vec<i32>)> {
    let build = || {
        let mut store = S::default();
        store.populate(GATE);
        // Churn, because an archetype store's iteration order is a function of
        // what has been added and removed, not only of what is there now.
        store.attach_every_other();
        store.despawn_every_other();
        store.populate(GATE / 4);
        store.detach_all();
        store.order()
    };
    let first = build();
    let second = build();
    if first == second {
        Ok(())
    } else {
        Err((first, second))
    }
}

fn report(name: &str, count: usize, row: &Row) {
    let tick = Duration::from_millis(50);
    let share = |value: Duration| value.as_secs_f64() / tick.as_secs_f64() * 100.0;
    println!(
        "| {name} | {count} | {:.2} µs ({:.3}%) | {:.2} µs | {:.2} µs | {:.2} µs | {:.2} µs | \
         {:.2} µs |",
        microseconds(row.step),
        share(row.step),
        microseconds(row.spawn),
        microseconds(row.despawn),
        microseconds(row.within),
        microseconds(row.attach),
        microseconds(row.detach),
    );
}

fn main() {
    println!("# ECS candidates, measured\n");
    println!("Tick budget is 50 ms (charter rule 18); `step` is reported as a share of it.\n");
    println!(
        "| store | entities | step | spawn all | spawn+despawn half | radius query | attach half \
         | detach all |"
    );
    println!("|---|---|---|---|---|---|---|---|");
    for count in [GATE, HEADROOM] {
        report("hand-rolled", count, &bench::<HandRolled>(count));
        report("hecs", count, &bench::<Hecs>(count));
        report("bevy_ecs", count, &bench::<Bevy>(count));
    }

    println!("\n## Deterministic iteration order\n");
    println!("Two worlds, identical call sequence, same process.\n");
    for (name, outcome) in [
        ("hand-rolled", deterministic::<HandRolled>()),
        ("hecs", deterministic::<Hecs>()),
        ("bevy_ecs", deterministic::<Bevy>()),
    ] {
        match outcome {
            Ok(()) => println!("- **{name}**: identical."),
            Err((first, second)) => {
                let at = first
                    .iter()
                    .zip(&second)
                    .position(|(a, b)| a != b)
                    .unwrap_or(first.len().min(second.len()));
                println!(
                    "- **{name}**: DIFFER, first at position {at} ({} entities vs {}).",
                    first.len(),
                    second.len()
                );
            }
        }
    }
}
