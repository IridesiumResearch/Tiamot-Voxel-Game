// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The connector, driven the way a harness drives it: JSON lines on a pipe.
//!
//! **Run as a real subprocess**, not by calling into its functions. The whole
//! thing is a binary that reads stdin and writes stdout, and a test that
//! bypassed the pipe would not be testing the part that can break — nor would
//! it prove the refusal rule holds where it has to, which is in the running
//! program and not in a unit test of a predicate.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

use bot::Bot;
use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_core::proto::ServerMessage;
use tiamot_server::{ServerHandle, Settings};

const PATIENCE: Duration = Duration::from_secs(15);

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tiamot-watcher-{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

fn start(name: &str) -> ServerHandle {
    ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: scratch(&format!("{name}-world")),
        max_players: 4,
        allowlist: Allowlist::open(),
        operators: Vec::new(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: None,
        enabled_mods: None,
        seed: Some(4),
        rcon: None,
        materials: Vec::new(),
    })
    .expect("start")
}

fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

async fn join(server: &ServerHandle, name: &str) -> Bot {
    let mut bot = Bot::connect(
        server.local_addr(),
        Identity::generate().expect("identity"),
        server.cert_fingerprint(),
    )
    .await
    .expect("connect");
    bot.join(name).await.expect("join");
    bot
}

fn hex(bytes: [u8; 32]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Runs the connector with `instructions` on its stdin and returns its output.
fn drive(server: &ServerHandle, name: &str, acting: bool, instructions: &str) -> Vec<String> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_watcher"));
    command
        .arg("--server")
        .arg(server.local_addr().to_string())
        .arg("--name")
        .arg(name)
        .arg("--fingerprint")
        .arg(hex(server.cert_fingerprint()))
        .arg("--identity")
        .arg(scratch(name).join("identity.key"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    if acting {
        command.arg("--allow-acting");
    }
    let mut child = command.spawn().expect("the connector must start");

    let mut stdin = child.stdin.take().expect("piped");
    stdin
        .write_all(instructions.as_bytes())
        .expect("write instructions");
    // Closed so the connector stops when it runs out: a harness that goes away
    // is a connector with nobody to watch for.
    drop(stdin);

    let stdout = child.stdout.take().expect("piped");
    let lines: Vec<String> = std::io::BufReader::new(stdout)
        .lines()
        .map_while(Result::ok)
        .collect();
    let _ = child.wait();
    lines
}

/// Whether any line carries this event.
fn saw(lines: &[String], event: &str) -> bool {
    lines
        .iter()
        .any(|line| line.contains(&format!("\"event\":\"{event}\"")))
}

#[test]
fn a_connector_joins_and_says_what_it_sees() {
    let server = start("watching");
    let lines = drive(&server, "watcher-a", false, "{\"do\":\"quit\"}\n");

    assert!(
        saw(&lines, "joined"),
        "the connector never reported joining: {lines:?}"
    );
    assert!(
        lines.iter().any(|line| line.contains("\"acting\":false")),
        "a connector without `--allow-acting` must say so on the way in: {lines:?}"
    );
    server.stop();
}

#[test]
fn a_watching_connector_refuses_to_act_and_says_why() {
    // **The rule the design rests on, checked in the running program.** A
    // connector that silently dropped instructions would have a harness sending
    // them forever and a person concluding it was broken.
    let server = start("refusing");
    block_on(async {
        let mut listener = join(&server, "Listener").await;

        let lines = drive(
            &server,
            "watcher-b",
            false,
            "{\"do\":\"say\",\"text\":\"knock knock\"}\n{\"do\":\"quit\"}\n",
        );
        assert!(
            saw(&lines, "refused"),
            "a watching connector must refuse and explain: {lines:?}"
        );

        // And nothing reached the world. Read for a moment rather than
        // asserting on an empty queue immediately — the point is that it never
        // arrives, and a race would make this pass for the wrong reason.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let mut heard = false;
        while tokio::time::Instant::now() < deadline {
            if listener.received().iter().any(
                |message| matches!(message, ServerMessage::Chat { text, .. } if text.contains("knock")),
            ) {
                heard = true;
                break;
            }
            if listener.recv().await.is_err() {
                break;
            }
        }
        assert!(!heard, "a refused instruction reached the world anyway");
        listener.disconnect().await;
    });
    server.stop();
}

#[test]
fn an_allowed_connector_speaks_and_the_world_hears_it() {
    let server = start("acting");
    block_on(async {
        let mut listener = join(&server, "Listener").await;

        // Spawned on its own thread so the listener keeps reading while the
        // connector talks — the chat line arrives while the subprocess is
        // still running, and a test that drained afterwards would depend on
        // the server having buffered it.
        let address = server.local_addr();
        let fingerprint = hex(server.cert_fingerprint());
        let handle = std::thread::spawn(move || {
            let mut child = Command::new(env!("CARGO_BIN_EXE_watcher"))
                .arg("--server")
                .arg(address.to_string())
                .arg("--name")
                .arg("watcher-c")
                .arg("--fingerprint")
                .arg(fingerprint)
                .arg("--identity")
                .arg(scratch("watcher-c").join("identity.key"))
                .arg("--allow-acting")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .expect("the connector must start");
            let mut stdin = child.stdin.take().expect("piped");
            stdin
                .write_all(b"{\"do\":\"say\",\"text\":\"the watcher is here\"}\n")
                .expect("write");
            // Left open for a moment so the connector is still connected when
            // the line lands, then closed to stop it.
            std::thread::sleep(Duration::from_secs(3));
            drop(stdin);
            let _ = child.wait();
        });

        let deadline = tokio::time::Instant::now() + PATIENCE;
        let mut heard = false;
        while tokio::time::Instant::now() < deadline && !heard {
            heard = listener.received().iter().any(
                |message| matches!(message, ServerMessage::Chat { text, .. } if text.contains("the watcher is here")),
            );
            if !heard && listener.recv().await.is_err() {
                break;
            }
        }
        assert!(
            heard,
            "the connector spoke and nobody in the world heard it"
        );

        handle.join().expect("the connector thread");
        listener.disconnect().await;
    });
    server.stop();
}

#[test]
fn acting_against_a_server_that_is_not_local_is_refused_before_connecting() {
    // The check is on what is TRUE, not on what the flag says — and it happens
    // before any connection, so a connector aimed at somebody else's world
    // never gets as far as joining it.
    let output = Command::new(env!("CARGO_BIN_EXE_watcher"))
        .arg("--server")
        .arg("203.0.113.9:4433")
        .arg("--fingerprint")
        .arg("00".repeat(32))
        .arg("--allow-acting")
        .stdin(Stdio::null())
        .output()
        .expect("the connector must start");

    assert!(!output.status.success(), "it should have refused");
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.contains("not on this machine"),
        "the refusal must say why: {text}"
    );
}
