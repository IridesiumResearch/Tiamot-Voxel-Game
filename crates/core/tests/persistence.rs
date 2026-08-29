// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! End-to-end persistence tests.
//!
//! The unit tests in `persist::*` check each layer against itself. These check
//! the things that only go wrong when the layers meet: a world surviving a
//! process death mid-write, and a player's build surviving a mod being removed
//! and put back.

use std::path::{Path, PathBuf};

use proptest::prelude::*;

use tiamot_core::block::{EMPTY_CELLS, SUBNODES_PER_BLOCK};
use tiamot_core::chunk::Chunk;
use tiamot_core::coords::LocalBlock;
use tiamot_core::material::MaterialRegistry;
use tiamot_core::persist::{DEFAULT_DOMAIN, WorldDb};
use tiamot_core::{BLOCKS_PER_CHUNK, BlockValue, ChunkPos, MaterialId, Registry};

/// A unique scratch path per test, so tests can run in parallel.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-persist-tests");
    std::fs::create_dir_all(&dir).expect("scratch dir");
    let path = dir.join(format!("{name}.sqlite"));
    // WAL leaves sidecars; a stale one from a previous run would mask a bug.
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    path
}

fn registry_with(names: &[&str]) -> Registry {
    let mut registry = Registry::new();
    for name in names {
        registry.register(name).expect("register");
    }
    registry
}

// ---------------------------------------------------------------------------
// Round trip
// ---------------------------------------------------------------------------

/// Deliberately tiny material alphabet, as in the Task 02 property tests:
/// palette reuse and interning only happen when values repeat.
fn material(count: u16) -> impl Strategy<Value = MaterialId> {
    prop_oneof![
        3 => Just(MaterialId::AIR),
        1 => (2u16..2 + count).prop_map(MaterialId),
    ]
}

fn block_value(count: u16) -> impl Strategy<Value = BlockValue> {
    prop_oneof![
        4 => material(count).prop_map(BlockValue::Uniform),
        3 => (material(count), 0u32..=tiamot_core::block::OCCUPANCY_FULL)
            .prop_map(|(m, occupancy)| BlockValue::Partial { material: m, occupancy }),
        3 => proptest::collection::vec(material(count), SUBNODES_PER_BLOCK).prop_map(|drawn| {
            let mut cells = EMPTY_CELLS;
            cells.copy_from_slice(&drawn);
            BlockValue::Cells(cells)
        }),
    ]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// Any chunk survives a save and load byte-for-byte, in its exact internal
    /// state — not merely with equal contents.
    ///
    /// Exact state matters because the cross-platform determinism hash in
    /// Task 04 will run over chunk internals. A round trip that preserved
    /// contents but reshuffled the palette would make a world hash differently
    /// after being saved and reloaded.
    #[test]
    fn chunks_round_trip_through_the_database(
        writes in proptest::collection::vec((0usize..BLOCKS_PER_CHUNK, block_value(4)), 0..40),
    ) {
        let mut registry = registry_with(&["m:a", "m:b", "m:c", "m:d"]);
        let db = WorldDb::open_in_memory(&mut registry).expect("open");

        let pos = ChunkPos::new(3, -2, 7);
        let mut chunk = Chunk::air(pos);
        for (index, value) in writes {
            chunk.set_block_local(LocalBlock::from_index(index), value);
        }

        db.save_chunk(pos, &chunk).expect("save");
        let loaded = db.load_chunk(pos).expect("load").expect("present");
        prop_assert_eq!(chunk, loaded);
    }
}

#[test]
fn an_absent_chunk_loads_as_none() {
    let mut registry = registry_with(&[]);
    let db = WorldDb::open_in_memory(&mut registry).expect("open");
    assert!(
        db.load_chunk(ChunkPos::new(1, 2, 3))
            .expect("load")
            .is_none()
    );
}

#[test]
fn saving_the_same_chunk_twice_upserts_rather_than_failing() {
    let mut registry = registry_with(&["core:stone"]);
    let db = WorldDb::open_in_memory(&mut registry).expect("open");
    let stone = registry.id_of("core:stone").expect("registered");
    let pos = ChunkPos::new(0, 0, 0);

    db.save_chunk(pos, &Chunk::air(pos)).expect("first save");
    db.save_chunk(pos, &Chunk::new(pos, stone))
        .expect("second save");

    let loaded = db.load_chunk(pos).expect("load").expect("present");
    assert_eq!(loaded.is_uniform(), Some(stone));
}

#[test]
fn a_batch_save_writes_every_chunk() {
    let mut registry = registry_with(&["core:stone"]);
    let mut db = WorldDb::open_in_memory(&mut registry).expect("open");
    let stone = registry.id_of("core:stone").expect("registered");

    let chunks: Vec<(ChunkPos, Chunk)> = (0..64)
        .map(|i| {
            let pos = ChunkPos::new(i, 0, 0);
            (pos, Chunk::new(pos, stone))
        })
        .collect();

    let written = db
        .save_chunks_batch(chunks.iter().map(|(pos, chunk)| (*pos, chunk)))
        .expect("batch");
    assert_eq!(written, 64);

    for (pos, _) in &chunks {
        assert!(
            db.load_chunk(*pos).expect("load").is_some(),
            "missing {pos:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// Fluid. Not derived state — there is no function from terrain back to
// "somebody poured milk here", so a pond that is not written down is a pond
// that empties on restart.
// ---------------------------------------------------------------------------

#[test]
fn a_pond_survives_a_round_trip_through_the_database() {
    use tiamot_core::fluid::{Fluid, FluidId, FluidLayer, MAX_VOLUME};

    let mut registry = registry_with(&[]);
    let db = WorldDb::open_in_memory(&mut registry).expect("open");
    let pos = ChunkPos::new(4, -2, 9);
    let milk = FluidId(1);

    let mut layer = FluidLayer::empty();
    layer.set(LocalBlock::new(3, 0, 3), Fluid::new(milk, MAX_VOLUME));
    layer.set(LocalBlock::new(4, 0, 3), Fluid::new(milk, 13));
    layer.set(LocalBlock::new(5, 0, 3), Fluid::new(milk, 2));

    db.save_chunk_fluid(pos, &layer).expect("save");
    let loaded = db.load_chunk_fluid(pos).expect("load").expect("present");

    assert_eq!(loaded, layer);
    // Every distinction the word carries, not just "there is milk". Volume is
    // conserved, so a block that came back holding a different amount than it
    // was saved with is a conservation failure that survived a restart — the
    // hardest kind to find, because nothing in the running world did it.
    assert_eq!(loaded.get(LocalBlock::new(3, 0, 3)).volume(), MAX_VOLUME);
    assert_eq!(loaded.get(LocalBlock::new(4, 0, 3)).volume(), 13);
    assert_eq!(loaded.get(LocalBlock::new(5, 0, 3)).volume(), 2);
}

#[test]
fn a_dry_chunk_has_no_row_at_all() {
    // The reason fluid is its own table. A world of dry chunks must cost
    // nothing, and "nothing" means no row rather than a small row.
    let mut registry = registry_with(&[]);
    let db = WorldDb::open_in_memory(&mut registry).expect("open");
    let pos = ChunkPos::new(0, 0, 0);

    db.save_chunk_fluid(pos, &tiamot_core::fluid::FluidLayer::empty())
        .expect("save");
    assert!(db.load_chunk_fluid(pos).expect("load").is_none());
}

#[test]
fn a_pond_that_drains_does_not_come_back() {
    // **The bug this test exists for.** Save a pond, drain it, save again: if
    // the second save upserted an empty layer instead of deleting the row, or
    // skipped the write because there was nothing to write, the milk would
    // reappear the next time the chunk loaded.
    use tiamot_core::fluid::{Fluid, FluidId, FluidLayer, MAX_VOLUME};

    let mut registry = registry_with(&[]);
    let db = WorldDb::open_in_memory(&mut registry).expect("open");
    let pos = ChunkPos::new(1, 1, 1);

    let mut layer = FluidLayer::empty();
    layer.set(LocalBlock::new(0, 0, 0), Fluid::new(FluidId(1), MAX_VOLUME));
    db.save_chunk_fluid(pos, &layer).expect("save the pond");
    assert!(db.load_chunk_fluid(pos).expect("load").is_some());

    layer.set(LocalBlock::new(0, 0, 0), Fluid::EMPTY);
    assert!(layer.is_empty(), "the layer did not actually drain");
    db.save_chunk_fluid(pos, &layer).expect("save the drain");

    assert!(
        db.load_chunk_fluid(pos).expect("load").is_none(),
        "a drained pond came back from the database"
    );
}

#[test]
fn a_fluid_batch_writes_the_ponds_and_removes_the_drains_together() {
    use tiamot_core::fluid::{Fluid, FluidId, FluidLayer, MAX_VOLUME};

    let mut registry = registry_with(&[]);
    let mut db = WorldDb::open_in_memory(&mut registry).expect("open");
    let milk = FluidId(1);

    // Sixteen chunks with milk, saved first so there is something to remove.
    let mut wet = FluidLayer::empty();
    wet.set(LocalBlock::new(2, 2, 2), Fluid::new(milk, MAX_VOLUME));
    let positions: Vec<ChunkPos> = (0..16).map(|i| ChunkPos::new(i, 0, 0)).collect();
    let written = db
        .save_chunk_fluid_batch(positions.iter().map(|pos| (*pos, &wet)))
        .expect("first batch");
    assert_eq!(written, 16);

    // Now half of them drain, in the same batch as the half that stay.
    let dry = FluidLayer::empty();
    db.save_chunk_fluid_batch(
        positions
            .iter()
            .enumerate()
            .map(|(index, pos)| (*pos, if index % 2 == 0 { &dry } else { &wet })),
    )
    .expect("second batch");

    for (index, pos) in positions.iter().enumerate() {
        let loaded = db.load_chunk_fluid(*pos).expect("load");
        if index % 2 == 0 {
            assert!(loaded.is_none(), "{pos:?} should have drained");
        } else {
            assert_eq!(
                loaded.as_ref(),
                Some(&wet),
                "{pos:?} should still hold milk"
            );
        }
    }
}

#[test]
fn terrain_and_fluid_are_independent_rows() {
    // A chunk can hold milk with no terrain edits and terrain edits with no
    // milk, and neither save may disturb the other — which is the property that
    // makes them separate tables rather than one blob.
    use tiamot_core::fluid::{Fluid, FluidId, FluidLayer, MAX_VOLUME};

    let mut registry = registry_with(&["core:stone"]);
    let db = WorldDb::open_in_memory(&mut registry).expect("open");
    let stone = registry.id_of("core:stone").expect("registered");
    let pos = ChunkPos::new(7, 7, 7);

    let mut layer = FluidLayer::empty();
    layer.set(LocalBlock::new(1, 1, 1), Fluid::new(FluidId(1), MAX_VOLUME));
    db.save_chunk_fluid(pos, &layer).expect("fluid");
    db.save_chunk(pos, &Chunk::new(pos, stone))
        .expect("terrain");

    assert_eq!(
        db.load_chunk(pos)
            .expect("load")
            .expect("present")
            .is_uniform(),
        Some(stone)
    );
    assert_eq!(db.load_chunk_fluid(pos).expect("load"), Some(layer));

    // And a chunk with terrain but no milk answers `None` rather than erroring.
    let dry = ChunkPos::new(8, 8, 8);
    db.save_chunk(dry, &Chunk::new(dry, stone))
        .expect("terrain");
    assert!(db.load_chunk_fluid(dry).expect("load").is_none());
}

// ---------------------------------------------------------------------------
// Charter rule 8 for fluids: string ids are canonical, numbers are per session,
// and the world owns the mapping.
// ---------------------------------------------------------------------------

/// A fluid registration under a name, with milk-ish rules.
fn fluid_named(name: &str) -> tiamot_core::fluid::Registered {
    tiamot_core::fluid::Registered {
        name: name.to_owned(),
        waterlogs_at: 14,
        tick_rate: 1,
        evaporates: 0,
        color: [255, 255, 255],
        material: MaterialId(4),
    }
}

#[test]
fn a_pond_is_still_milk_after_another_mod_loads_ahead_of_it() {
    // **The defect this table exists to remove, end to end.**
    //
    // `Fluids::register` numbers positionally in registration order, and a fluid
    // byte carries that number. Before the world owned the mapping, adding a mod
    // that registered a fluid ahead of an existing one renumbered everything
    // after it — and the renumbering went straight to disk, so every stored pond
    // silently became a different fluid. The byte stays perfectly valid, which
    // is what makes it the kind of bug nobody finds by looking.
    use tiamot_core::fluid::{Fluid, FluidLayer, Fluids, MAX_VOLUME};

    let path = scratch("fluid-ids-reordered");
    let pos = ChunkPos::new(1, 0, 2);
    let block = LocalBlock::new(3, 4, 5);

    // Session one: milk alone, so it is fluid id 1.
    {
        let mut registry = registry_with(&[]);
        let mut db = WorldDb::open(&path, &mut registry).expect("open");
        let mut fluids = Fluids::new();
        fluids.register(fluid_named("core_milk:milk")).expect("reg");
        db.reconcile_fluids(&mut fluids).expect("reconcile");

        let milk = fluids.id_of("core_milk:milk").expect("registered");
        let mut layer = FluidLayer::empty();
        layer.set(block, Fluid::new(milk, MAX_VOLUME));
        db.save_chunk_fluid(pos, &layer).expect("save");
    }

    // Session two: a mod that registers a fluid alphabetically-first loads
    // ahead of milk, so milk is now id 2 in this session.
    let mut registry = registry_with(&[]);
    let mut db = WorldDb::open(&path, &mut registry).expect("reopen");
    let mut fluids = Fluids::new();
    fluids.register(fluid_named("acid:acid")).expect("reg");
    fluids.register(fluid_named("core_milk:milk")).expect("reg");
    assert_eq!(
        fluids.id_of("core_milk:milk"),
        Some(tiamot_core::fluid::FluidId(2)),
        "the staging is wrong: milk was supposed to be renumbered this session"
    );
    db.reconcile_fluids(&mut fluids).expect("reconcile");

    let loaded = db
        .load_chunk_fluid(pos)
        .expect("read")
        .expect("the pond is gone");
    let value = loaded.get(block);

    assert_eq!(
        value.fluid(),
        fluids.id_of("core_milk:milk").expect("registered"),
        "the pond came back as fluid {:?}, which this session calls {:?} — a mod \
         loading ahead of milk changed what somebody's lake is made of",
        value.fluid(),
        fluids
            .get(value.fluid())
            .map(|entry| entry.name.as_str())
            .unwrap_or("nothing at all")
    );
    assert_eq!(
        value.volume(),
        MAX_VOLUME,
        "the pond came back holding less than it was saved with"
    );
}

#[test]
fn a_pond_whose_mod_was_removed_survives_and_comes_back() {
    // Charter rule 8's round trip. Disabling a mod must not delete the world's
    // record of what it made — and re-enabling it must give the same blocks
    // back, unchanged.
    use tiamot_core::fluid::{Fluid, FluidLayer, Fluids};

    let path = scratch("fluid-mod-removed");
    let pos = ChunkPos::new(-4, 1, 0);
    let block = LocalBlock::new(1, 1, 1);

    // With the mod: pour some acid.
    {
        let mut registry = registry_with(&[]);
        let mut db = WorldDb::open(&path, &mut registry).expect("open");
        let mut fluids = Fluids::new();
        fluids.register(fluid_named("acid:acid")).expect("reg");
        db.reconcile_fluids(&mut fluids).expect("reconcile");

        let acid = fluids.id_of("acid:acid").expect("registered");
        let mut layer = FluidLayer::empty();
        layer.set(block, Fluid::new(acid, 5));
        db.save_chunk_fluid(pos, &layer).expect("save");
    }

    // Without it: the world still reads, and the block is held by a stand-in.
    {
        let mut registry = registry_with(&[]);
        let mut db = WorldDb::open(&path, &mut registry).expect("reopen");
        let mut fluids = Fluids::new();
        fluids.register(fluid_named("core_milk:milk")).expect("reg");
        db.reconcile_fluids(&mut fluids).expect("reconcile");

        let loaded = db
            .load_chunk_fluid(pos)
            .expect("a world must be able to read its own chunks after a mod is removed")
            .expect("the row is gone");
        let value = loaded.get(block);
        assert!(
            fluids.is_placeholder(value.fluid()),
            "acid came back as something other than a stand-in"
        );
        assert_eq!(value.volume(), 5, "the stand-in lost the volume");

        // Saving it back must not lose it either — the common case is a chunk
        // that gets rewritten for an unrelated reason.
        db.save_chunk_fluid(pos, &loaded).expect("save");
    }

    // The mod comes back, and so does the acid — unchanged, after a round trip
    // through a session that could not name it.
    let mut registry = registry_with(&[]);
    let mut db = WorldDb::open(&path, &mut registry).expect("reopen");
    let mut fluids = Fluids::new();
    fluids.register(fluid_named("core_milk:milk")).expect("reg");
    fluids.register(fluid_named("acid:acid")).expect("reg");
    db.reconcile_fluids(&mut fluids).expect("reconcile");

    let loaded = db.load_chunk_fluid(pos).expect("read").expect("the row");
    let value = loaded.get(block);
    assert_eq!(
        value.fluid(),
        fluids.id_of("acid:acid").expect("registered"),
        "the acid did not come back when its mod did"
    );
    assert_eq!(value.volume(), 5);
}

#[test]
fn a_world_with_no_fluid_needs_no_reconcile_at_all() {
    // The overwhelmingly common case, and the reason the map defaults to the
    // identity: a world whose mods registered no fluid, and which has never
    // stored one, must work with no ceremony.
    let mut registry = registry_with(&["core:stone"]);
    let db = WorldDb::open_in_memory(&mut registry).expect("open");
    assert!(!db.fluids_reconciled());

    let pos = ChunkPos::new(0, 0, 0);
    let stone = registry.id_of("core:stone").expect("registered");
    db.save_chunk(pos, &Chunk::new(pos, stone)).expect("save");
    assert!(db.load_chunk_fluid(pos).expect("read").is_none());
}

// ---------------------------------------------------------------------------
// The domain column (schema readiness for Task 15a — no domain logic yet)
// ---------------------------------------------------------------------------

#[test]
fn writes_default_to_the_overworld_domain() {
    let mut registry = registry_with(&["core:stone"]);
    let db = WorldDb::open_in_memory(&mut registry).expect("open");
    let stone = registry.id_of("core:stone").expect("registered");
    let pos = ChunkPos::new(5, 5, 5);

    db.save_chunk(pos, &Chunk::new(pos, stone)).expect("save");
    assert!(
        db.load_chunk_in(DEFAULT_DOMAIN, pos)
            .expect("load")
            .is_some(),
        "a default-domain write must be readable as 'overworld'"
    );
}

#[test]
fn a_second_domain_is_independent_and_does_not_collide() {
    let mut registry = registry_with(&["core:stone", "core:dirt"]);
    let db = WorldDb::open_in_memory(&mut registry).expect("open");
    let stone = registry.id_of("core:stone").expect("registered");
    let dirt = registry.id_of("core:dirt").expect("registered");
    let pos = ChunkPos::new(1, 1, 1);

    // Same coordinates, two domains. The composite primary key is what makes
    // this legal, and it is the whole reason the column is reserved now.
    db.save_chunk(pos, &Chunk::new(pos, stone))
        .expect("overworld");
    db.save_chunk_in("nether", pos, &Chunk::new(pos, dirt))
        .expect("second domain");

    assert_eq!(
        db.load_chunk(pos)
            .expect("load")
            .expect("present")
            .is_uniform(),
        Some(stone)
    );
    assert_eq!(
        db.load_chunk_in("nether", pos)
            .expect("load")
            .expect("present")
            .is_uniform(),
        Some(dirt),
        "the second domain must not have overwritten the first"
    );
}

// ---------------------------------------------------------------------------
// Mod churn (charter rule 8)
// ---------------------------------------------------------------------------

#[test]
fn a_build_survives_its_mod_being_removed_and_restored() {
    // The scenario the whole id-mapping design exists for. A player builds with
    // a mod, the mod is removed, they play on and the world is saved again, the
    // mod comes back. Their build must be exactly as they left it.
    let path = scratch("mod-churn");
    let pos = ChunkPos::new(2, 3, 4);

    // Session 1: build with the mod present.
    let expected = {
        let mut registry = registry_with(&["core:stone", "fancy:marble"]);
        let db = WorldDb::open(&path, &mut registry).expect("open");
        let stone = registry.id_of("core:stone").expect("registered");
        let marble = registry.id_of("fancy:marble").expect("registered");

        let mut chunk = Chunk::new(pos, stone);
        chunk.set_block_local(LocalBlock::new(1, 2, 3), BlockValue::Uniform(marble));
        let mut cells = [marble; SUBNODES_PER_BLOCK];
        cells[0] = stone;
        chunk.set_block_local(LocalBlock::new(4, 5, 6), BlockValue::Cells(cells));
        chunk.set_block_local(
            LocalBlock::new(7, 8, 9),
            BlockValue::Partial {
                material: marble,
                occupancy: 0b1011,
            },
        );

        db.save_chunk(pos, &chunk).expect("save");
        db.close().expect("close");
        chunk
    };

    // Session 2: the mod is gone. Load, touch nothing, save again.
    {
        let mut registry = registry_with(&["core:stone"]);
        let db = WorldDb::open(&path, &mut registry).expect("open without the mod");
        let chunk = db.load_chunk(pos).expect("load").expect("present");

        // The marble is present but has no material behind it this session.
        let view = chunk.get_block_local(LocalBlock::new(1, 2, 3));
        let material = view.subnode(0);
        assert!(
            db.materials().is_unknown(material),
            "an absent mod's material must be flagged unknown, not silently valid"
        );
        assert_ne!(material, MaterialId::AIR, "it must not have been erased");

        // Save it back untouched — this is the step that would destroy the
        // build if the mapping collapsed absent materials onto the placeholder.
        db.save_chunk(pos, &chunk).expect("save");
        db.close().expect("close");
    }

    // Session 3: the mod is back.
    {
        let mut registry = registry_with(&["core:stone", "fancy:marble"]);
        let db = WorldDb::open(&path, &mut registry).expect("open with the mod again");
        let restored = db.load_chunk(pos).expect("load").expect("present");
        assert_eq!(
            restored, expected,
            "the build must be byte-identical after a round trip without the mod"
        );
        assert!(
            db.materials().unknown().is_empty(),
            "nothing should be unknown once the mod is back"
        );
    }
}

// ---------------------------------------------------------------------------
// Crash safety
// ---------------------------------------------------------------------------

/// Set in the child process to tell it which world to crash while writing.
const CRASH_ENV: &str = "TIAMOT_CRASH_TARGET";

/// Meta key the child commits before starting the doomed write.
const CHILD_REACHED: &str = "test_child_reached";

/// Aborts the process mid-transaction.
///
/// Not a test of its own — it runs only in the child spawned by
/// [`a_crash_mid_write_leaves_the_world_intact`]. The `#[test]` attribute is how
/// it becomes reachable in the test binary at all.
#[test]
fn crash_child() {
    let Ok(target) = std::env::var(CRASH_ENV) else {
        // The ordinary case: this is the parent's test run, so do nothing.
        return;
    };

    let mut registry = registry_with(&["core:stone"]);
    let mut db = WorldDb::open(&target, &mut registry).expect("child open");
    let stone = registry.id_of("core:stone").expect("registered");

    // A committed breadcrumb, so the parent can prove the child really got
    // here. Without it, a child that failed to start would also exit non-zero
    // and the test would pass having tested nothing.
    db.set_meta(CHILD_REACHED, b"1").expect("breadcrumb");
    db.flush().expect("flush breadcrumb");

    // A large batch, so the process is reliably killed while the transaction is
    // still open rather than in the gap after a commit.
    let chunks: Vec<(ChunkPos, Chunk)> = (0..2000)
        .map(|i| {
            let pos = ChunkPos::new(1000 + i, 0, 0);
            (pos, Chunk::new(pos, stone))
        })
        .collect();

    std::thread::spawn(|| {
        // Give the batch a moment to open its transaction, then die hard.
        // SIGKILL-equivalent: no unwinding, no destructors, no SQLite cleanup —
        // exactly the failure WAL is supposed to survive.
        std::thread::sleep(std::time::Duration::from_millis(40));
        std::process::abort();
    });

    let _ = db.save_chunks_batch(chunks.iter().map(|(pos, chunk)| (*pos, chunk)));
    // If the batch somehow finished first, still die before a clean close.
    std::process::abort();
}

#[test]
fn a_crash_mid_write_leaves_the_world_intact() {
    if std::env::var(CRASH_ENV).is_ok() {
        return; // We are the child; `crash_child` is the one that acts.
    }

    let path = scratch("crash-safety");
    let pos = ChunkPos::new(-9, -9, -9);

    // Write something we care about, and close cleanly.
    {
        let mut registry = registry_with(&["core:stone"]);
        let db = WorldDb::open(&path, &mut registry).expect("open");
        let stone = registry.id_of("core:stone").expect("registered");
        db.save_chunk(pos, &Chunk::new(pos, stone)).expect("save");
        db.close().expect("close");
    }

    // Now have a child process die in the middle of a large write.
    let status = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["crash_child", "--exact", "--nocapture", "--test-threads=1"])
        .env(CRASH_ENV, &path)
        .output()
        .expect("spawn child");
    assert!(
        !status.status.success(),
        "the child was supposed to abort, not exit cleanly"
    );
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(
            status.status.signal(),
            Some(libc_sigabrt()),
            "the child should have died by abort, not exited with a code: {:?}\nstderr: {}",
            status.status,
            String::from_utf8_lossy(&status.stderr),
        );
    }

    // Reopen. The database must be structurally sound and the earlier data
    // must still be there.
    let mut registry = registry_with(&["core:stone"]);
    let db = WorldDb::open(&path, &mut registry).expect("reopen after crash");
    let stone = registry.id_of("core:stone").expect("registered");

    assert!(
        db.meta(CHILD_REACHED).expect("meta").is_some(),
        "the child never reached the write — this test would otherwise pass \
         without testing anything"
    );

    assert_eq!(
        db.load_chunk(pos)
            .expect("load")
            .expect("the pre-crash chunk must survive")
            .is_uniform(),
        Some(stone),
        "data committed before the crash must be intact"
    );

    // Whatever the interrupted batch managed to write, the file must be valid:
    // either the whole transaction landed or none of it did.
    let integrity = db.integrity_check().expect("integrity check");
    assert_eq!(
        integrity, "ok",
        "database failed integrity check after crash"
    );
}

/// `SIGABRT`, without taking a dependency on `libc` for one constant.
#[cfg(unix)]
const fn libc_sigabrt() -> i32 {
    6
}

// ---------------------------------------------------------------------------
// Players and identity (charter rule 13 storage)
// ---------------------------------------------------------------------------

#[test]
fn player_state_round_trips() {
    let mut registry = registry_with(&[]);
    let db = WorldDb::open_in_memory(&mut registry).expect("open");
    db.save_player("uuid-1", 1, b"state").expect("save");
    assert_eq!(
        db.load_player("uuid-1").expect("load").as_deref(),
        Some(&b"state"[..])
    );
    assert_eq!(db.load_player("absent").expect("load"), None);
}

#[test]
fn an_identity_holds_a_set_of_keys_with_a_single_root() {
    let mut registry = registry_with(&[]);
    let db = WorldDb::open_in_memory(&mut registry).expect("open");

    // Charter rule 13: the root key has no adder; every other key was added by
    // a signature from an existing one.
    db.add_player_key(&tiamot_core::persist::PlayerKey {
        uuid: "uuid-1",
        pubkey: b"root",
        next_key_hash: Some(b"hash-of-successor"),
        added_at: 100,
        added_by: None,
    })
    .expect("root key");
    db.add_player_key(&tiamot_core::persist::PlayerKey {
        uuid: "uuid-1",
        pubkey: b"second-device",
        next_key_hash: None,
        added_at: 200,
        added_by: Some(b"root"),
    })
    .expect("second key");

    let keys = db.player_keys("uuid-1").expect("keys");
    assert_eq!(keys.len(), 2);
    assert_eq!(keys[0].pubkey, b"root");
    assert_eq!(keys[0].added_by, None, "the root key has no adder");
    assert_eq!(
        keys[0].next_key_hash.as_deref(),
        Some(&b"hash-of-successor"[..])
    );
    assert_eq!(keys[1].added_by.as_deref(), Some(&b"root"[..]));
}

#[test]
fn revoking_a_key_hides_it_without_deleting_the_record() {
    let mut registry = registry_with(&[]);
    let db = WorldDb::open_in_memory(&mut registry).expect("open");
    db.add_player_key(&tiamot_core::persist::PlayerKey {
        uuid: "uuid-1",
        pubkey: b"lost-device",
        next_key_hash: None,
        added_at: 1,
        added_by: None,
    })
    .expect("add");

    db.revoke_player_key("uuid-1", b"lost-device", 500)
        .expect("revoke");
    assert!(
        db.player_keys("uuid-1").expect("keys").is_empty(),
        "a revoked key must not be returned as authorised"
    );
    // The row itself survives, so the key-set history stays replayable.
    assert_eq!(db.revoked_key_count("uuid-1").expect("count"), 1);
}

#[test]
fn a_display_name_has_exactly_one_holder() {
    let mut registry = registry_with(&[]);
    let db = WorldDb::open_in_memory(&mut registry).expect("open");
    db.claim_name("Bob", "uuid-1").expect("claim");
    assert_eq!(
        db.name_holder("Bob").expect("holder").as_deref(),
        Some("uuid-1")
    );
    assert!(
        db.claim_name("Bob", "uuid-2").is_err(),
        "a second identity must not be able to take a held name"
    );
}

// ---------------------------------------------------------------------------
// Meta
// ---------------------------------------------------------------------------

#[test]
fn the_world_seed_round_trips() {
    let path = scratch("meta");
    {
        let mut registry = registry_with(&[]);
        let db = WorldDb::open(&path, &mut registry).expect("open");
        db.set_world_seed(0xDEAD_BEEF_CAFE_F00D).expect("set");
        db.close().expect("close");
    }
    let mut registry = registry_with(&[]);
    let db = WorldDb::open(&path, &mut registry).expect("reopen");
    assert_eq!(db.world_seed().expect("seed"), Some(0xDEAD_BEEF_CAFE_F00D));
}

#[test]
fn a_world_file_is_created_on_a_path_that_does_not_exist_yet() {
    let dir = std::env::temp_dir().join("tiamot-persist-tests/nested/deeper");
    let _ = std::fs::remove_dir_all(&dir);
    let path = dir.join("world.sqlite");

    let mut registry = registry_with(&[]);
    let db = WorldDb::open(&path, &mut registry).expect("open should create the directory");
    db.close().expect("close");
    assert!(Path::new(&path).exists(), "the world file should exist");
}

// ---------------------------------------------------------------------------
// Entities, frozen with their chunk
// ---------------------------------------------------------------------------

/// A mob with everything filled in, so a round trip has something to lose.
fn furnished_mob(chunk: ChunkPos, label: &str) -> tiamot_core::ent::Entity {
    use tiamot_core::ent::{AnimTag, Collider, Entity, HUMANOID_MODEL, Health, Nametag, Transform};
    Entity {
        health: Some(Health::full(20)),
        nametag: Some(Nametag::Player(tiamot_core::PlayerUuid::from_bytes(
            [9; 32],
        ))),
        model: Some(HUMANOID_MODEL.to_owned()),
        collider: Some(Collider::HUMANOID),
        anim: AnimTag::SWIM,
        script: Some(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        ..Entity::at(Transform::at(chunk, [1.5, 2.5, 3.5]), label)
    }
}

#[test]
fn a_chunks_entities_survive_a_freeze_and_a_thaw() {
    let path = scratch("entity-freeze-thaw");
    let mut registry = registry_with(&["core:stone"]);
    let home = ChunkPos::new(4, 1, -2);

    // Freeze: the live store hands the chunk's entities over and the world
    // writes them. This is what a chunk unloading does.
    let frozen = {
        let mut world = tiamot_core::ent::Entities::new();
        world.spawn(furnished_mob(home, "test:first"));
        world.spawn(furnished_mob(ChunkPos::new(9, 9, 9), "test:elsewhere"));
        world.spawn(furnished_mob(home, "test:second"));

        let db = WorldDb::open(&path, &mut registry).expect("open");
        let taken = world.take_chunk(home);
        db.save_chunk_entities(home, &taken).expect("save");
        taken
    };
    assert_eq!(frozen.len(), 2);

    // Thaw, through a second `WorldDb` over the same file — the strong form,
    // since a single connection could be answering from its own cache.
    let db = WorldDb::open(&path, &mut registry).expect("reopen");
    let thawed = db.load_chunk_entities(home).expect("load");

    assert_eq!(
        thawed.iter().map(|e| e.source.as_str()).collect::<Vec<_>>(),
        vec!["test:first", "test:second"],
        "entities came back in a different order than they were frozen in, so \
         the iteration order every later tick sees depends on the database"
    );
    for (before, after) in frozen.iter().zip(&thawed) {
        assert_eq!(after.transform.chunk, before.transform.chunk);
        assert_eq!(after.nametag, before.nametag);
        assert_eq!(after.health, before.health);
        assert_eq!(after.model, before.model);
        assert_eq!(after.script, before.script);
        assert_eq!(after.owner, before.owner);
    }

    // **The one deliberate loss.** A mob thawing has no business resuming the
    // swim it was in the middle of, and not writing the tag is what keeps a
    // per-session number out of the world file.
    assert!(
        thawed
            .iter()
            .all(|e| e.anim == tiamot_core::ent::AnimTag::IDLE),
        "an animation tag survived to disk"
    );
}

#[test]
fn saving_a_chunks_entities_replaces_them_rather_than_adding_to_them() {
    // **The bug this is here to prevent**: a mob wanders from chunk A into
    // chunk B, both are saved, and the copy in A is never removed — so the
    // world slowly fills with duplicates of everything that ever moved.
    let path = scratch("entity-replace");
    let mut registry = registry_with(&["core:stone"]);
    let db = WorldDb::open(&path, &mut registry).expect("open");
    let home = ChunkPos::new(0, 0, 0);

    db.save_chunk_entities(home, &[furnished_mob(home, "test:a")])
        .expect("first save");
    db.save_chunk_entities(home, &[furnished_mob(home, "test:b")])
        .expect("second save");

    let loaded = db.load_chunk_entities(home).expect("load");
    assert_eq!(
        loaded.iter().map(|e| e.source.as_str()).collect::<Vec<_>>(),
        vec!["test:b"],
        "the first save's entity was still there beside the second's"
    );

    // And an empty save is a delete, so a chunk nothing lives in costs nothing.
    db.save_chunk_entities(home, &[]).expect("empty save");
    assert!(db.load_chunk_entities(home).expect("load").is_empty());
    assert!(
        db.chunks_with_entities().expect("index").is_empty(),
        "a chunk with no entities still has rows"
    );
}

#[test]
fn one_chunks_entities_do_not_disturb_another_chunks() {
    let path = scratch("entity-chunk-isolation");
    let mut registry = registry_with(&["core:stone"]);
    let db = WorldDb::open(&path, &mut registry).expect("open");
    let here = ChunkPos::new(1, 0, 0);
    let there = ChunkPos::new(2, 0, 0);

    db.save_chunk_entities(here, &[furnished_mob(here, "test:here")])
        .expect("save here");
    db.save_chunk_entities(there, &[furnished_mob(there, "test:there")])
        .expect("save there");
    // Rewriting one must not touch the other, which is what a per-chunk DELETE
    // scoped too widely would do.
    db.save_chunk_entities(here, &[]).expect("clear here");

    assert!(db.load_chunk_entities(here).expect("load").is_empty());
    assert_eq!(
        db.load_chunk_entities(there).expect("load").len(),
        1,
        "clearing one chunk removed another chunk's entities"
    );
    assert_eq!(db.chunks_with_entities().expect("index"), vec![there]);
}

#[test]
fn an_entity_written_by_a_newer_format_is_refused_rather_than_guessed_at() {
    // Everything on disk is untrusted (see the `persist` module docs). A blob
    // stamped with a version this build does not write is a world from a newer
    // engine, and decoding it as though it were current is how a save file
    // becomes quietly wrong instead of loudly unreadable.
    let path = scratch("entity-future-version");
    let mut registry = registry_with(&["core:stone"]);
    let home = ChunkPos::new(0, 0, 0);

    {
        let db = WorldDb::open(&path, &mut registry).expect("open");
        db.save_chunk_entities(home, &[furnished_mob(home, "test:a")])
            .expect("save");
    }
    // Reach past the API deliberately: there is no supported way to write a
    // future version, which is the point.
    let conn = rusqlite::Connection::open(&path).expect("raw open");
    conn.execute(
        "UPDATE entities SET version = ?1",
        [i64::from(tiamot_core::persist::ENTITY_FORMAT_VERSION) + 1],
    )
    .expect("bump");
    drop(conn);

    let db = WorldDb::open(&path, &mut registry).expect("reopen");
    let err = db.load_chunk_entities(home).expect_err("should refuse");
    assert!(
        err.to_string().contains("entity in chunk"),
        "the error does not say which chunk: {err}"
    );
}

// ---------------------------------------------------------------------------
// Mod storage
// ---------------------------------------------------------------------------

#[test]
fn a_mods_facts_survive_a_restart_and_stay_its_own() {
    // A mob imprinting on a player is exactly this: a fact about the world that
    // is not attached to a block, a chunk or an entity, and that has to be the
    // same fact after the server comes back up.
    use tiamot_core::storage::{Bag, Value};

    let path = scratch("mod-storage");
    let mut registry = registry_with(&["core:stone"]);
    let uuid = tiamot_core::PlayerUuid::from_bytes([0x5A; 32]);

    {
        let db = WorldDb::open(&path, &mut registry).expect("open");
        let mut mine = Bag::new();
        mine.insert("imprint".into(), Value::uuid(uuid));
        mine.insert("greeted".into(), Value::Flag(true));
        mine.insert("count".into(), Value::Number(3.5));
        db.save_mod_storage("a_mod", &mine).expect("save mine");

        let mut theirs = Bag::new();
        theirs.insert("imprint".into(), Value::Text("not a uuid".into()));
        db.save_mod_storage("someone_else", &theirs)
            .expect("save theirs");
    }

    let db = WorldDb::open(&path, &mut registry).expect("reopen");
    let mine = db.load_mod_storage("a_mod").expect("load mine");
    assert_eq!(
        mine.get("imprint").and_then(Value::as_uuid),
        Some(uuid),
        "the imprint did not survive the restart as the same player"
    );
    assert_eq!(mine.get("greeted"), Some(&Value::Flag(true)));
    assert_eq!(mine.get("count").and_then(Value::as_number), Some(3.5));
    assert_eq!(
        mine.keys().cloned().collect::<Vec<_>>(),
        vec![
            "count".to_owned(),
            "greeted".to_owned(),
            "imprint".to_owned()
        ],
        "keys must come back in order, or a mod iterating them sees a different \
         world on two runs"
    );

    // One mod's key of the same name is a different fact.
    assert_eq!(
        db.load_mod_storage("someone_else")
            .expect("load theirs")
            .get("imprint")
            .and_then(Value::as_uuid),
        None
    );
    assert_eq!(
        db.mods_with_storage().expect("index"),
        vec!["a_mod".to_owned(), "someone_else".to_owned()]
    );
}

#[test]
fn saving_a_mods_storage_replaces_it_rather_than_merging() {
    // The caller holds the whole bag in memory, so a merge would leave a
    // deleted key on disk for ever — and the next load would bring it back.
    use tiamot_core::storage::{Bag, Value};

    let path = scratch("mod-storage-replace");
    let mut registry = registry_with(&["core:stone"]);
    let db = WorldDb::open(&path, &mut registry).expect("open");

    let mut first = Bag::new();
    first.insert("gone".into(), Value::Flag(true));
    first.insert("kept".into(), Value::Number(1.0));
    db.save_mod_storage("keeper", &first).expect("first save");

    let mut second = Bag::new();
    second.insert("kept".into(), Value::Number(2.0));
    db.save_mod_storage("keeper", &second).expect("second save");

    let loaded = db.load_mod_storage("keeper").expect("load");
    assert_eq!(
        loaded.keys().cloned().collect::<Vec<_>>(),
        vec!["kept".to_owned()],
        "a deleted key came back"
    );
    assert_eq!(loaded.get("kept").and_then(Value::as_number), Some(2.0));
}

// ---------------------------------------------------------------------------
// Domains
// ---------------------------------------------------------------------------

#[test]
fn the_domain_instances_round_trip() {
    // Which instances exist has to survive a restart, or a ship somebody built
    // is a domain nothing can name the next morning.
    let path = scratch("domain-instances");
    let saved = vec![
        ("mod:ship/17".to_owned(), "mod:ship".to_owned()),
        ("mod:ship/18".to_owned(), "mod:ship".to_owned()),
    ];
    {
        let mut registry = registry_with(&[]);
        let db = WorldDb::open(&path, &mut registry).expect("open");
        db.set_domain_instances(&saved).expect("set");
        db.close().expect("close");
    }
    let mut registry = registry_with(&[]);
    let db = WorldDb::open(&path, &mut registry).expect("reopen");
    assert_eq!(db.domain_instances().expect("read"), saved);
}

#[test]
fn a_world_with_no_instances_reads_as_none_rather_than_failing() {
    // Every world written before this feature existed has no such key, and
    // every one of them must open.
    let path = scratch("domain-instances-absent");
    let mut registry = registry_with(&[]);
    let db = WorldDb::open(&path, &mut registry).expect("open");
    assert_eq!(db.domain_instances().expect("read"), Vec::new());
}

#[test]
fn an_unreadable_instance_line_costs_that_line_and_not_the_world() {
    // A world that will not open because one line of a side table is malformed
    // is worse than one that opens with a ship missing — and the ship's chunks
    // are still there either way, as a domain nothing can name.
    let path = scratch("domain-instances-malformed");
    let mut registry = registry_with(&[]);
    let db = WorldDb::open(&path, &mut registry).expect("open");
    db.set_meta(
        "domain_instances",
        b"mod:ship/17\tmod:ship\nrubbish with no tab\n\tno instance\nmod:ship/18\tmod:ship",
    )
    .expect("set");
    assert_eq!(
        db.domain_instances().expect("read"),
        vec![
            ("mod:ship/17".to_owned(), "mod:ship".to_owned()),
            ("mod:ship/18".to_owned(), "mod:ship".to_owned()),
        ]
    );
}

#[test]
fn every_domain_with_anything_in_it_is_found_from_the_tables() {
    // **Read from the tables and not from a list**, because the list is the
    // thing that can be wrong. A domain with chunks in it exists whatever any
    // registry says, and this is how one whose mod was removed is found and
    // kept rather than quietly orphaned (charter rule 8).
    let path = scratch("stored-domains");
    let mut registry = registry_with(&["test:stone"]);
    let stone = registry.register("test:stone").expect("register");
    let db = WorldDb::open(&path, &mut registry).expect("open");

    let pos = ChunkPos::new(0, 0, 0);
    db.save_chunk(pos, &Chunk::new(pos, stone)).expect("save");
    db.save_chunk_in("mod:ship/17", pos, &Chunk::new(pos, stone))
        .expect("save");
    db.save_chunk_in("gone:place", pos, &Chunk::new(pos, stone))
        .expect("save");

    assert_eq!(
        db.stored_domains().expect("read"),
        vec![
            "gone:place".to_owned(),
            "mod:ship/17".to_owned(),
            DEFAULT_DOMAIN.to_owned(),
        ],
        "a domain with chunks in it went unlisted, so its data could be orphaned"
    );
}

#[test]
fn a_fresh_world_stores_nothing_under_any_domain() {
    // Criterion: a registered but never-visited domain costs zero rows. The
    // overworld itself is not listed until something is written to it, which is
    // the same statement about lazy instantiation.
    let path = scratch("stored-domains-empty");
    let mut registry = registry_with(&[]);
    let db = WorldDb::open(&path, &mut registry).expect("open");
    assert!(
        db.stored_domains().expect("read").is_empty(),
        "an untouched world already had storage under a domain"
    );
}
