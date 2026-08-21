// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The immediate-mode HUD a trusted client script draws.
//!
//! # Two tiers, two threat models
//!
//! [`crate::ui`] is tier 1: an untrusted server mod describes a widget tree as
//! DATA and the client renders it. Nothing crosses the wire that could run.
//!
//! This is tier 2, and it is the opposite trade. A HUD is not a tree — a health
//! bar's length is a function of the player's health, and expressing "a
//! function of" as data means inventing an expression language, which is a
//! worse thing to put on the wire than a sandboxed script. So a client script
//! RUNS, once per frame, and what it produces is this [`Frame`]: a flat list of
//! draw commands with no logic left in it.
//!
//! The trust that buys is real and is charter rule 10's: a pushed client script
//! gets a hard sandbox — no filesystem, no network, no `os`, no `io`, no binary
//! `load` — plus an instruction and memory ceiling. The player also chose to
//! connect to this server, which is the only trust decision in the model.
//!
//! # Virtual resolution: a fixed HEIGHT, not a fixed size
//!
//! Commands are in virtual pixels against a canvas [`VIRTUAL_HEIGHT`] tall and
//! **as wide as the window's aspect ratio makes it**. A script that assumed a
//! fixed width would be written for one monitor: the first ultrawide player
//! would find the hotbar somewhere off to the left of the middle. A fixed
//! height plus [`Anchor`] covers what a HUD actually wants — "16 up from the
//! bottom edge, centred" is expressible on any window, and text stays the same
//! apparent size on all of them.
//!
//! # Integers, and why
//!
//! Every coordinate here is an `i16` or a `u16` and every fraction is per-mille
//! ([`Fill`]). None of this is simulation state, so charter rule 4 does not
//! reach it — but a `NaN` arriving from Lua and travelling into a layout
//! calculation is a hazard whatever the rule says, and the cheapest way to not
//! have it is a type that cannot represent one. It is the same argument
//! [`crate::ui::Widget::Slider`] makes.
//!
//! # Chat is not in [`Builtin`], on purpose
//!
//! A script may hide the engine's crosshair, hotbar, health and dig progress,
//! because a mod that replaces them wants the originals gone. It may not hide
//! chat, and that is the same decision that put chat in the engine at all:
//! moderation depends on a player being able to see what is said, and a HUD
//! script that could take the chat window away could take a warning away with
//! it.

mod frame;
mod state;

pub use frame::{Anchor, Builtin, Command, Fill, Frame, HudError, Limits, Mark, VIRTUAL_HEIGHT};
pub use state::{Carried, HeldTool, Look, State};
