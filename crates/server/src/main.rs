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
use tiamot_core::{Registry, WorldDb};
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

/// The world database file inside the configured world directory.
const WORLD_FILE: &str = "world.sqlite";

/// Anything that stops the server starting or running.
#[derive(Debug, thiserror::Error)]
enum ServerError {
    /// The configuration file could not be read or understood.
    #[error(transparent)]
    Config(#[from] config::ConfigError),

    /// The world database could not be opened, or failed on the final flush.
    ///
    /// The source is boxed: a `WorldError` carries a whole codec error chain
    /// and is far larger than the other variants, which would make every
    /// `Result` in the startup path pay for the rare case.
    #[error("world database at `{path}`")]
    World {
        /// Path we tried to use.
        path: std::path::PathBuf,
        /// Why it failed.
        #[source]
        source: Box<tiamot_core::WorldError>,
    },
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

    // Charter rule 9's lifecycle is manifest scan → resolve → load → register →
    // FREEZE → world load → play. Mod loading arrives in Task 05, so the
    // registry currently holds only the engine's reserved materials — but the
    // world is opened AFTER it, in the right order, so Task 05 slots in without
    // moving this call.
    let mut registry = Registry::new();

    let world_file = config.world_path.join(WORLD_FILE);
    let world = WorldDb::open(&world_file, &mut registry).map_err(|source| ServerError::World {
        path: world_file.clone(),
        source: Box::new(source),
    })?;

    info!(
        world = %world.path().display(),
        materials = world.ids().len(),
        unknown_materials = world.materials().unknown().len(),
        "world opened"
    );
    if !world.materials().unknown().is_empty() {
        // Charter rule 8: content from an absent mod is preserved, not
        // destroyed. Operators should know it happened rather than discover it
        // when a player reports missing blocks.
        info!(
            count = world.materials().unknown().len(),
            "some materials in this world have no loaded mod; their blocks are \
             preserved and will render as unknown until the mod returns"
        );
    }

    // Task 06 replaces this with the real listener and tick loop. Until then
    // the server's job is to come up cleanly and go down cleanly.
    info!("no simulation yet — waiting for shutdown signal (ctrl-c or SIGTERM)");

    let signal = shutdown::listen();
    signal.wait();

    info!("saving and shutting down");
    world.close().map_err(|source| ServerError::World {
        path: world_file,
        source: Box::new(source),
    })?;
    info!("world flushed and closed");
    Ok(())
}
