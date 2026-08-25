// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Regenerates the `proto_decode` fuzz corpus seeds.
//!
//! Run: `cargo run --release -p tiamot-core --example fuzz_seeds -- fuzz/corpus/proto_decode`
//!
//! # Why seeds matter more than corpus size
//!
//! A fuzzer starting from random bytes spends most of its budget discovering
//! that `postcard` rejects almost everything. Seeded with **valid encodings of
//! every message variant**, it starts inside the space where the interesting
//! bugs are — a length field one byte too large, a nested `Vec` claiming more
//! elements than it carries, an enum ordinal past the end — and mutates
//! outwards from there.
//!
//! Which is why this is regenerated whenever a message shape changes. A corpus
//! seeded for the protocol as it was two tasks ago is a corpus exercising a
//! decoder that no longer exists: it would still run clean, and prove nothing
//! about the variants added since.

use std::path::PathBuf;

use tiamot_core::proto::{
    ActionDef, Click, ClientMessage, DialogEvent, DisconnectReason, Edit, EntityDef, EntityDelta,
    FluidDef, HudScriptDef, MaterialDef, ModEntry, PROTOCOL_VERSION, ServerMessage, SkyFrame,
    SkyGrade, SoundDef, WireSignature, encode,
};
use tiamot_core::{BlockPos, ChunkPos, SubNodePos};

fn main() {
    let mut args = std::env::args().skip(1);
    let out = args
        .next()
        .map_or_else(|| PathBuf::from("fuzz/corpus/proto_decode"), PathBuf::from);
    if let Err(err) = std::fs::create_dir_all(&out) {
        eprintln!("could not create `{}`: {err}", out.display());
        std::process::exit(1);
    }

    let mut written = 0usize;
    for bytes in client_messages().into_iter().chain(server_messages()) {
        // Named by content hash, so regenerating is idempotent: an unchanged
        // message overwrites its own file rather than adding a duplicate under
        // a new sequence number.
        let name = blake3::hash(&bytes).to_hex();
        let path = out.join(&name.as_str()[..32]);
        match std::fs::write(&path, &bytes) {
            Ok(()) => written += 1,
            Err(err) => eprintln!("could not write `{}`: {err}", path.display()),
        }
    }

    println!("wrote {written} seeds to {}", out.display());
}

/// A small widget tree: a container with two children.
///
/// Nested and index-addressed, which is what the decoder's limits are about —
/// an empty tree exercises the envelope and nothing else.
fn sample_tree() -> tiamot_core::ui::Tree {
    use tiamot_core::ui::{Align, Children, Direction, Node, Style, Tree, Widget};
    Tree {
        nodes: vec![
            Node {
                widget: Widget::Container {
                    direction: Direction::Column,
                    gap: 8,
                    padding: 8,
                    align: Align::Start,
                },
                name: "root".to_owned(),
                style: Style::default(),
                grow: 1,
                size: None,
                cross_size: None,
                children: Children { first: 1, count: 3 },
            },
            Node {
                widget: Widget::Label {
                    text: "Inventory".to_owned(),
                },
                name: "title".to_owned(),
                style: Style::default(),
                grow: 0,
                size: None,
                cross_size: None,
                children: Children { first: 0, count: 0 },
            },
            Node {
                widget: Widget::Button {
                    text: "Close".to_owned(),
                },
                name: "close".to_owned(),
                style: Style::default(),
                grow: 0,
                size: None,
                cross_size: None,
                children: Children { first: 0, count: 0 },
            },
            // Protocol v25's widget, so a tree the fuzzer mutates has one in
            // it: its mask is the only widget field with a range narrower than
            // its type.
            Node {
                widget: Widget::ShapeEditor {
                    shape: 0b0000_0001_1111,
                    material: 3,
                },
                name: "cut".to_owned(),
                style: Style::default(),
                grow: 0,
                size: None,
                cross_size: None,
                children: Children { first: 0, count: 0 },
            },
        ],
    }
}

/// One encoding of every `ClientMessage` variant.
fn client_messages() -> Vec<Vec<u8>> {
    let messages = vec![
        ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION,
            public_key: [0x11; 32],
            display_name: "Alice".to_owned(),
        },
        // A version mismatch, which is a path with its own handling.
        ClientMessage::Hello {
            protocol_version: PROTOCOL_VERSION.wrapping_add(1),
            public_key: [0xFF; 32],
            display_name: "x".repeat(32),
        },
        ClientMessage::AuthResponse {
            signature: WireSignature([0x22; 64]),
        },
        ClientMessage::ContentRequest {
            hashes: vec![[0x33; 32]],
        },
        // A long request list: the bound is what stops this being an
        // amplifier, so the fuzzer should have an example near it.
        ClientMessage::ContentRequest {
            hashes: vec![[0x44; 32]; 512],
        },
        ClientMessage::ContentRequest { hashes: Vec::new() },
        ClientMessage::JoinWorld,
        ClientMessage::PlayerInput {
            tick: 42,
            movement: [0.5, -0.25, 1.0],
            look: [0.1, 0.9],
            actions: 0b1010,
        },
        // Extremes that validation must reject rather than propagate.
        ClientMessage::PlayerInput {
            tick: u64::MAX,
            movement: [f32::MAX, f32::MIN, 0.0],
            look: [-0.0, 0.0],
            actions: u32::MAX,
        },
        ClientMessage::BlockDelta {
            edit: Edit::Block {
                pos: BlockPos::new(1, -2, 3),
                material: 7,
            },
        },
        ClientMessage::BlockDelta {
            edit: Edit::SubNode {
                pos: SubNodePos::new(-9, 100, 4),
                material: u16::MAX,
            },
        },
        ClientMessage::BlockDelta {
            edit: Edit::Block {
                pos: BlockPos::new(i32::MIN, i32::MAX, 0),
                material: 0,
            },
        },
        // Protocol v6's `Edit::Partial`, with the occupancy masks worth having
        // an example of: a legitimate partial fill, a completely full one, an
        // empty one, and one whose bits address cells a block does not have —
        // which the validator refuses, and which is the shape a mutator is most
        // likely to produce from the others.
        ClientMessage::BlockDelta {
            edit: Edit::Partial {
                pos: BlockPos::new(4, 5, 6),
                material: 3,
                occupancy: 0b0000_0001_1111_1111_1111,
            },
        },
        ClientMessage::BlockDelta {
            edit: Edit::Partial {
                pos: BlockPos::new(0, 0, 0),
                material: 1,
                occupancy: (1 << tiamot_core::UNITS_PER_BLOCK) - 1,
            },
        },
        ClientMessage::BlockDelta {
            edit: Edit::Partial {
                pos: BlockPos::new(-1, -1, -1),
                material: u16::MAX,
                occupancy: 0,
            },
        },
        ClientMessage::BlockDelta {
            edit: Edit::Partial {
                pos: BlockPos::new(1, 1, 1),
                material: 2,
                occupancy: u32::MAX,
            },
        },
        ClientMessage::Chat {
            text: "hello there".to_owned(),
        },
        ClientMessage::Chat {
            text: "\u{1F9F1} unicode and emoji".to_owned(),
        },
        ClientMessage::AddKey {
            new_public_key: [0x55; 32],
            next_key_hash: Some([0x66; 32]),
            signature: WireSignature([0x77; 64]),
            signer_public_key: [0x56; 32],
        },
        // All-zero keys: 32 zero bytes DO decode as an Ed25519 point, and it is
        // a small-order one. The decoder must not treat that as special.
        ClientMessage::AddKey {
            new_public_key: [0x00; 32],
            next_key_hash: None,
            signature: WireSignature([0x00; 64]),
            signer_public_key: [0x00; 32],
        },
        ClientMessage::RotateKey {
            new_public_key: [0x88; 32],
            new_next_key_hash: Some([0x99; 32]),
            signature: WireSignature([0xAA; 64]),
        },
        ClientMessage::RotateKey {
            new_public_key: [0xFF; 32],
            new_next_key_hash: None,
            signature: WireSignature([0xFF; 64]),
        },
        ClientMessage::Disconnect,
        // Protocol v5. These were appended without the corpus being re-seeded,
        // so until now the fuzzer had never seen a valid encoding of any of
        // them — it was mutating outwards from a protocol that stopped at
        // `Disconnect`, and reporting a clean run while doing it.
        ClientMessage::StartDig {
            target: SubNodePos::new(3, 4, 5),
        },
        ClientMessage::StartDig {
            target: SubNodePos::new(i32::MIN, 0, i32::MAX),
        },
        ClientMessage::CancelDig,
        ClientMessage::SelectTool { tool: None },
        ClientMessage::SelectTool {
            tool: Some("core_tools:chisel".to_owned()),
        },
        // At the name-length bound, which is where the check either holds or
        // does not.
        ClientMessage::SelectTool {
            tool: Some("t".repeat(32)),
        },
        // Protocol v6.
        ClientMessage::Place {
            target: SubNodePos::new(6, 7, 8),
            material: 3,
            shape: 0,
        },
        ClientMessage::Place {
            target: SubNodePos::new(i32::MAX, i32::MIN, 0),
            material: u16::MAX,
            // Protocol v24: a cut being placed, and a mask with bits past the
            // block's twenty-seven — which the server must read as loose rather
            // than as a shape nobody can hold.
            shape: u32::MAX,
        },
        ClientMessage::Place {
            target: SubNodePos::new(0, 0, 0),
            material: 3,
            shape: 0b101,
        },
        // Protocol v12. Missing until v15 — the checklist's re-seed step is the
        // one people skip, and a corpus that stops at an older variant means
        // the fuzzer never reaches the framing of the newer ones.
        ClientMessage::ViewDistance {
            horizontal: 0,
            vertical: 0,
        },
        ClientMessage::ViewDistance {
            horizontal: u8::MAX,
            vertical: u8::MAX,
        },
        // Protocol v15.
        ClientMessage::Punch { entity: 0 },
        ClientMessage::Punch { entity: u64::MAX },
        // Protocol v26. The out-of-range slot is the interesting one: the
        // server refuses it rather than clamping, and a clamp is exactly the
        // bug a fuzzer would find by asking for slot 65,535.
        ClientMessage::SwapOffhand { slot: 0 },
        ClientMessage::SwapOffhand { slot: u16::MAX },
        // **Every dialog event**, which had NO seeds at all until protocol
        // v25 — the whole family of messages a client sends back from a
        // server's own interface, every string of which the server echoed to
        // it and is now reading again.
        ClientMessage::DialogEvent {
            form: "core_ui:inventory".to_owned(),
            event: DialogEvent::Pressed {
                name: "close".to_owned(),
            },
        },
        ClientMessage::DialogEvent {
            form: "shop".to_owned(),
            event: DialogEvent::Submitted {
                name: "search".to_owned(),
                text: "x".repeat(64),
            },
        },
        ClientMessage::DialogEvent {
            form: "shop".to_owned(),
            event: DialogEvent::Toggled {
                name: "auto".to_owned(),
                checked: true,
            },
        },
        ClientMessage::DialogEvent {
            form: "shop".to_owned(),
            event: DialogEvent::Slid {
                name: "amount".to_owned(),
                value: i32::MIN,
            },
        },
        ClientMessage::DialogEvent {
            form: "shop".to_owned(),
            event: DialogEvent::Chose {
                name: "kind".to_owned(),
                index: u16::MAX,
            },
        },
        ClientMessage::DialogEvent {
            form: "core_ui:inventory".to_owned(),
            event: DialogEvent::Clicked {
                view: "player:main".to_owned(),
                index: 26,
                click: Click::ShiftLeft,
            },
        },
        ClientMessage::DialogEvent {
            form: "core_ui:inventory".to_owned(),
            event: DialogEvent::Closed,
        },
        // Protocol v25, and a mask with every bit set above the block's
        // twenty-seven — the shape the validator has to refuse.
        ClientMessage::DialogEvent {
            form: "bench:craft".to_owned(),
            event: DialogEvent::Chiselled {
                name: "cut".to_owned(),
                shape: 0b0000_0111,
            },
        },
        ClientMessage::DialogEvent {
            form: "bench:craft".to_owned(),
            event: DialogEvent::Chiselled {
                name: "cut".to_owned(),
                shape: u32::MAX,
            },
        },
    ];
    messages.iter().filter_map(|m| encode(m).ok()).collect()
}

/// One encoding of every `ServerMessage` variant.
fn server_messages() -> Vec<Vec<u8>> {
    let messages = vec![
        ServerMessage::HelloAck {
            protocol_version: PROTOCOL_VERSION,
            cert_fingerprint: [0xBB; 32],
        },
        ServerMessage::AuthChallenge { nonce: [0xCC; 32] },
        ServerMessage::ModManifest {
            mods: vec![
                ModEntry {
                    id: "core".to_owned(),
                    version: "0.1.0".to_owned(),
                    content_hash: [0xDD; 32],
                },
                ModEntry {
                    id: "core_worldgen".to_owned(),
                    version: "0.1.0".to_owned(),
                    content_hash: [0xEE; 32],
                },
            ],
            set_fingerprint: 0xDEAD_BEEF_CAFE_F00D,
        },
        ServerMessage::ModManifest {
            mods: Vec::new(),
            set_fingerprint: 0,
        },
        // Content transfer: the shapes a slice takes, including the empty and
        // boundary cases the transfer code has to get right.
        ServerMessage::ContentChunk {
            hash: [0x01; 32],
            offset: 0,
            total_len: 5,
            data: vec![1, 2, 3, 4, 5],
        },
        ServerMessage::ContentChunk {
            hash: [0x02; 32],
            offset: 0,
            total_len: 0,
            data: Vec::new(),
        },
        ServerMessage::ContentChunk {
            hash: [0x03; 32],
            offset: u64::MAX,
            total_len: u64::MAX,
            data: vec![0xFF; 1024],
        },
        ServerMessage::JoinWorld {
            player_uuid: [0x12; 32],
            spawn: BlockPos::new(0, 1, 0),
            tick: 7,
        },
        ServerMessage::ChunkData {
            pos: ChunkPos::new(1, -2, 3),
            blob: vec![0x05, 0x00, 0x01, 0x02, 0x03],
        },
        ServerMessage::ChunkData {
            pos: ChunkPos::new(0, 0, 0),
            blob: Vec::new(),
        },
        ServerMessage::ChunkUnload {
            pos: ChunkPos::new(-4, 5, -6),
        },
        ServerMessage::BlockDelta {
            edit: Edit::Block {
                pos: BlockPos::new(2, 3, 4),
                material: 9,
            },
            actor: Some([0x0F; 32]),
        },
        ServerMessage::BlockDelta {
            edit: Edit::SubNode {
                pos: SubNodePos::new(7, 8, 9),
                material: 1,
            },
            actor: None,
        },
        ServerMessage::EntityStateDelta {
            tick: 99,
            payload: vec![0xAB; 64],
        },
        ServerMessage::Chat {
            from: Some([0x10; 32]),
            text: "hello".to_owned(),
        },
        ServerMessage::Chat {
            from: None,
            text: String::new(),
        },
        // Every disconnect reason: each carries a different payload shape.
        ServerMessage::Disconnect {
            reason: DisconnectReason::VersionMismatch {
                server: PROTOCOL_VERSION,
                client: PROTOCOL_VERSION.wrapping_add(9),
            },
        },
        ServerMessage::Disconnect {
            reason: DisconnectReason::AuthFailed {
                detail: "authentication failed".to_owned(),
            },
        },
        ServerMessage::Disconnect {
            reason: DisconnectReason::NameTaken {
                name: "Alice".to_owned(),
            },
        },
        ServerMessage::Disconnect {
            reason: DisconnectReason::NotAllowlisted,
        },
        ServerMessage::Disconnect {
            reason: DisconnectReason::ServerFull { max_players: 50 },
        },
        ServerMessage::Disconnect {
            reason: DisconnectReason::ProtocolError {
                detail: "malformed".to_owned(),
            },
        },
        ServerMessage::Disconnect {
            reason: DisconnectReason::Kicked {
                reason: "being tiresome".to_owned(),
            },
        },
        ServerMessage::Disconnect {
            reason: DisconnectReason::ServerStopping,
        },
        // Protocol v3. The interesting shapes for a decoder are the empty
        // table, a name at the field limit, and the two states of the optional
        // texture hash — an `Option` inside a `Vec` is where a length claim and
        // a discriminant meet.
        ServerMessage::MaterialTable {
            materials: Vec::new(),
        },
        ServerMessage::MaterialTable {
            materials: vec![
                MaterialDef {
                    id: 0,
                    name: "engine:air".to_owned(),
                    texture: None,
                    step_sound: None,
                },
                MaterialDef {
                    id: 1,
                    name: "engine:unknown".to_owned(),
                    texture: None,
                    step_sound: None,
                },
                MaterialDef {
                    id: 2,
                    name: "core:white".to_owned(),
                    texture: Some([0x12; 32]),
                    step_sound: None,
                },
                MaterialDef {
                    id: u16::MAX,
                    name: "\u{1F9F1} unicode in a material name".to_owned(),
                    texture: Some([0x00; 32]),
                    step_sound: None,
                },
            ],
        },
        // Protocol v4 and v5, also never seeded when they were added. Both
        // carry `f32` fields that `validate_server_message` bounds, so the
        // shapes worth having are the ordinary one and the ones that are only
        // rejected because something checks them: a non-finite position, and a
        // dig progress outside 0..=1.
        ServerMessage::PlayerState {
            last_processed_input: 1234,
            chunk: ChunkPos::new(1, 2, 3),
            local: [24.0, 0.5, 47.9],
            velocity: [0.1, -0.2, 0.3],
            on_ground: true,
        },
        ServerMessage::PlayerState {
            last_processed_input: u64::MAX,
            chunk: ChunkPos::new(i32::MIN, 0, i32::MAX),
            local: [f32::NAN, f32::INFINITY, f32::NEG_INFINITY],
            velocity: [f32::MAX, f32::MIN, 0.0],
            on_ground: false,
        },
        ServerMessage::DigProgress {
            target: SubNodePos::new(9, 9, 9),
            progress: 0.5,
        },
        ServerMessage::DigProgress {
            target: SubNodePos::new(0, 0, 0),
            progress: f32::NAN,
        },
        // Protocol v7's tool table. Empty, ordinary, and with a brush string
        // the engine does not know — mods grow brush shapes, and a client that
        // could not decode an unfamiliar one would refuse the whole message.
        ServerMessage::ToolTable { tools: Vec::new() },
        ServerMessage::ToolTable {
            tools: vec![
                tiamot_core::proto::ToolDef {
                    id: "core_tools:hand".to_owned(),
                    name: "Bare Hand".to_owned(),
                    brush: "block".to_owned(),
                    default: true,
                },
                tiamot_core::proto::ToolDef {
                    id: "core_tools:chisel".to_owned(),
                    name: "Chisel".to_owned(),
                    brush: "subnode".to_owned(),
                    default: false,
                },
                tiamot_core::proto::ToolDef {
                    id: "m:big".to_owned(),
                    name: String::new(),
                    brush: "three_by_three_column".to_owned(),
                    default: false,
                },
            ],
        },
        // Protocol v6's partial edit, on the way back out.
        ServerMessage::BlockDelta {
            edit: Edit::Partial {
                pos: BlockPos::new(2, 3, 4),
                material: 5,
                occupancy: 0b0000_0111_1111_1111_1111_1111,
            },
            actor: Some([0xAB; 32]),
        },
        // Protocol v8's chunk light, in all three shapes the format has: the
        // uniform payload that most of a world is, a run-length one, and an
        // empty body — which is not valid light but IS valid framing, and the
        // fuzzer should start from a message whose envelope parses.
        ServerMessage::ChunkLight {
            pos: ChunkPos::new(0, 0, 0),
            light: tiamot_core::light::codec::encode(&tiamot_core::light::LightLayer::uniform(
                tiamot_core::light::Light::DAYLIGHT,
            )),
        },
        ServerMessage::ChunkLight {
            pos: ChunkPos::new(-3, 1, 7),
            light: {
                let mut layer = tiamot_core::light::LightLayer::dark();
                for index in 0..tiamot_core::BLOCKS_PER_CHUNK {
                    layer.set(
                        tiamot_core::coords::LocalBlock::from_index(index),
                        tiamot_core::light::Light::new((index % 16) as u8, 3, 0, 9),
                    );
                }
                tiamot_core::light::codec::encode(&layer)
            },
        },
        ServerMessage::ChunkLight {
            pos: ChunkPos::new(1, 1, 1),
            light: Vec::new(),
        },
        // **Everything from protocol v9 to v22 was unseeded until Task 14.**
        // The corpus stopped at v8, so the fuzzer had been starting from a
        // decoder two years of tasks out of date — running clean and proving
        // nothing about seventeen variants. That is CONTRIBUTING's step 4
        // skipped four more times, the same way v4 and v5 skipped it. Every
        // variant below is here so the next append has somewhere to sit.
        ServerMessage::SkyTable {
            day_length_ticks: 24_000,
            keyframes: vec![SkyFrame {
                time: 0.5,
                sky: [0.4, 0.6, 0.9],
                sun: [1.0, 0.98, 0.9],
                intensity: 1.0,
                grade: SkyGrade::NONE,
            }],
        },
        ServerMessage::TimeOfDay { time: 0.25 },
        ServerMessage::ChunkFluid {
            pos: ChunkPos::new(2, -1, 4),
            fluid: vec![0x01, 0x02, 0x03, 0x04],
        },
        ServerMessage::FluidTable {
            fluids: vec![FluidDef {
                id: 1,
                name: "core_milk:milk".to_owned(),
                material: 4,
                depths: [1, 2, 3, 4, 5, 6, 7, 8],
                color: [240, 240, 230],
            }],
        },
        ServerMessage::ViewDistance {
            horizontal: 8,
            vertical: 4,
        },
        ServerMessage::EntitySpawn {
            entities: vec![EntityDef {
                id: 9,
                chunk: ChunkPos::new(0, 0, 0),
                local: [1.0, 2.0, 3.0],
                velocity: [0.0; 3],
                yaw: 128,
                pitch: -32,
                anim: 1,
                model: Some("core_mimic:chest".to_owned()),
                collider: Some([0.6, 1.8]),
                nametag: Some("a mimic".to_owned()),
                item: None,
            }],
        },
        ServerMessage::EntityDespawn {
            entities: vec![9, 10, 11],
        },
        ServerMessage::EntityState {
            tick: 42,
            entities: vec![EntityDelta {
                id: 9,
                chunk: ChunkPos::new(0, 0, 0),
                local: [1.5, 2.0, 3.0],
                velocity: [0.1, 0.0, 0.0],
                yaw: 130,
                pitch: -30,
                anim: 2,
            }],
        },
        ServerMessage::ActionTable {
            actions: vec![ActionDef {
                id: "core_tools:chisel_mode".to_owned(),
                description: "Switch the chisel".to_owned(),
                mod_id: "core_tools".to_owned(),
                default_key: "KeyR".to_owned(),
            }],
        },
        ServerMessage::SoundTable {
            sounds: vec![SoundDef {
                id: "core_tools:break".to_owned(),
                mod_id: "core_tools".to_owned(),
                file: Some([0xEE; 32]),
                gain: 1.0,
                pitch_variance: 0.1,
            }],
        },
        ServerMessage::PlaySound {
            sound: "core_tools:break".to_owned(),
            pos: [10.0, 64.0, -3.0],
            radius: 16.0,
            gain: 1.0,
            entity: Some(9),
        },
        // A dialog with a real tree rather than an empty one: the tree is
        // nested, indexed and bounded, which is the part with something to
        // find in it.
        ServerMessage::ShowDialog {
            form: "core_ui:inventory".to_owned(),
            tree: sample_tree(),
            compact: false,
        },
        ServerMessage::UpdateDialog {
            form: "core_ui:inventory".to_owned(),
            tree: sample_tree(),
            compact: true,
        },
        ServerMessage::CloseDialog {
            form: "core_ui:inventory".to_owned(),
        },
        ServerMessage::ViewUpdate {
            view: "player:main".to_owned(),
            slots: vec![
                Some(tiamot_core::proto::StackDef {
                    material: 3,
                    units: 40,
                    shape: 0,
                }),
                None,
                // Protocol v24: a shaped stack, which is the shape a decoder
                // has never seen before this version.
                Some(tiamot_core::proto::StackDef {
                    material: 4,
                    units: 27,
                    shape: 0b1_0101,
                }),
            ],
            held: Some(tiamot_core::proto::StackDef {
                material: 3,
                units: 13,
                shape: 0,
            }),
        },
        // Protocol v23: the cue table and the loops.
        ServerMessage::SoundBindings {
            bindings: vec![
                tiamot_core::proto::SoundBinding {
                    cue: "engine:jump".to_owned(),
                    sound: "core_blocks:jump".to_owned(),
                    mod_id: "core_blocks".to_owned(),
                },
                tiamot_core::proto::SoundBinding {
                    cue: "core_doors:open".to_owned(),
                    sound: "core_doors:creak".to_owned(),
                    mod_id: "core_doors".to_owned(),
                },
            ],
        },
        ServerMessage::StartLoop {
            id: "core_sky:ambience".to_owned(),
            sound: "core_sky:night".to_owned(),
            pos: [0.0; 3],
            radius: 16.0,
            gain: 0.3,
            everywhere: true,
        },
        // And the positional shape, which takes a different branch on both ends.
        ServerMessage::StartLoop {
            id: "mill:wheel".to_owned(),
            sound: "mill:creak".to_owned(),
            pos: [12.0, 64.0, -8.0],
            radius: 24.0,
            gain: 1.0,
            everywhere: false,
        },
        ServerMessage::StopLoop {
            id: "core_sky:ambience".to_owned(),
        },
        ServerMessage::HudScripts {
            scripts: vec![
                HudScriptDef {
                    mod_id: "core_ui".to_owned(),
                    file: Some([0xAA; 32]),
                },
                // The shape a mod produces by naming a file it does not have.
                HudScriptDef {
                    mod_id: "broken".to_owned(),
                    file: None,
                },
            ],
        },
    ];
    messages.iter().filter_map(|m| encode(m).ok()).collect()
}
