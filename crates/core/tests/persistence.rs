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
