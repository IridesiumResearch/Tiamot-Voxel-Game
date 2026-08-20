// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The client's connection against a real server.
//!
//! Not a mock. `ServerHandle::start` is the same entry point the standalone
//! binary uses (charter rule 2), so what these tests drive is the shipping
//! server over a real QUIC loopback — the join flow, the material table, the
//! content cache, chunk streaming, and live edits.
//!
//! This is the headless half of Task 08's acceptance criteria: everything about
//! "connect to a server and receive a world" that does not need a GPU.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use client::cache::ContentCache;
use client::net::{Connection, Event};
use client::world::ChunkStore;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::proto::MaterialDef;
use tiamot_core::{BlockPos, MaterialId};
use tiamot_server::{ServerHandle, Settings};

/// Long enough for a cold start under a loaded CI runner, short enough that a
/// hang is a failure rather than a timeout of the whole suite.
const PATIENCE: Duration = Duration::from_secs(20);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-client-net").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn reference_mods() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../game")
        .canonicalize()
        .expect("the reference mods live at the repo root")
}

fn start(name: &str) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(&format!("{name}-world")),
        max_players: 4,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(reference_mods()),
        seed: Some(4242),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("start")
}

/// A server whose mod directory is a copy of `game/` with one mod removed.
///
/// **This is how the sky criterion is checked**, and it is checked by absence:
/// the claim is that sky content lives in a mod, and the way to test that is to
/// take the mod away and watch the day disappear while everything else keeps
/// working.
fn start_without(name: &str, omit: &str) -> ServerHandle {
    let mods = scratch(&format!("{name}-mods"));
    let _ = std::fs::remove_dir_all(&mods);
    std::fs::create_dir_all(&mods).expect("mod dir");
    for entry in std::fs::read_dir(reference_mods()).expect("read game/") {
        let entry = entry.expect("entry");
        if !entry.path().is_dir() || entry.file_name() == omit {
            continue;
        }
        let target = mods.join(entry.file_name());
        std::fs::create_dir_all(&target).expect("mod dir");
        for file in std::fs::read_dir(entry.path()).expect("read mod") {
            let file = file.expect("entry");
            if file.path().is_file() {
                std::fs::copy(file.path(), target.join(file.file_name())).expect("copy");
            }
        }
    }

    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(&format!("{name}-world")),
        max_players: 4,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: Some(mods),
        seed: Some(4242),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("start")
}

/// One client's data directory: an identity, a content cache, a known-hosts file.
struct Home {
    root: PathBuf,
}

impl Home {
    fn new(name: &str) -> Self {
        Self {
            root: scratch(&format!("{name}-home")),
        }
    }

    fn cache(&self) -> ContentCache {
        ContentCache::open(&self.root.join("content")).expect("cache")
    }

    fn trust(&self) -> PathBuf {
        self.root.join("known-hosts")
    }

    /// Opens a connection under a distinct display name.
    ///
    /// Distinct because a name is bound to the UUID that first claimed it
    /// (charter rule 13), and every `Identity::generate` here is a different
    /// player — a second connection reusing the name is name theft, and the
    /// server is right to refuse it.
    fn open_as(&self, server: &ServerHandle, name: &str) -> Connection {
        Connection::open(
            server.local_addr(),
            Identity::generate().expect("identity"),
            name.to_owned(),
            self.cache(),
            &self.trust(),
        )
        .expect("connect")
    }

    fn open(&self, server: &ServerHandle) -> Connection {
        self.open_as(server, "Viewer")
    }
}

/// Everything the client learned, accumulated the way a render loop would.
#[derive(Default)]
struct Seen {
    connected: Option<(String, bool)>,
    table: Vec<MaterialDef>,
    images: BTreeMap<u16, client::texture::Image>,
    joined: Option<BlockPos>,
    /// Whether the material table arrived while the client was still outside
    /// the world.
    ///
    /// Recorded as events are applied rather than checked afterwards: a render
    /// loop drains the whole queue in one pass, so by the time anything looks
    /// at the accumulated state both have landed and the ORDER — which is the
    /// property under test — is gone.
    table_before_join: Option<bool>,
    store: ChunkStore,
    warnings: Vec<String>,
    disconnect: Option<String>,
    /// Authoritative positions, in the order they arrived.
    states: Vec<client::predict::Authoritative>,
    /// The tool table, which charter rule 1 says only the mods can supply.
    tools: Vec<tiamot_core::proto::ToolDef>,
    actions: Vec<tiamot_core::proto::ActionDef>,
    sounds: Vec<tiamot_core::proto::SoundDef>,
    heard: u32,
    decoded: u32,
    /// The sky, which charter rule 1 says the same about.
    sky: Option<client::sky::Sky>,
    /// The most recent time of day the server sent.
    time_of_day: Option<f32>,
    /// The radius the server said it is streaming at.
    view_distance: Option<(u8, u8)>,
    entities: client::entities::Entities,
    dialogs: std::collections::BTreeMap<String, tiamot_core::ui::Tree>,
}

impl Seen {
    fn apply(&mut self, event: Event) {
        match event {
            Event::Connected {
                address, first_use, ..
            } => self.connected = Some((address, first_use)),
            Event::Materials { table, images } => {
                self.table_before_join = Some(self.joined.is_none());
                self.table = table;
                self.images = images;
            }
            Event::Joined { spawn, .. } => self.joined = Some(spawn),
            Event::Dialog { form, tree } => {
                self.dialogs.insert(form, *tree);
            }
            Event::DialogClosed { form } => {
                self.dialogs.remove(&form);
            }
            Event::Chunk(chunk) => self.store.insert(*chunk),
            Event::ChunkLight(pos, layer) => self.store.set_light(pos, *layer),
            Event::ChunkFluid(pos, layer) => self.store.set_fluid(pos, *layer),
            Event::EntitySpawn(entities) => {
                self.entities.spawned(&entities, std::time::Duration::ZERO);
            }
            Event::EntityDespawn(ids) => self.entities.despawned(&ids),
            Event::EntityState { tick, entities } => {
                self.entities
                    .moved(tick, &entities, std::time::Duration::ZERO);
            }
            Event::Fluids { fluids } => self.store.set_fluid_table(&fluids),
            Event::Sky(sky) => self.sky = Some(sky),
            Event::ViewDistance {
                horizontal,
                vertical,
            } => self.view_distance = Some((horizontal, vertical)),
            Event::TimeOfDay(time) => self.time_of_day = Some(time),
            Event::ChunkUnload(pos) => {
                self.store.remove(pos);
            }
            Event::Edit(edit) => {
                self.store.apply(&edit);
            }
            Event::Warning(text) => self.warnings.push(text),
            Event::Disconnected { reason } => self.disconnect = Some(reason),
            Event::PlayerState(state) => self.states.push(state),
            Event::DigProgress { .. } => {}
            Event::Inventory { .. } => {}
            Event::Tools { tools } => self.tools = tools,
            // Recorded like the tools table: this harness asserts on what
            // arrives, and an action set is one of the things that does.
            Event::Actions { actions } => self.actions = actions,
            Event::Sounds { sounds } => self.sounds = sounds,
            // Recorded like everything else this harness asserts on.
            Event::PlaySound { .. } => self.heard += 1,
            Event::SoundReady { .. } => self.decoded += 1,
            Event::Chat { .. } => {}
        }
    }
}

/// Pumps events until `done` or the deadline, the way a render loop would.
fn pump(connection: &mut Connection, seen: &mut Seen, done: impl Fn(&Seen) -> bool) -> bool {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        while let Some(event) = connection.poll() {
            seen.apply(event);
        }
        if done(seen) {
            return true;
        }
        if let Some(reason) = &seen.disconnect {
            panic!("the connection ended before the test finished: {reason}");
        }
        // A frame at 60 Hz. Polling in a tight loop would test a spin, not a
        // render loop.
        std::thread::sleep(Duration::from_millis(16));
    }
    false
}

#[test]
fn a_client_joins_a_real_server_and_receives_a_world() {
    // The acceptance criterion, minus the pixels: connect to a server the same
    // build starts, complete the join flow, and end up holding chunks.
    let server = start("join");
    let home = Home::new("join");
    let mut connection = home.open(&server);
    let mut seen = Seen::default();

    assert!(
        pump(&mut connection, &mut seen, |seen| seen.joined.is_some()
            && seen.store.len() >= 4),
        "expected to join and receive chunks; got joined={:?} chunks={} warnings={:?}",
        seen.joined,
        seen.store.len(),
        seen.warnings
    );

    let (address, first_use) = seen.connected.clone().expect("a Connected event");
    assert_eq!(address, server.local_addr().to_string());
    assert!(
        first_use,
        "a fresh data directory has never seen this server"
    );

    assert!(
        seen.warnings.is_empty(),
        "a clean join should produce no warnings: {:?}",
        seen.warnings
    );

    connection.shutdown();
    assert!(server.stop());
}

#[test]
fn the_material_table_and_its_textures_arrive_before_the_world() {
    // The ordering that makes an atlas possible: a renderer cannot size or fill
    // one without the table, and a chunk that arrived first would be a grid of
    // numbers with no names.
    let server = start("materials");
    let home = Home::new("materials");
    let mut connection = home.open(&server);
    let mut seen = Seen::default();

    assert!(
        pump(&mut connection, &mut seen, |seen| !seen.table.is_empty()),
        "the material table must arrive"
    );
    assert_eq!(
        seen.table_before_join,
        Some(true),
        "and it must arrive BEFORE the world, or the first chunk cannot be drawn"
    );

    let white = seen
        .table
        .iter()
        .find(|entry| entry.name == "core:white")
        .expect("the reference mod's block");
    assert_eq!(
        seen.images.get(&white.id),
        Some(&client::texture::Image::white_with_border()),
        "the texture the mod ships must arrive decoded and intact"
    );

    connection.shutdown();
    assert!(server.stop());
}

#[test]
fn a_second_connection_asks_for_nothing_it_already_has() {
    // Content addressing's payoff, and what makes rejoining a server you have
    // played on before fast.
    //
    // Asserted on the CACHE rather than on the wire: "did the client send a
    // ContentRequest" is not observable from outside the connection, whereas
    // "is there anything left to ask for" is the exact question the client asks
    // itself before deciding, and answering it wrongly is the only way a
    // redundant request happens.
    let server = start("cache");
    let home = Home::new("cache");

    let mut first = home.open_as(&server, "First");
    let mut seen = Seen::default();
    assert!(
        pump(&mut first, &mut seen, |seen| !seen.images.is_empty()),
        "the first connection must fetch the textures"
    );
    first.shutdown();

    let wanted: Vec<_> = seen
        .table
        .iter()
        .filter_map(|entry| entry.texture)
        .collect();
    assert!(!wanted.is_empty(), "the reference mods ship a texture");
    assert!(
        home.cache().missing(&wanted).is_empty(),
        "after one join there should be nothing left to ask for"
    );

    let mut second = home.open_as(&server, "Second");
    let mut again = Seen::default();
    assert!(
        pump(&mut second, &mut again, |seen| !seen.images.is_empty()),
        "the second connection must still end up with textures"
    );
    assert_eq!(
        again.images, seen.images,
        "and they must be the same images, served from disk"
    );

    second.shutdown();
    assert!(server.stop());
}

#[test]
fn a_server_is_pinned_on_first_use_and_matches_on_the_next_visit() {
    // Trust on first use. The second connection is the one that would fail if
    // the fingerprint were not recorded, or were recorded wrong.
    let server = start("tofu");
    let home = Home::new("tofu");

    let mut first = home.open_as(&server, "First");
    let mut seen = Seen::default();
    assert!(pump(&mut first, &mut seen, |seen| seen.connected.is_some()));
    assert_eq!(seen.connected.as_ref().map(|(_, first)| *first), Some(true));
    first.shutdown();

    let recorded = std::fs::read_to_string(home.trust()).expect("known-hosts was written");
    assert!(
        recorded.contains(&server.local_addr().to_string()),
        "the address must be recorded: {recorded}"
    );

    let mut second = home.open(&server);
    let mut again = Seen::default();
    assert!(pump(&mut second, &mut again, |seen| seen
        .connected
        .is_some()));
    assert_eq!(
        again.connected.as_ref().map(|(_, first)| *first),
        Some(false),
        "the second visit is not a first use"
    );

    second.shutdown();
    assert!(server.stop());
}

#[test]
fn a_pinned_fingerprint_that_no_longer_matches_refuses_the_connection() {
    // The property the whole trust store exists for. A client that reconnected
    // happily to a different certificate would make the pin decorative.
    let server = start("pin-mismatch");
    let home = Home::new("pin-mismatch");

    // Pin something the server cannot present.
    let mut store = client::trust::TrustStore::load(&home.trust());
    store.remember(&server.local_addr().to_string(), [0x5Au8; 32]);
    store.save().expect("save");

    let err = Connection::open(
        server.local_addr(),
        Identity::generate().expect("identity"),
        "Viewer".to_owned(),
        home.cache(),
        &home.trust(),
    )
    .expect_err("a changed fingerprint must refuse the connection");

    let text = err.to_string();
    assert!(
        text.contains("has CHANGED") && text.contains("remove this address"),
        "the refusal has to tell the player what to do about it: {text}"
    );

    assert!(server.stop());
}

#[test]
fn an_edit_arrives_and_dirties_exactly_the_chunk_it_landed_in() {
    // The remesh path, headless. A renderer rebuilds whatever comes out of the
    // dirty queue, so proving the edit lands and marks the right chunk proves
    // everything upstream of the GPU.
    let server = start("edit");
    let home = Home::new("edit");
    let mut connection = home.open(&server);
    let mut seen = Seen::default();

    assert!(
        pump(&mut connection, &mut seen, |seen| seen.joined.is_some()
            && seen.store.len() >= 4),
        "join and stream first"
    );

    // Settle the queue so what follows is attributable to the edit alone.
    let spawn = seen.joined.expect("joined");
    while !seen.store.take_dirty(spawn.chunk(), 1024).is_empty() {
        while let Some(event) = connection.poll() {
            seen.apply(event);
        }
    }

    let target = BlockPos::new(spawn.x, spawn.y, spawn.z);
    let stone = seen
        .table
        .iter()
        .find(|entry| entry.name == "core:white")
        .map(|entry| entry.id)
        .expect("core:white");
    // Seeded by the OPERATOR. A client cannot edit the world any more, and
    // what this test is about is the remesh path — that an edit ARRIVING marks
    // the right chunk dirty — which does not care who made it.
    assert!(server.seed_block(target, stone), "seed queue full");

    assert!(
        pump(&mut connection, &mut seen, |seen| {
            seen.store
                .get(target.chunk())
                .and_then(|chunk| chunk.get_block(target))
                .is_some_and(|view| view.subnode(0) == MaterialId(stone))
        }),
        "the edit must come back from the server and land in the local copy"
    );

    let dirty = seen.store.take_dirty(target.chunk(), 1024);
    assert!(
        dirty.contains(&target.chunk()),
        "the chunk the edit landed in must be queued for a remesh: {dirty:?}"
    );

    connection.shutdown();
    assert!(server.stop());
}

#[test]
fn the_sky_comes_from_a_mod_and_goes_when_the_mod_does() {
    // **Task 10's fourth acceptance criterion, checked by absence.** The claim
    // is that sky content — how long a day is, what colour it goes — lives in
    // `game/core_sky` and not in the engine. Deleting the directory is the test:
    // the world keeps its light, its lamps and its terrain, and simply stops
    // having a day.
    //
    // The positive half first, or the negative half proves nothing.
    let server = start("sky-present");
    let home = Home::new("sky-present");
    let mut connection = home.open(&server);
    let mut seen = Seen::default();

    assert!(
        pump(&mut connection, &mut seen, |seen| seen.sky.is_some()
            && seen.joined.is_some()),
        "the server never sent a sky; warnings={:?}",
        seen.warnings
    );
    let sky = seen.sky.clone().expect("a sky");
    assert!(
        sky.has_day(),
        "the reference mods registered a sky but it has no day"
    );
    // And it really is the mod's day rather than something the engine invented:
    // the colours change across it.
    let mut walked = sky.clone();
    walked.set_time(0.0);
    let midnight = walked.moment();
    walked.set_time(0.5);
    let noon = walked.moment();
    assert!(
        noon.intensity > midnight.intensity + 0.5,
        "noon is not brighter than midnight: {} against {}",
        noon.intensity,
        midnight.intensity
    );

    connection.shutdown();
    assert!(server.stop());

    // Now the same world with the sky mod deleted.
    let server = start_without("sky-absent", "core_sky");
    let home = Home::new("sky-absent");
    let mut connection = home.open(&server);
    let mut seen = Seen::default();

    assert!(
        pump(&mut connection, &mut seen, |seen| seen.joined.is_some()
            && !seen.store.is_empty()),
        "the client should still join a world with no sky mod; warnings={:?}",
        seen.warnings
    );
    let sky = seen
        .sky
        .clone()
        .expect("a sky table is still sent, just an empty one");
    assert!(
        !sky.has_day(),
        "the day survived deleting the only mod that defines one"
    );

    // The rest of the world is untouched, which is what makes this about the
    // sky rather than about the server failing to start.
    assert!(
        seen.warnings.is_empty(),
        "removing the sky mod produced warnings: {:?}",
        seen.warnings
    );
    assert!(!seen.tools.is_empty(), "the tools mod went with it");

    connection.shutdown();
    assert!(server.stop());
}

#[test]
fn connecting_to_nothing_fails_with_an_address_rather_than_hanging() {
    // A client that hung on an unreachable server would look identical to one
    // that had crashed. Port 1 on loopback is reserved and never listening.
    let home = Home::new("unreachable");
    let err = Connection::open(
        "127.0.0.1:1".parse().expect("addr"),
        Identity::generate().expect("identity"),
        "Viewer".to_owned(),
        home.cache(),
        &home.trust(),
    )
    .expect_err("nothing is listening there");

    assert!(
        err.to_string().contains("127.0.0.1:1"),
        "the error must name what it could not reach: {err}"
    );
}
