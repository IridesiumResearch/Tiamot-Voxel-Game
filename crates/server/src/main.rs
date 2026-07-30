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

mod config;
mod shutdown;

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;
use tracing::{error, info};

use crate::config::Config;

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

fn run(cli: &Cli) -> Result<(), config::ConfigError> {
    let config = Config::load(&cli.config)?;

    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %cli.config.display(),
        bind_addr = %config.bind_addr,
        world_path = %config.world_path.display(),
        max_players = config.max_players,
        "tiamot server starting"
    );

    // Task 06 replaces this with the real listener and Task 03 with the real
    // world database. Until then the server's only job is to come up cleanly
    // and go down cleanly, which is exactly what the acceptance criterion asks
    // for.
    info!("no simulation yet — waiting for shutdown signal (ctrl-c or SIGTERM)");

    let signal = shutdown::listen();
    signal.wait();

    info!("saving and shutting down");
    Ok(())
}
