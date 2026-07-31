// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! QUIC transport.
//!
//! Moves bytes and nothing else. Every rule about who may do what lives in
//! [`tiamot_core::session`]; see [`endpoint`]'s module docs for why.

pub mod endpoint;
pub mod frame;

pub use endpoint::{Shared, TransportError, accept_loop, bind, server_config};
pub use frame::FrameError;
