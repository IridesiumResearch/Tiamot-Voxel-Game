// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Declarative UI: what a server mod may describe, as data.
//!
//! # The trust model, which is the whole design
//!
//! Charter rule 14 says every server-pushed asset is hostile input, and a UI is
//! the asset with the most obvious reason to want to be code. So it is not
//! code. A server mod describes a widget TREE — containers, labels, buttons,
//! slots — and the client renders it. **Nothing a server sends is executed.**
//!
//! That is the tier-1 half of Task 14. Tier 2 — an immediate-mode HUD for
//! scripts a client has chosen to trust — is a different mechanism with a
//! different threat model, and lives elsewhere.
//!
//! # Layout is flex, not coordinates
//!
//! Widgets say how they want to GROW, not where they are. A mod that placed
//! things at absolute pixels would be writing for one window size, and the
//! first player on a different monitor would see it broken. Rows, columns,
//! gaps, padding and alignment cover what a dialog needs; anything that needs
//! more is a case for a new container, not for coordinates.
//!
//! # Styling is a token set, deliberately small
//!
//! [`Style`] carries a background, a border, an optional nine-slice image and
//! text colour and size. That is all. The pressure to grow this into a
//! stylesheet language should be resisted every time: a constrained set is what
//! keeps a mod's dialog looking like it belongs to the game a player is in, and
//! what keeps the renderer swappable.
//!
//! # On egui
//!
//! The client renders this with egui today. **No egui type appears here**, and
//! charter rule 3 makes that structural rather than a matter of taste: `core`
//! must not depend on a windowing or render crate at all. The schema is the
//! contract; the renderer is an implementation detail that can be replaced
//! without a protocol change.
//!
//! # Versioning
//!
//! There is no cross-version widget negotiation, because there are no
//! cross-version sessions: [`crate::proto::PROTOCOL_VERSION`] is checked at
//! join and a mismatch is refused. Client and server therefore always agree on
//! exactly this widget set. An unknown widget is a MOD's mistake — a typo in
//! Lua — and is caught when the mod builds the tree, with a message naming it,
//! rather than travelling to a client that cannot draw it.

pub mod host;
mod layout;
mod limits;
#[cfg(test)]
mod proptests;
mod tree;

pub use layout::{Laid, Measure, Rect, layout};
pub use limits::{Limits, UiError, check};
pub use tree::{Align, Build, Children, Direction, Node, Style, Tree, Widget};
