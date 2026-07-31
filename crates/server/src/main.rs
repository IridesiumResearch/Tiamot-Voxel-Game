// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Headless Tiamot server.
//!
//! The server is the game (charter rule 2). Singleplayer runs this same code
//! embedded over loopback, so there is exactly one simulation path and no
//! "singleplayer-only" behaviour to drift out of sync.
//!
//! This binary is headless by construction: no windowing, no GPU, no audio. It
//! must build and run on a machine with no display server, and CI proves that
//! by building it on runners that have none.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use tiamot_core::identity::Allowlist;
use tiamot_server::config::{Config, ConfigError};
use tiamot_server::{ServerHandle, Settings, StartError, shutdown};
use tracing::{error, info};

/// Command-line arguments.
#[derive(Debug, Parser)]
#[command(
    name = "server",
    about = "Headless Tiamot voxel engine server",
    version
)]
struct Cli {
    /// Path to the server configuration file (TOML).
    #[arg(long, value_name = "path")]
    config: PathBuf,
}

fn main() -> ExitCode {
    // Presentation-layer only; never used from simulation code.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();

    match run(&cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Print the whole chain; a bare "could not parse config file" with
            // no cause is not an error message, it is a riddle.
            error!("{err}");
            let mut source = std::error::Error::source(&err);
            while let Some(cause) = source {
                error!("  caused by: {cause}");
                source = cause.source();
            }
            ExitCode::FAILURE
        }
    }
}

/// Anything that stops the server starting or running.
#[derive(Debug, thiserror::Error)]
enum ServerError {
    /// The configuration file could not be read or understood.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// The server could not be started.
    #[error(transparent)]
    Start(#[from] StartError),

    /// A thread panicked, so the final save may be incomplete.
    #[error("the server did not shut down cleanly; the last save may be incomplete")]
    UncleanShutdown,
}

fn run(cli: &Cli) -> Result<(), ServerError> {
    let config = Config::load(&cli.config)?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %cli.config.display(),
        bind_addr = %config.bind_addr,
        world_path = %config.world_path.display(),
        max_players = config.max_players,
        "tiamot server starting"
    );

    // The SAME call singleplayer makes. Charter rule 2: the server is the game,
    // and a second startup path would be a second set of bugs that only appear
    // in one mode.
    let server = ServerHandle::start(&Settings {
        bind_addr: config.bind_addr,
        world_path: config.world_path.clone(),
        max_players: config.max_players,
        allowlist: Allowlist::open(),
        seed: config.seed,
        mods_path: config.mods_path.clone(),
        view_distance: tiamot_core::interest::ViewDistance::clamped(
            config.view_distance,
            config.vertical_view_distance,
        ),
        rcon: config
            .rcon
            .as_ref()
            .map(|rcon| (rcon.bind_addr, rcon.token.clone())),
        // Mods supply materials; this is only for a server configured without
        // any that still wants something placeable.
        materials: Vec::new(),
    })?;

    info!(
        local_addr = %server.local_addr(),
        tick_rate_hz = tiamot_core::tick::TICK_RATE_HZ,
        "server running — waiting for shutdown signal (ctrl-c or SIGTERM)"
    );

    let signal = shutdown::listen();
    signal.wait();

    info!(
        ticks = server.control().tick(),
        dropped = server.control().dropped(),
        "shutting down"
    );

    if server.stop() {
        info!("world flushed and closed");
        Ok(())
    } else {
        Err(ServerError::UncleanShutdown)
    }
}
