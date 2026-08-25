// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! What a mod asks the server to do with a dialog, and who does it.
//!
//! The same shape [`crate::sound::Access`] uses: `core` declares what it wants
//! done, the server installs something that does it, and the Lua binding calls
//! through the slot. `core` therefore never learns what a connection is.

/// A dialog a mod wants on somebody's screen.
#[derive(Debug, Clone, PartialEq)]
pub struct ShowRequest {
    /// The player's canonical UUID (charter rule 13 — never the display name).
    pub player: String,
    /// The mod's name for this dialog, echoed back on every event.
    pub form: String,
    /// What to draw.
    pub tree: super::Tree,
    /// Whether this replaces a dialog already open, rather than opening one.
    pub update: bool,
    /// Whether the client should draw it as a small prompt sized to its
    /// contents rather than as the full sheet every other screen takes.
    ///
    /// See [`crate::proto::ServerMessage::ShowDialog::compact`]: the engine
    /// cannot tell a prompt from an inventory by looking at the tree, so the
    /// mod says, and saying nothing means the sheet.
    pub compact: bool,
}

/// The server's side of the dialog API.
pub trait Access: Send + Sync {
    /// Shows or replaces a dialog. Returns whether the player was there to see
    /// it — a mod's only feedback, and deliberately not a promise it rendered.
    fn show(&self, request: &ShowRequest) -> bool;

    /// Closes a dialog. Returns whether one was open.
    fn close(&self, player: &str, form: &str) -> bool;
}

/// Who is allowed to act on an event, and on which dialog.
///
/// # Why events are routed by owner rather than broadcast
///
/// A dialog belongs to the mod that opened it. If every mod saw every event,
/// one mod could watch what a player typed into another's dialog — a password
/// field, a trade amount — and any mod could act on a button it did not put
/// there. So the server records who opened each form and delivers events only
/// to that mod.
///
/// The form name is namespaced with the owning mod's id for the same reason ids
/// are everywhere else (charter rule 8): two mods must be able to use the name
/// `"inventory"` without colliding, and a mod must not be able to name a form
/// into somebody else's namespace and receive their events.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owner {
    /// The mod that opened the dialog.
    pub mod_id: String,
    /// The qualified form name.
    pub form: String,
}
