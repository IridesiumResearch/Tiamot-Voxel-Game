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
    ClientMessage, DisconnectReason, Edit, ModEntry, PROTOCOL_VERSION, ServerMessage,
    WireSignature, encode,
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
    ];
    messages.iter().filter_map(|m| encode(m).ok()).collect()
}
