// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The harness testing itself.
//!
//! Task 07's stated tests: a failing bot assertion fails the run, a server
//! crash is detected rather than hanging, temp worlds are cleaned up, and the
//! starter scripts pass against a live server.
//!
//! # These drive the real binary
//!
//! Every test here shells out to the `bot` executable rather than calling the
//! library, because the thing under test is the *tool*: its argument parsing,
//! its output, and above all its exit code. A harness that reported failures
//! and exited 0 would be worse than no harness, and only a subprocess test can
//! catch that.

use std::path::{Path, PathBuf};
use std::process::Command;

use tiamot_core::identity::{Allowlist, Identity};
use tiamot_core::interest::ViewDistance;
use tiamot_server::{ServerHandle, Settings};

const MATERIALS: [&str; 2] = ["test:stone", "test:dirt"];

/// Runs one future to completion. Most of this file drives the bot BINARY, so
/// there is no runtime lying about; the one case that talks to the server
/// directly builds its own.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

/// Path to the `bot` binary built alongside this test.
///
/// Derived from the test executable's own location rather than assumed, so it
/// works under `cargo test`, `cargo test --release`, and a custom target dir.
fn bot_binary() -> PathBuf {
    let mut path = std::env::current_exe().expect("test executable path");
    path.pop(); // the deps/ directory
    if path.ends_with("deps") {
        path.pop();
    }
    path.join(format!("bot{}", std::env::consts::EXE_SUFFIX))
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join("tiamot-harness").join(name);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch dir");
    dir
}

/// The reference mods, which register the tools and generate the terrain.
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
        max_players: 32,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        // The reference mods, because scenarios now DIG rather than writing
        // blocks into the world. Charter rule 1 means the engine has no tools,
        // so a modless server is one where every script would fail at the first
        // dig — and `core_worldgen` is what puts terrain under them to dig.
        mods_path: Some(reference_mods()),
        enabled_mods: None,
        seed: Some(9),
        rcon: None,
        materials: MATERIALS.iter().map(|name| (*name).to_owned()).collect(),
    })
    .expect("start")
}

/// Runs `bot run <script>` against a server and returns (exit code, output).
fn run_script(server: &ServerHandle, script: &Path) -> (i32, String) {
    let output = Command::new(bot_binary())
        .arg("run")
        .arg(script)
        .arg("--server")
        .arg(server.local_addr().to_string())
        .output()
        .expect("the bot binary should run");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    (output.status.code().unwrap_or(-1), combined)
}

fn repo_script(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("scripts")
        .join(name)
}

#[test]
fn a_failing_assertion_fails_the_run() {
    // The single most important property of the harness. A green exit on a
    // failed assertion makes every scenario script worthless.
    let server = start("failing-assertion");
    let dir = scratch("failing-assertion-script");
    let script = dir.join("fails.lua");
    std::fs::write(
        &script,
        "bot.join('failer')\nbot.assert(false, 'this scenario is supposed to fail')",
    )
    .expect("write");

    let (code, output) = run_script(&server, &script);

    assert_ne!(code, 0, "a failed assertion must not exit 0:\n{output}");
    assert!(
        output.contains("this scenario is supposed to fail"),
        "the message must reach the operator:\n{output}"
    );
    assert!(output.contains("FAIL"), "and be recognisable:\n{output}");

    server.stop();
}

#[test]
fn a_passing_script_exits_zero_and_reports_its_assertions() {
    let server = start("passing");
    let dir = scratch("passing-script");
    let script = dir.join("passes.lua");
    std::fs::write(
        &script,
        "bot.join('passer')\nbot.assert(true)\nbot.assert(1 + 1 == 2)\nbot.disconnect()",
    )
    .expect("write");

    let (code, output) = run_script(&server, &script);

    assert_eq!(code, 0, "a passing script must exit 0:\n{output}");
    assert!(output.contains("PASS"), "{output}");
    assert!(
        output.contains("2 assertion"),
        "the count should be reported so an empty script is distinguishable:\n{output}"
    );

    server.stop();
}

#[test]
fn a_server_that_is_not_there_fails_rather_than_hanging() {
    // A crashed or absent server must be a reported failure, not a test that
    // hangs until CI kills the job. Detecting the difference is the point.
    let dir = scratch("no-server");
    let script = dir.join("anything.lua");
    std::fs::write(&script, "bot.join('nobody')").expect("write");

    // Port 1 on loopback: nothing listens there, and binding it needs root.
    let started = std::time::Instant::now();
    let output = Command::new(bot_binary())
        .arg("run")
        .arg(&script)
        .arg("--server")
        .arg("127.0.0.1:1")
        .output()
        .expect("the bot binary should run");
    let elapsed = started.elapsed();

    assert_ne!(output.status.code(), Some(0), "connecting must have failed");
    assert!(
        elapsed < std::time::Duration::from_secs(60),
        "it must fail rather than hang; took {elapsed:?}"
    );
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not connect"),
        "the failure must say what went wrong"
    );
}

#[test]
fn a_missing_script_is_a_clear_error() {
    let output = Command::new(bot_binary())
        .arg("run")
        .arg("/definitely/not/a/script.lua")
        .arg("--server")
        .arg("127.0.0.1:1")
        .output()
        .expect("run");

    assert_ne!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not read"),
        "the error must name the problem"
    );
}

#[test]
fn the_smoke_script_passes_against_a_live_server() {
    let server = start("smoke");
    let (code, output) = run_script(&server, &repo_script("smoke_join.lua"));
    assert_eq!(code, 0, "smoke_join.lua should pass:\n{output}");
    server.stop();
}

#[test]
fn mine_3x3_passes_and_proves_the_unit_arithmetic() {
    // The canonical end-to-end proof of the 27-unit design, as a script rather
    // than a Rust test — which is the point of having a scripting harness at
    // all: a scenario a modder can read and modify.
    let server = start("mine-3x3");
    let (code, output) = run_script(&server, &repo_script("mine_3x3.lua"));
    assert_eq!(code, 0, "mine_3x3.lua should pass:\n{output}");
    server.stop();
}

#[test]
fn subnode_mining_passes_and_proves_the_spares_arithmetic() {
    let server = start("subnode-mining");
    let (code, output) = run_script(&server, &repo_script("subnode_mining.lua"));
    assert_eq!(code, 0, "subnode_mining.lua should pass:\n{output}");
    server.stop();
}

#[test]
fn churn_passes_and_leaves_the_world_as_it_found_it() {
    let server = start("churn");
    let (code, output) = run_script(&server, &repo_script("churn.lua"));
    assert_eq!(code, 0, "churn.lua should pass:\n{output}");
    server.stop();
}

#[test]
fn a_long_write_burst_does_not_stall_the_connection() {
    // A regression test for a cancellation-safety bug in the SERVER, which
    // this test found and which took three wrong diagnoses to pin down.
    //
    // `frame::read` reads a 4-byte length prefix and then the body: two
    // sequential awaits. The server's connection loop had it directly inside a
    // `tokio::select!`, which cancels the branches that do not win — so a timer
    // or a broadcast firing between those two reads discarded the partial frame
    // and left the stream mid-message. The next read treated body bytes as a
    // length prefix, the decode failed, and the client was disconnected for a
    // protocol error it had not committed.
    //
    // It surfaced as "connection stream failed" on the CLIENT's write, which is
    // why the first two fixes went after client back-pressure instead. The
    // giveaway was that it failed in debug builds and passed in release: both
    // load and debug widen the window between the two awaits.
    //
    // The burst is deliberately large so the window is hit reliably.
    let server = start("write-burst");
    let dir = scratch("write-burst-script");
    let script = dir.join("burst.lua");
    std::fs::write(
        &script,
        // Chat rather than block edits. What this test is about is a client
        // writing hard without reading — the transport, not the world — and
        // chat is the cheapest message that does that. Placing would now need
        // an inventory to draw on, which would make the burst a test of
        // whether the player is carrying anything.
        "bot.join('burst')\n\
         for i = 0, 2000 do\n\
           bot.chat('burst ' .. i)\n\
         end\n\
         bot.disconnect()",
    )
    .expect("write");

    let (code, output) = run_script(&server, &script);
    assert_eq!(code, 0, "a long write burst must not stall:\n{output}");

    server.stop();
}

#[test]
fn a_small_swarm_runs_and_reports_latency() {
    // Swarm mode from the CLI, as documented in the README. Four bots for two
    // seconds: enough to exercise the path, fast enough for `cargo test`.
    let server = start("swarm");
    let output = Command::new(bot_binary())
        .arg("swarm")
        .arg("4")
        .arg("--server")
        .arg(server.local_addr().to_string())
        .arg("--duration")
        .arg("2")
        .output()
        .expect("run");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    assert_eq!(
        output.status.code(),
        Some(0),
        "the swarm should finish healthy:\n{combined}"
    );
    assert!(combined.contains("bots healthy: 4/4"), "{combined}");
    for label in ["mean", "p50", "p95", "p99", "max"] {
        assert!(
            combined.contains(label),
            "the latency report should include {label}:\n{combined}"
        );
    }

    server.stop();
}

#[test]
fn an_unknown_swarm_behaviour_is_refused() {
    let output = Command::new(bot_binary())
        .arg("swarm")
        .arg("1")
        .arg("--server")
        .arg("127.0.0.1:1")
        .arg("--behavior")
        .arg("teleport-everywhere")
        .output()
        .expect("run");

    assert_ne!(output.status.code(), Some(0));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown behavior"),
        "an unknown behaviour must be refused rather than silently doing nothing"
    );
}

#[test]
fn replay_applies_a_recording_against_a_live_server() {
    let server = start("replay");
    let dir = scratch("replay-session");
    let session = dir.join("session.log");

    // **The surface material, ASKED FOR rather than written down.** This used
    // to place a hard-coded `2`, which was the id the reference world's top
    // block happened to have — so giving the generator a layer of absorbent
    // ground on top broke a test about the replay format, with an error about
    // not carrying anything. A number that means "whatever the world is made
    // of" has to be looked up or it is a guess with a date on it.
    let surface = block_on(async {
        let mut bot = bot::Bot::connect(
            server.local_addr(),
            Identity::generate().expect("identity"),
            server.cert_fingerprint(),
        )
        .await
        .expect("connect");
        bot.join("Surveyor").await.expect("join");
        let id = bot
            .material_table()
            .expect("the server should have sent a material table")
            .into_iter()
            .find(|entry| entry.name == "core:ground")
            .map(|entry| entry.id)
            .expect("the reference generator's surface material");
        bot.disconnect().await;
        id
    });

    std::fs::write(
        &session,
        // Digs first, then builds with what the digging yielded. A recording
        // that placed before digging would need the player to be carrying
        // something at tick zero, and nobody is.
        //
        // The worldgen fills BELOW its heightmap, so y = -1 is the highest
        // block that actually exists and y = 0 is the air above it — dig the
        // first, build in the second.
        format!(
            "# a tiny recorded session\n\
             0 dig_block 2 -1 0\n\
             2 dig_block 1 -1 0\n\
             4 place 2 0 0 {surface}\n\
             6 place 1 0 0 {surface}\n"
        ),
    )
    .expect("write");

    let output = Command::new(bot_binary())
        .arg("replay")
        .arg(&session)
        .arg("--server")
        .arg(server.local_addr().to_string())
        .output()
        .expect("run");

    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(output.status.code(), Some(0), "{combined}");
    assert!(
        combined.contains("4 command(s) applied"),
        "every command should be applied:\n{combined}"
    );

    server.stop();
}

#[test]
fn chisel_sculpt_passes_and_proves_the_subnode_round_trip() {
    // **Task 09's [A] criterion, as a scenario a modder can read and change.**
    // Chisel 13 of a block's 27 cells, hold 13 spare nodes, put them back, and
    // get a block that is 13 cells full — not a cube and not nothing.
    //
    // The Rust version of this lives in `crates/bot/tests/placement.rs`. Having
    // both is the point of a scripting harness at all: one proves the engine,
    // the other proves the engine is usable from outside it.
    let server = start("chisel-sculpt");
    let (code, output) = run_script(&server, &repo_script("chisel_sculpt.lua"));
    assert_eq!(code, 0, "chisel_sculpt.lua should pass:\n{output}");
    server.stop();
}

#[test]
fn a_malformed_recording_is_refused_with_a_line_number() {
    let dir = scratch("bad-replay");
    let session = dir.join("bad.log");
    std::fs::write(&session, "0 place 70 6 70 2\n1 teleport 1 2 3\n").expect("write");

    let output = Command::new(bot_binary())
        .arg("replay")
        .arg(&session)
        .arg("--server")
        .arg("127.0.0.1:1")
        .output()
        .expect("run");

    assert_ne!(output.status.code(), Some(0));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("line 2"),
        "the line must be named: {stderr}"
    );
    assert!(stderr.contains("teleport"), "{stderr}");
}
