// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The Tiamot server, as a library.
//!
//! Charter rule 2: the server is the game. Singleplayer is this server running
//! in-process on loopback, which is why this is a library with a thin binary on
//! top rather than a binary alone — the client embeds it directly, and there is
//! exactly one simulation path.
//!
//! # The async boundary stops at the transport
//!
//! [`quinn`] is async, so the transport runs on a tokio runtime. The simulation
//! does not: a tick that can yield in the middle is a tick whose result depends
//! on the scheduler, which is the opposite of what charter rule 4 requires.
//! Network work happens on the runtime, hands messages to the simulation
//! thread through channels, and reads results back the same way.

#![forbid(unsafe_code)]

pub mod announce;
pub mod cert;
pub mod checkmods;
pub mod config;
pub mod containers;
pub mod content;
pub mod ent;
pub mod fluid;
pub mod handle;
pub mod hud;
pub mod lease;
pub mod light;
pub mod rcon;
pub mod shutdown;
pub mod sim;
pub mod storage;
pub mod trace;
pub mod transport;
pub mod world;

pub use cert::{CertError, ServerCert};
pub use config::{Config, ConfigError};
pub use handle::{ServerHandle, Settings, StartError};
pub use sim::Control;
