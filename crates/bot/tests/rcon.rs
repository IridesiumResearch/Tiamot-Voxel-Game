// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! RCON against a real server over a real loopback socket.

use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_server::{ServerHandle, Settings};
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _, BufReader};
use tokio::net::TcpStream;

const TOKEN: &str = "a-sufficiently-long-test-token";

fn start(name: &str) -> (ServerHandle, std::net::SocketAddr) {
    let dir = std::env::temp_dir().join("tiamot-rcon-tests").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");

    // Port 0 for RCON too, so tests can run in parallel.
    let rcon_addr: std::net::SocketAddr = "127.0.0.1:0".parse().expect("loopback");
    let listener = std::net::TcpListener::bind(rcon_addr).expect("probe a free port");
    let rcon_addr = listener.local_addr().expect("addr");
    drop(listener);

    let server = ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: dir,
        max_players: 8,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        rcon: Some((rcon_addr, TOKEN.to_owned())),
        view_distance: tiamot_core::interest::ViewDistance::MINIMUM,
        mods_path: None,
        enabled_mods: None,
        seed: Some(1),
        materials: vec!["test:stone".to_owned()],
    })
    .expect("start");

    (server, rcon_addr)
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

/// A connected RCON client.
struct Admin {
    lines: BufReader<tokio::net::tcp::OwnedReadHalf>,
    write: tokio::net::tcp::OwnedWriteHalf,
}

impl Admin {
    async fn connect(addr: std::net::SocketAddr) -> Self {
        // The listener spawns on the network runtime a moment after start
        // returns, so a first connection can beat it. Retry briefly rather
        // than sleeping a fixed amount and hoping.
        let stream = {
            let mut attempt = None;
            for _ in 0..100 {
                match TcpStream::connect(addr).await {
                    Ok(stream) => {
                        attempt = Some(stream);
                        break;
                    }
                    Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
                }
            }
            attempt.expect("RCON should be listening")
        };
        let (read, write) = stream.into_split();
        let mut admin = Self {
            lines: BufReader::new(read),
            write,
        };
        // Banner.
        let _ = admin.read_reply().await;
        admin
    }

    /// Reads a whole reply: every line up to the lone `.` terminator.
    async fn read_reply(&mut self) -> String {
        let mut reply = String::new();
        loop {
            let mut line = String::new();
            let read =
                tokio::time::timeout(Duration::from_secs(5), self.lines.read_line(&mut line))
                    .await
                    .expect("RCON read timed out")
                    .expect("read");
            if read == 0 {
                // Connection closed mid-reply. Return what arrived so the
                // caller can assert on it rather than seeing a panic here.
                return reply.trim_end().to_owned();
            }
            if line.trim_end() == "." {
                return reply.trim_end().to_owned();
            }
            reply.push_str(&line);
        }
    }

    async fn send(&mut self, command: &str) -> String {
        self.write
            .write_all(format!("{command}\n").as_bytes())
            .await
            .expect("write");
        self.read_reply().await
    }

    async fn authenticate(&mut self) {
        let reply = self.send(&format!("auth {TOKEN}")).await;
        assert_eq!(reply, "ok: authenticated", "auth should succeed");
    }
}

#[test]
fn a_bad_token_is_refused_and_the_connection_closed() {
    // An unauthenticated peer that can keep guessing is an unauthenticated peer
    // brute-forcing.
    let (server, rcon_addr) = start("bad-token");

    block_on(async {
        let mut admin = Admin::connect(rcon_addr).await;
        let reply = admin.send("auth definitely-not-the-token").await;
        assert_eq!(reply, "error: bad token");

        // The server must have closed; a further read returns nothing.
        let mut line = String::new();
        let read = admin.lines.read_line(&mut line).await.expect("read");
        assert_eq!(read, 0, "a bad token must close the connection");
    });

    server.stop();
}

#[test]
fn commands_before_authentication_are_refused() {
    let (server, rcon_addr) = start("unauthenticated");

    block_on(async {
        let mut admin = Admin::connect(rcon_addr).await;
        let reply = admin.send("status").await;
        assert!(
            reply.starts_with("error:"),
            "an unauthenticated command must be refused, got `{reply}`"
        );
    });

    server.stop();
}

#[test]
fn status_reports_the_tick_and_the_players() {
    let (server, rcon_addr) = start("status");
    let addr = server.local_addr();
    let fingerprint = server.cert_fingerprint();

    block_on(async {
        let mut admin = Admin::connect(rcon_addr).await;
        admin.authenticate().await;

        let empty = admin.send("status").await;
        assert!(empty.contains("players 0/8"), "got `{empty}`");

        let mut alice = Bot::connect(addr, Identity::generate().expect("identity"), fingerprint)
            .await
            .expect("connect");
        alice.join("Alice").await.expect("join");

        // The roster is populated by the connection task, so give it a moment.
        let mut with_player = String::new();
        for _ in 0..100 {
            with_player = admin.send("status").await;
            if with_player.contains("players 1/8") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            with_player.contains("players 1/8"),
            "status should count the joined player, got `{with_player}`"
        );

        alice.disconnect().await;
    });

    server.stop();
}

#[test]
fn kick_disconnects_the_named_player_with_a_reason() {
    let (server, rcon_addr) = start("kick");
    let addr = server.local_addr();
    let fingerprint = server.cert_fingerprint();

    block_on(async {
        let mut admin = Admin::connect(rcon_addr).await;
        admin.authenticate().await;

        let mut alice = Bot::connect(addr, Identity::generate().expect("identity"), fingerprint)
            .await
            .expect("connect");
        alice.join("Alice").await.expect("join");

        // Wait for the name binding to be visible to RCON.
        let mut reply = String::new();
        for _ in 0..100 {
            reply = admin.send("kick Alice being tiresome").await;
            if !reply.contains("no player named") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(reply, "ok: kicked Alice", "got `{reply}`");

        // Alice must receive a reason, not a silent drop.
        //
        // Read until the disconnect rather than taking the next message: since
        // Task 09 the server sends a `PlayerState` every tick, so "the next
        // thing Alice hears" is almost always that.
        let message = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let message = alice.recv().await.expect("a message, not an error");
                if matches!(
                    message,
                    tiamot_core::proto::ServerMessage::Disconnect { .. }
                ) {
                    return message;
                }
            }
        })
        .await
        .expect("kick should arrive promptly");
        assert!(
            matches!(
                message,
                tiamot_core::proto::ServerMessage::Disconnect {
                    reason: tiamot_core::proto::DisconnectReason::Kicked { ref reason }
                } if reason == "being tiresome"
            ),
            "expected a Kicked disconnect carrying the reason, got {message:?}"
        );
    });

    server.stop();
}

#[test]
fn kicking_someone_who_is_not_there_says_so() {
    let (server, rcon_addr) = start("kick-absent");

    block_on(async {
        let mut admin = Admin::connect(rcon_addr).await;
        admin.authenticate().await;
        let reply = admin.send("kick Nobody").await;
        assert!(reply.contains("no player named"), "got `{reply}`");
    });

    server.stop();
}

#[test]
fn rename_moves_a_name_and_frees_the_old_one() {
    let (server, rcon_addr) = start("rename");
    let addr = server.local_addr();
    let fingerprint = server.cert_fingerprint();

    block_on(async {
        let mut admin = Admin::connect(rcon_addr).await;
        admin.authenticate().await;

        let mut alice = Bot::connect(addr, Identity::generate().expect("identity"), fingerprint)
            .await
            .expect("connect");
        alice.join("Alice").await.expect("join");

        let mut reply = String::new();
        for _ in 0..100 {
            reply = admin.send("rename Alice Alicia").await;
            if !reply.contains("no player named") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(reply, "ok: Alice is now Alicia", "got `{reply}`");

        // The freed name must be claimable by someone else.
        let mut bob = Bot::connect(addr, Identity::generate().expect("identity"), fingerprint)
            .await
            .expect("connect");
        bob.join("Alice")
            .await
            .expect("the released name should be free");

        alice.disconnect().await;
        bob.disconnect().await;
    });

    server.stop();
}

#[test]
fn allowlist_reports_open_and_enforced_differently() {
    // "Open" and "enforced but empty" are opposites — everyone may join versus
    // nobody may. Reporting them the same way would be actively misleading.
    let (server, rcon_addr) = start("allowlist");

    block_on(async {
        let mut admin = Admin::connect(rcon_addr).await;
        admin.authenticate().await;

        let open = admin.send("allowlist list").await;
        assert!(open.contains("open"), "got `{open}`");

        assert_eq!(admin.send("allowlist on").await, "ok: allowlist on");
        let enforced = admin.send("allowlist").await;
        assert!(
            enforced.contains("enforced") && enforced.contains("nobody may join"),
            "an enforced empty allowlist must not read like an open one, got `{enforced}`"
        );

        let uuid = "ab".repeat(32);
        let added = admin.send(&format!("allowlist add {uuid}")).await;
        assert!(added.starts_with("ok:"), "got `{added}`");
        let listed = admin.send("allowlist list").await;
        assert!(listed.contains(&uuid), "the entry should be listed");

        let removed = admin.send(&format!("allowlist remove {uuid}")).await;
        assert!(removed.starts_with("ok:"), "got `{removed}`");
        assert!(!admin.send("allowlist list").await.contains(&uuid));
    });

    server.stop();
}

#[test]
fn an_enforced_allowlist_takes_effect_without_a_restart() {
    // The reason the allowlist is behind a lock rather than fixed at startup:
    // an operator should not have to disconnect everyone to change it.
    let (server, rcon_addr) = start("allowlist-live");
    let addr = server.local_addr();
    let fingerprint = server.cert_fingerprint();

    block_on(async {
        let mut admin = Admin::connect(rcon_addr).await;
        admin.authenticate().await;
        assert_eq!(admin.send("allowlist on").await, "ok: allowlist on");

        let mut stranger = Bot::connect(addr, Identity::generate().expect("identity"), fingerprint)
            .await
            .expect("connect");
        let err = stranger
            .join("Stranger")
            .await
            .expect_err("an empty enforced allowlist must refuse everyone");
        assert!(
            matches!(
                err,
                bot::BotError::Refused {
                    reason: tiamot_core::proto::DisconnectReason::NotAllowlisted
                }
            ),
            "got {err}"
        );

        // Permit them, still without restarting.
        let permitted = Identity::generate().expect("identity");
        let reply = admin
            .send(&format!("allowlist add {}", permitted.uuid_as_root()))
            .await;
        assert!(reply.starts_with("ok:"), "got `{reply}`");

        let mut allowed = Bot::connect(addr, permitted, fingerprint)
            .await
            .expect("connect");
        allowed
            .join("Permitted")
            .await
            .expect("an allowlisted identity should get in without a restart");
        allowed.disconnect().await;
    });

    server.stop();
}

#[test]
fn rebind_replaces_a_root_key_and_refuses_nonsense() {
    let (server, rcon_addr) = start("rebind");
    let addr = server.local_addr();
    let fingerprint = server.cert_fingerprint();

    block_on(async {
        let mut admin = Admin::connect(rcon_addr).await;
        admin.authenticate().await;

        let alice = Identity::generate().expect("identity");
        let uuid = alice.uuid_as_root();
        let mut bot = Bot::connect(addr, alice, fingerprint)
            .await
            .expect("connect");
        bot.join("Alice").await.expect("join");
        bot.disconnect().await;

        // Nonsense arguments are refused rather than accepted and ignored.
        assert!(admin.send("rebind").await.starts_with("error:"));
        assert!(
            admin
                .send("rebind not-a-uuid aa")
                .await
                .starts_with("error:")
        );
        assert!(
            admin
                .send(&format!("rebind {uuid} not-a-key"))
                .await
                .starts_with("error:")
        );
        // 32 zero bytes decode as a public key but are a small-order point;
        // accepting one would authorise a key nobody holds a secret for.
        assert!(
            admin
                .send(&format!("rebind {uuid} {}", "0".repeat(64)))
                .await
                .starts_with("error:"),
            "a weak key must be refused"
        );

        let replacement = Identity::generate().expect("identity");
        let key_hex: String = replacement
            .public_key()
            .as_bytes()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();

        let mut reply = String::new();
        for _ in 0..100 {
            reply = admin.send(&format!("rebind {uuid} {key_hex}")).await;
            if !reply.contains("unknown identity") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(reply.starts_with("ok:"), "got `{reply}`");

        // The new root key can now join as that identity.
        let mut heir = Bot::connect(addr, replacement, fingerprint)
            .await
            .expect("connect");
        heir.join("Alice")
            .await
            .expect("the new root key should claim the existing identity");
        assert_eq!(
            heir.received()
                .iter()
                .filter(|m| matches!(m, tiamot_core::proto::ServerMessage::JoinWorld { .. }))
                .count(),
            1
        );
        heir.disconnect().await;
    });

    server.stop();
}

#[test]
fn stop_shuts_the_server_down() {
    let (server, rcon_addr) = start("stop");

    block_on(async {
        let mut admin = Admin::connect(rcon_addr).await;
        admin.authenticate().await;
        assert_eq!(admin.send("stop").await, "ok: stopping");
    });

    // `stop` on the handle must still join cleanly after an RCON stop.
    assert!(server.stop(), "an RCON stop must leave a clean shutdown");
}

#[test]
fn save_and_mods_and_help_answer() {
    let (server, rcon_addr) = start("misc");

    block_on(async {
        let mut admin = Admin::connect(rcon_addr).await;
        admin.authenticate().await;

        assert_eq!(admin.send("save").await, "ok: save requested");
        assert_eq!(admin.send("mods").await, "no mods loaded");
        assert!(admin.send("help").await.contains("status"));
        assert!(
            admin.send("nonsense").await.starts_with("error:"),
            "an unknown command should say so rather than being ignored"
        );
    });

    server.stop();
}
