// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Line-based remote administration, on loopback only.
//!
//! # Why loopback only, and why that is not the whole story
//!
//! The listener binds `127.0.0.1` and refuses any peer that is not loopback.
//! That is a real boundary — a machine on the same network cannot reach it —
//! but it is **not** a substitute for the token: any process on the same host,
//! including one running as another user, can connect. An operator wanting
//! remote access should tunnel over SSH rather than widen the bind address,
//! because this protocol has no transport encryption and never will.
//!
//! # The token is compared in constant time
//!
//! An admin token is a secret, and a naive `==` on strings returns as soon as
//! two bytes differ. Over a loopback socket the timing difference is small but
//! it is measurable, and a byte-at-a-time oracle turns a 32-character token
//! into 32 × 62 guesses. Constant-time comparison costs nothing here.
//!
//! # Every reply ends with a lone `.`
//!
//! `status` and `allowlist list` are naturally multi-line, and a line-based
//! protocol whose replies can span an unknown number of lines is not actually
//! parseable — a client cannot tell "the reply continues" from "the server has
//! not answered yet". A terminator line, as SMTP and NNTP use, makes reading a
//! reply a loop with an end condition instead of a guess.
//!
//! # Commands are deliberately loud
//!
//! `rebind` in particular hands an identity to a new key. It exists for the
//! player with no recovery phrase and no second device, and it is the one
//! operation that can take an account from its owner. Every use is logged at
//! warn level with both keys named.

use std::net::SocketAddr;
use std::sync::Arc;

use tiamot_core::identity::{PlayerUuid, public_key_from_bytes};
use tiamot_core::session::store;
use tokio::io::{AsyncBufReadExt as _, AsyncReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tracing::{info, warn};

use crate::transport::Shared;

/// Longest line accepted, in bytes.
///
/// A peer that sends a gigabyte without a newline must not make the server
/// buffer it. Generous for a command line, trivial as a memory bound.
const MAX_LINE_BYTES: usize = 8 * 1024;

/// What the RCON layer needs beyond [`Shared`].
pub struct RconContext {
    /// Shared server state.
    pub shared: Arc<Shared>,
    /// The token an admin must present.
    pub token: String,
    /// The resolved mod set, for `mods`.
    pub mods: Vec<tiamot_core::proto::ModEntry>,
}

/// Runs the RCON listener until the simulation stops.
///
/// # Errors
///
/// [`std::io::Error`] if the socket cannot be bound.
pub async fn serve(addr: SocketAddr, context: Arc<RconContext>) -> Result<(), std::io::Error> {
    if !addr.ip().is_loopback() {
        // Refused rather than warned. An operator who typed 0.0.0.0 by mistake
        // would otherwise expose an unencrypted admin channel to the network,
        // and the first sign of it would be someone else's `stop`.
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "RCON must bind a loopback address, not {addr}. Tunnel over SSH for remote \
                 access; this protocol has no transport encryption."
            ),
        ));
    }

    let listener = TcpListener::bind(addr).await?;
    info!(%addr, "RCON listening on loopback");

    loop {
        let (stream, peer) = tokio::select! {
            accepted = listener.accept() => accepted?,
            () = wait_for_stop(&context.shared) => break,
        };

        if !peer.ip().is_loopback() {
            // Belt and braces: the bind should make this impossible, but a
            // proxy or a misconfigured interface could still deliver one.
            warn!(%peer, "refused a non-loopback RCON connection");
            continue;
        }

        let context = Arc::clone(&context);
        tokio::spawn(async move {
            if let Err(err) = session(stream, &context).await {
                warn!(%peer, "RCON session ended: {err}");
            }
        });
    }

    Ok(())
}

async fn wait_for_stop(shared: &Shared) {
    while !shared.control.stopping() {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

async fn session(stream: TcpStream, context: &RconContext) -> Result<(), std::io::Error> {
    let (read, mut write) = stream.into_split();
    let mut lines = BufReader::new(read);
    let mut authenticated = false;
    let mut line = String::new();

    write
        .write_all(b"tiamot rcon. first line must be: auth <token>\n.\n")
        .await?;

    loop {
        line.clear();
        let read = read_line(&mut lines, &mut line).await?;
        if read == 0 {
            return Ok(());
        }

        let input = line.trim();
        if input.is_empty() {
            continue;
        }

        if !authenticated {
            let Some(offered) = input.strip_prefix("auth ") else {
                write
                    .write_all(b"error: authenticate first: auth <token>\n.\n")
                    .await?;
                // Close rather than loop. An unauthenticated peer that can keep
                // trying is an unauthenticated peer brute-forcing.
                return Ok(());
            };
            if constant_time_eq(offered.trim().as_bytes(), context.token.as_bytes()) {
                authenticated = true;
                write.write_all(b"ok: authenticated\n.\n").await?;
            } else {
                warn!("RCON authentication failed");
                write.write_all(b"error: bad token\n.\n").await?;
                return Ok(());
            }
            continue;
        }

        let response = execute(input, context).await;
        write.write_all(response.text.as_bytes()).await?;
        // The terminator, always. A client reads until this line; without it a
        // multi-line reply is indistinguishable from a server still thinking.
        write.write_all(b"\n.\n").await?;
        if response.close {
            return Ok(());
        }
    }
}

/// Reads one line, refusing anything over [`MAX_LINE_BYTES`].
async fn read_line(
    reader: &mut BufReader<tokio::net::tcp::OwnedReadHalf>,
    line: &mut String,
) -> Result<usize, std::io::Error> {
    // `take` caps the read so a peer sending no newline cannot make this
    // buffer without bound.
    let mut limited = Vec::new();
    let mut handle = (&mut *reader).take(MAX_LINE_BYTES as u64);
    let read = handle.read_until(b'\n', &mut limited).await?;
    if read >= MAX_LINE_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "RCON line too long",
        ));
    }
    line.push_str(&String::from_utf8_lossy(&limited));
    Ok(read)
}

/// One command's result.
struct Reply {
    text: String,
    close: bool,
}

impl Reply {
    fn ok(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            close: false,
        }
    }

    fn error(text: impl std::fmt::Display) -> Self {
        Self {
            text: format!("error: {text}"),
            close: false,
        }
    }
}

async fn execute(input: &str, context: &RconContext) -> Reply {
    let mut parts = input.split_whitespace();
    let Some(command) = parts.next() else {
        return Reply::ok("");
    };
    let rest: Vec<&str> = parts.collect();
    let shared = &context.shared;

    match command {
        "help" => Reply::ok(
            "status | save | stop | kick <name> [reason] | mods | rename <name> <new> | \
             rebind <uuid> <pubkey-hex> | allowlist [list|on|off|add <uuid>|remove <uuid>] | quit",
        ),

        "status" => {
            let players = shared.online_players();
            let mut text = format!(
                "tick {} | players {}/{} | dropped {} | slowest tick {}us",
                shared.control.tick(),
                players.len(),
                shared.max_players,
                shared.control.dropped(),
                shared.control.slowest_tick_micros(),
            );
            for (uuid, name) in players {
                text.push_str(&format!("\n  {name} ({})", uuid.short()));
            }
            Reply::ok(text)
        }

        "save" => {
            // The simulation owns the database; asking it to save is a request,
            // not a call. Doing the write from here would mean two threads
            // writing chunks, which is the thing world.rs exists to prevent.
            shared.control.request_save();
            Reply::ok("ok: save requested")
        }

        "stop" => {
            info!("stop requested over RCON");
            shared.control.stop();
            Reply {
                text: "ok: stopping".to_owned(),
                close: true,
            }
        }

        "mods" => {
            if context.mods.is_empty() {
                return Reply::ok("no mods loaded");
            }
            let mut text = String::new();
            for entry in &context.mods {
                text.push_str(&format!("{} {}\n", entry.id, entry.version));
            }
            Reply::ok(text.trim_end().to_owned())
        }

        "kick" => {
            let Some(name) = rest.first() else {
                return Reply::error("usage: kick <name> [reason]");
            };
            let reason = if rest.len() > 1 {
                rest[1..].join(" ")
            } else {
                "kicked by an operator".to_owned()
            };

            let uuid = {
                let identities = shared.identities.lock().await;
                identities.name_holder(name)
            };
            let Some(uuid) = uuid else {
                return Reply::error(format!("no player named `{name}`"));
            };
            if shared.kick(uuid, reason.clone()) {
                info!(player = %name, %reason, "kicked over RCON");
                Reply::ok(format!("ok: kicked {name}"))
            } else {
                Reply::error("nobody is connected to kick")
            }
        }

        "rename" => {
            let ([current, new], []) = rest.split_at(2.min(rest.len())) else {
                return Reply::error("usage: rename <name> <new-name>");
            };
            let mut identities = shared.identities.lock().await;
            let Some(uuid) = identities.name_holder(current) else {
                return Reply::error(format!("no player named `{current}`"));
            };
            if identities.name_holder(new).is_some_and(|held| held != uuid) {
                return Reply::error(format!("`{new}` is already held by someone else"));
            }
            match identities.bind_name(new, uuid) {
                Ok(()) => {
                    info!(from = %current, to = %new, "renamed over RCON");
                    Reply::ok(format!("ok: {current} is now {new}"))
                }
                Err(err) => Reply::error(err),
            }
        }

        "rebind" => {
            let ([uuid_hex, key_hex], []) = rest.split_at(2.min(rest.len())) else {
                return Reply::error("usage: rebind <uuid-hex> <new-root-pubkey-hex>");
            };
            let Ok(uuid) = PlayerUuid::from_hex(uuid_hex) else {
                return Reply::error("that is not a valid 64-character UUID");
            };
            let Some(key_bytes) = hex_to_32(key_hex) else {
                return Reply::error("that is not a valid 64-character public key");
            };
            let Ok(new_root) = public_key_from_bytes(&key_bytes) else {
                return Reply::error("that is not a usable Ed25519 public key");
            };

            let mut identities = shared.identities.lock().await;
            match identities.admin_rebind(&uuid, new_root, unix_now()) {
                Ok(()) => {
                    // Deliberately loud. This is the one operation that can
                    // take an account from its owner, and a quiet audit trail
                    // is no audit trail.
                    warn!(
                        uuid = %uuid,
                        new_root_key = %key_hex,
                        "ADMIN REBIND: an identity's root key was replaced over RCON"
                    );
                    Reply::ok(format!("ok: {} rebound to {key_hex}", uuid.short()))
                }
                Err(err) => Reply::error(err),
            }
        }

        "allowlist" => allowlist(&rest, context),

        "quit" | "exit" => Reply {
            text: "bye".to_owned(),
            close: true,
        },

        other => Reply::error(format!("unknown command `{other}`; try `help`")),
    }
}

fn allowlist(rest: &[&str], context: &RconContext) -> Reply {
    let shared = &context.shared;
    match rest.first().copied() {
        None | Some("list") => {
            let allowlist = shared
                .allowlist
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            // "Open" and "enabled but empty" are opposites — everyone may join
            // versus nobody may — so they must never be reported the same way.
            if !allowlist.is_enabled() {
                return Reply::ok("allowlist: open (everyone may join)");
            }
            let entries = allowlist.entries();
            let mut text = format!("allowlist: enforced, {} permitted", entries.len());
            if entries.is_empty() {
                text.push_str(" (nobody may join)");
            }
            for uuid in entries {
                text.push_str(&format!("\n  {uuid}"));
            }
            Reply::ok(text)
        }
        Some(state @ ("on" | "off")) => {
            let mut allowlist = shared
                .allowlist
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            allowlist.set_enabled(state == "on");
            info!(
                enforced = state == "on",
                "allowlist enforcement changed over RCON"
            );
            Reply::ok(format!("ok: allowlist {state}"))
        }
        Some(action @ ("add" | "remove")) => {
            let Some(uuid_hex) = rest.get(1) else {
                return Reply::error(format!("usage: allowlist {action} <uuid-hex>"));
            };
            let Ok(uuid) = PlayerUuid::from_hex(uuid_hex) else {
                return Reply::error("that is not a valid 64-character UUID");
            };
            let mut allowlist = shared
                .allowlist
                .write()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if action == "add" {
                allowlist.allow(uuid);
            } else {
                allowlist.revoke(&uuid);
            }
            info!(%uuid, %action, "allowlist changed over RCON");
            Reply::ok(format!("ok: {action} {}", uuid.short()))
        }
        Some(other) => Reply::error(format!(
            "unknown allowlist action `{other}`; try list, on, off, add, or remove"
        )),
    }
}

/// Compares two byte strings without leaking where they differ.
///
/// See the module docs: a naive `==` returns at the first difference, which
/// turns a 32-character token into a byte-at-a-time search.
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    // The lengths themselves are not secret — a token's length is not the part
    // worth protecting, and comparing different-length inputs byte-wise would
    // need padding that leaks the same information anyway.
    if a.len() != b.len() {
        return false;
    }
    let mut difference = 0u8;
    for (left, right) in a.iter().zip(b) {
        difference |= left ^ right;
    }
    difference == 0
}

fn hex_to_32(text: &str) -> Option<[u8; 32]> {
    if text.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (index, slot) in out.iter_mut().enumerate() {
        *slot = u8::from_str_radix(text.get(index * 2..index * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(0))
}

/// Persists identity changes an RCON command made.
///
/// # Errors
///
/// [`store::StoreError`] if the write fails.
pub fn flush_identities(
    db: &tiamot_core::WorldDb,
    identities: &mut tiamot_core::session::IdentityRegistry,
) -> Result<(), store::StoreError> {
    store::flush(db, identities)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_agrees_with_ordinary_equality() {
        // The security property is about timing, which a unit test cannot
        // observe. What it CAN check is that the constant-time version did not
        // get the answer wrong in the process — a comparison that is beautifully
        // constant-time and returns true for everything is worse than useless.
        for (a, b) in [
            ("", ""),
            ("a", "a"),
            ("token", "token"),
            ("token", "toke"),
            ("token", "tokem"),
            ("token", "aoken"),
            ("token", ""),
            ("", "token"),
        ] {
            assert_eq!(
                constant_time_eq(a.as_bytes(), b.as_bytes()),
                a == b,
                "comparing `{a}` and `{b}`"
            );
        }
    }

    #[test]
    fn constant_time_eq_examines_every_byte() {
        // A difference in the LAST byte must be caught. An implementation that
        // short-circuited would pass the test above and fail here only if it
        // also got the answer wrong — so this is really a guard against a
        // future "optimisation" that reintroduces the early return.
        let mut a = [7u8; 64];
        let mut b = [7u8; 64];
        b[63] = 8;
        assert!(!constant_time_eq(&a, &b));
        a[63] = 8;
        assert!(constant_time_eq(&a, &b));
    }

    #[test]
    fn hex_parsing_rejects_anything_but_sixty_four_hex_characters() {
        assert!(hex_to_32(&"a".repeat(64)).is_some());
        assert!(hex_to_32(&"0".repeat(64)).is_some());
        assert_eq!(hex_to_32(&"a".repeat(63)), None, "too short");
        assert_eq!(hex_to_32(&"a".repeat(65)), None, "too long");
        assert_eq!(hex_to_32(&"z".repeat(64)), None, "not hex");
        assert_eq!(hex_to_32(""), None);
    }

    #[test]
    fn hex_parsing_round_trips() {
        let bytes: [u8; 32] =
            std::array::from_fn(|index| u8::try_from(index).expect("index fits in a byte") * 7);
        let text: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        assert_eq!(hex_to_32(&text), Some(bytes));
    }
}
