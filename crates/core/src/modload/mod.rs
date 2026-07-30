// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Mod discovery, dependency resolution, and load ordering.
//!
//! # Determinism is the requirement, not a nicety
//!
//! Two servers with the same mod directory must produce the same load order,
//! on any machine, in any filesystem order. Load order decides registration
//! order, registration order decides numeric material ids, and numeric ids go
//! into every chunk blob (charter rule 8). A resolver that returned a different
//! order on a different filesystem would give two servers incompatible worlds
//! from identical inputs.
//!
//! So: topological sort with an **alphabetical tiebreak**, and directory
//! scanning sorted before anything looks at it. `resolve` is a pure function of
//! the manifest set.
//!
//! # Errors have to be actionable
//!
//! A dependency failure is something a server operator sees, usually while
//! something is broken and they are in a hurry. Every error here names the mod
//! that has the requirement, the requirement itself, and what was actually
//! found — a cycle error prints the whole cycle, not just that one exists.

mod manifest;
mod resolve;

pub use manifest::{DiscoveredMod, ENTRY_FILE, ManifestError, ModManifest, scan_directory};

/// The entry script filename every mod must have.
#[must_use]
pub const fn manifest_entry_file() -> &'static str {
    ENTRY_FILE
}
pub use resolve::{ResolveError, ResolvedMod, ResolvedSet, resolve};
