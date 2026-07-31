// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The `bot` command-line tool.
//!
//! Three modes, documented in `crates/bot/README.md`:
//!
//! - `bot run script.lua --server addr` — one scripted session; the exit code
//!   is the assertions.
//! - `bot swarm N --server addr --duration 60s` — N bots wandering and editing,
//!   with a latency report.
//! - `bot replay session.json --server addr` — replays a recorded session.
//!
//! # The exit code is the product
//!
//! Everything here is meant to be run by CI as much as by a person, so every
//! mode exits 0 only when what it was asked to check actually held. A tool that
//! printed "FAILED" and exited 0 would be worse than no tool.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use bot::runner::SwarmStats;
use bot::script::Channel;
use clap::{Parser, Subcommand};
use tiamot_core::identity::Identity;

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(
    name = "bot",
    about = "Scripted headless Tiamot client: integration tests, load, and benchmarks",
    version
)]
struct Cli {
    #[command(subcommand)]
    mode: Mode,
}

#[derive(Debug, Subcommand)]
enum Mode {
    /// Run one Lua script against a server.
    Run {
        /// The script to run.
        script: PathBuf,
        /// Server address.
        #[arg(long, value_name = "addr")]
        server: SocketAddr,
        /// Display name to join under.
        #[arg(long, default_value = "bot")]
        name: String,
    },
    /// Run N wandering bots against a server and report latency.
    Swarm {
        /// How many bots.
        count: u32,
        /// Server address.
        #[arg(long, value_name = "addr")]
        server: SocketAddr,
        /// How long to run, in seconds.
        #[arg(long, default_value_t = 60)]
        duration: u64,
        /// Which behaviour to run. Only `wander` exists so far.
        #[arg(long, default_value = "wander")]
        behavior: String,
        /// Material id to build with.
        #[arg(long, default_value_t = 2)]
        material: u16,
        /// Seed for the movement sequence, so a run can be repeated.
        #[arg(long, default_value_t = 1)]
        seed: u64,
    },
    /// Run the macro benchmark: fixed workload, tick-time distribution.
    ///
    /// Starts its own server, so there is nothing to point it at — the whole
    /// value is that the workload and the world are identical every run.
    Bench {
        /// Where to write the machine-readable report.
        #[arg(long, value_name = "path")]
        json: Option<PathBuf>,
        /// Baseline to compare against. Exits non-zero on a regression.
        #[arg(long, value_name = "path")]
        baseline: Option<PathBuf>,
        /// How many bots replay the session.
        #[arg(long, default_value_t = 4)]
        bots: u32,
        /// How many rounds of the standard session.
        #[arg(long, default_value_t = 200)]
        rounds: u64,
    },
    /// Replay a recorded session.
    Replay {
        /// The recording.
        session: PathBuf,
        /// Server address.
        #[arg(long, value_name = "addr")]
        server: SocketAddr,
    },
}

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        .init();

    let cli = Cli::parse();
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("could not start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    let code = match cli.mode {
        Mode::Run {
            script,
            server,
            name,
        } => run_script_mode(&runtime, &script, server, &name),
        Mode::Swarm {
            count,
            server,
            duration,
            behavior,
            material,
            seed,
        } => swarm_mode(
            &runtime,
            count,
            server,
            Duration::from_secs(duration),
            &behavior,
            material,
            seed,
        ),
        Mode::Replay { session, server } => replay_mode(&runtime, &session, server),
        Mode::Bench {
            json,
            baseline,
            bots,
            rounds,
        } => bench_mode(&runtime, json.as_deref(), baseline.as_deref(), bots, rounds),
    };

    if code == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

fn run_script_mode(
    runtime: &tokio::runtime::Runtime,
    script: &PathBuf,
    server: SocketAddr,
    name: &str,
) -> u8 {
    let source = match std::fs::read_to_string(script) {
        Ok(source) => source,
        Err(err) => {
            eprintln!("could not read `{}`: {err}", script.display());
            return 1;
        }
    };

    let identity = match Identity::generate() {
        Ok(identity) => identity,
        Err(err) => {
            eprintln!("could not generate an identity: {err}");
            return 1;
        }
    };

    let connected = runtime.block_on(bot::Bot::connect_trusting(server, identity));
    let client = match connected {
        Ok(client) => client,
        Err(err) => {
            eprintln!("could not connect to {server}: {err}");
            return 1;
        }
    };
    eprintln!(
        "connected to {server}, certificate fingerprint {}",
        hex(&client.cert_fingerprint())
    );

    let (channel, commands, replies) = Channel::pair();
    let driver = runtime.spawn(bot::runner::drive(client, commands, replies));

    // The script joins under the given name unless it joins itself, so a
    // scenario can test the join flow when it wants to and ignore it otherwise.
    let source = if source.contains("bot.join") {
        source
    } else {
        format!("bot.join('{name}')\n{source}")
    };

    let outcome = match bot::run_script(&source, &script.display().to_string(), channel) {
        Ok(outcome) => outcome,
        Err(err) => {
            eprintln!("could not run the script: {err}");
            return 1;
        }
    };

    // Dropping the channel ends the driver.
    runtime.block_on(async {
        let _ = tokio::time::timeout(Duration::from_secs(5), driver).await;
    });

    if outcome.passed {
        println!(
            "PASS {} ({} assertion(s))",
            script.display(),
            outcome.assertions
        );
    } else {
        eprintln!("FAIL {}", script.display());
        if let Some(failure) = &outcome.failure {
            eprintln!("  {failure}");
        }
    }
    outcome.exit_code()
}

fn swarm_mode(
    runtime: &tokio::runtime::Runtime,
    count: u32,
    server: SocketAddr,
    duration: Duration,
    behavior: &str,
    material: u16,
    seed: u64,
) -> u8 {
    if behavior != "wander" {
        eprintln!("unknown behavior `{behavior}`; only `wander` exists so far");
        return 1;
    }

    println!("swarm: {count} bots, {duration:?}, behaviour `{behavior}`, against {server}");
    let started = std::time::Instant::now();

    let results: Vec<Result<SwarmStats, String>> = runtime.block_on(async {
        let mut handles = Vec::new();
        for index in 0..count {
            let name = format!("swarm{index}");
            handles.push(tokio::spawn(bot::wander(
                server,
                name,
                material,
                duration,
                seed.wrapping_add(u64::from(index))
                    .wrapping_mul(0x9E37_79B9),
            )));
        }
        let mut results = Vec::new();
        for handle in handles {
            results.push(match handle.await {
                Ok(Ok(stats)) => Ok(stats),
                Ok(Err(err)) => Err(err.to_string()),
                Err(err) => Err(format!("bot task panicked: {err}")),
            });
        }
        results
    });

    let elapsed = started.elapsed();
    let healthy = results.iter().filter(|r| r.is_ok()).count();
    let mut all_latencies: Vec<u64> = Vec::new();
    let mut edits = 0u64;
    let mut confirmed = 0u64;
    for stats in results.iter().flatten() {
        all_latencies.extend_from_slice(&stats.latencies_us);
        edits += stats.edits;
        confirmed += stats.confirmed;
    }

    let combined = SwarmStats {
        latencies_us: all_latencies,
        edits,
        confirmed,
        healthy: healthy == count as usize,
    };

    println!("  ran for {elapsed:?}");
    println!("  bots healthy: {healthy}/{count}");
    println!("  edits sent: {edits}, confirmed: {confirmed}");
    println!("  edit round-trip, client-observed:");
    println!("    mean {} us", combined.mean_us());
    println!("    p50  {} us", combined.percentile_us(50));
    println!("    p95  {} us", combined.percentile_us(95));
    println!("    p99  {} us", combined.percentile_us(99));
    println!("    max  {} us", combined.percentile_us(100));

    for (index, result) in results.iter().enumerate() {
        if let Err(err) = result {
            eprintln!("  bot {index} failed: {err}");
        }
    }

    if healthy == count as usize {
        println!("OK");
        0
    } else {
        eprintln!(
            "FAILED: {} of {count} bots did not finish",
            count as usize - healthy
        );
        1
    }
}

fn replay_mode(runtime: &tokio::runtime::Runtime, session: &PathBuf, server: SocketAddr) -> u8 {
    let recording = match std::fs::read_to_string(session) {
        Ok(text) => text,
        Err(err) => {
            eprintln!("could not read `{}`: {err}", session.display());
            return 1;
        }
    };

    let commands = match bot::replay::parse(&recording) {
        Ok(commands) => commands,
        Err(err) => {
            eprintln!("could not parse `{}`: {err}", session.display());
            return 1;
        }
    };
    println!(
        "replaying {} command(s) from {}",
        commands.len(),
        session.display()
    );

    let identity = match Identity::generate() {
        Ok(identity) => identity,
        Err(err) => {
            eprintln!("could not generate an identity: {err}");
            return 1;
        }
    };

    let outcome = runtime.block_on(async {
        let client = bot::Bot::connect_trusting(server, identity).await?;
        bot::replay::run(client, &commands, "replay").await
    });

    match outcome {
        Ok(applied) => {
            println!("OK: {applied} command(s) applied");
            0
        }
        Err(err) => {
            eprintln!("FAILED: {err}");
            1
        }
    }
}

/// Runs the macro benchmark against a server it starts itself.
fn bench_mode(
    runtime: &tokio::runtime::Runtime,
    json: Option<&std::path::Path>,
    baseline: Option<&std::path::Path>,
    bots: u32,
    rounds: u64,
) -> u8 {
    use tiamot_core::identity::Allowlist;
    use tiamot_core::interest::ViewDistance;
    use tiamot_server::{ServerHandle, Settings};

    let world = std::env::temp_dir().join("tiamot-macro-bench");
    let _ = std::fs::remove_dir_all(&world);
    if let Err(err) = std::fs::create_dir_all(&world) {
        eprintln!("could not create the benchmark world: {err}");
        return 1;
    }

    // A FIXED seed and a fresh world. The point of the benchmark is that the
    // only thing changing between runs is the server.
    let server = match ServerHandle::start(&Settings {
        bind_addr: "127.0.0.1:0".parse().expect("loopback"),
        world_path: world,
        max_players: 64,
        allowlist: Allowlist::open(),
        view_distance: ViewDistance::MINIMUM,
        mods_path: None,
        seed: Some(0x7149_7231),
        rcon: None,
        materials: vec!["bench:stone".to_owned()],
    }) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("could not start the benchmark server: {err}");
            return 1;
        }
    };

    let addr = server.local_addr();
    let session = bot::bench::standard_session(bots, rounds);
    println!(
        "macro bench: {bots} bots, {rounds} rounds, {} commands",
        session.len()
    );

    // Let startup settle, then throw away the samples it produced: the first
    // ticks include mod loading and first-visit chunk generation, which are
    // real but are not what a steady-state benchmark is measuring.
    std::thread::sleep(bot::bench::WARMUP);
    let _ = server.control().take_tick_samples();

    let outcome = runtime.block_on(async {
        let mut handles = Vec::new();
        for index in 0..bots {
            let session = session.clone();
            handles.push(tokio::spawn(async move {
                let identity = Identity::generate().map_err(|err| err.to_string())?;
                let client = bot::Bot::connect_trusting(addr, identity)
                    .await
                    .map_err(|err| err.to_string())?;
                bot::replay::run(client, &session, &format!("bench{index}"))
                    .await
                    .map_err(|err| err.to_string())
            }));
        }
        let mut failures = Vec::new();
        for handle in handles {
            match handle.await {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => failures.push(err),
                Err(err) => failures.push(format!("bot task panicked: {err}")),
            }
        }
        failures
    });

    let samples = server.control().take_tick_samples();
    let report = bot::bench::TickReport::from_samples(
        &samples,
        server.control().over_budget_ticks(),
        server.control().dropped(),
        bots,
        rounds,
    );
    server.stop();

    for failure in &outcome {
        eprintln!("  bot failed: {failure}");
    }

    print!("{}", report.to_table());

    if let Some(path) = json {
        match std::fs::write(path, report.to_json()) {
            Ok(()) => println!("wrote {}", path.display()),
            Err(err) => {
                eprintln!("could not write `{}`: {err}", path.display());
                return 1;
            }
        }
    }

    if !outcome.is_empty() {
        eprintln!("FAILED: {} bot(s) did not finish", outcome.len());
        return 1;
    }

    let Some(baseline_path) = baseline else {
        println!("OK (no baseline given, so nothing was gated)");
        return 0;
    };

    let baseline_text = match std::fs::read_to_string(baseline_path) {
        Ok(text) => text,
        Err(err) => {
            eprintln!(
                "could not read the baseline `{}`: {err}",
                baseline_path.display()
            );
            return 1;
        }
    };
    let baseline_report = match bot::bench::TickReport::from_json(&baseline_text) {
        Ok(report) => report,
        Err(err) => {
            eprintln!("could not parse the baseline: {err}");
            return 1;
        }
    };

    let comparison = bot::bench::compare(&baseline_report, &report);
    if comparison.within_tolerance {
        println!("OK: {}", comparison.message);
        0
    } else {
        eprintln!("REGRESSION: {}", comparison.message);
        1
    }
}

/// Lowercase hex, for printing a fingerprint an operator can pin.
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}
