// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The server's end of a mod's own HUD values.
//!
//! **The engine has no health bar, no hunger bar and no experience bar, and
//! should not.** Charter rule 1 puts what any of those mean in a mod — but a
//! mod that could compute one and not draw it would be a mod that cannot
//! finish the job, so the engine carries the numbers and reads none of them.
//!
//! One mod's values reach that mod's own HUD script and no other's, the way
//! `game.storage` works and for the same reason: the isolation is a property
//! of the surface, not of good behaviour.

use std::sync::Arc;

use tiamot_core::PlayerUuid;
use tiamot_core::hud::Values;

/// The mod-facing handle on the per-player HUD values.
pub struct Shared {
    endpoint: Arc<crate::transport::Shared>,
}

impl Shared {
    /// Wraps the connection state the values are queued on.
    #[must_use]
    pub const fn new(endpoint: Arc<crate::transport::Shared>) -> Self {
        Self { endpoint }
    }
}

impl tiamot_core::hud::Access for Shared {
    fn set_hud(&self, mod_id: &str, player: [u8; 32], values: Values) -> bool {
        let uuid = PlayerUuid::from_bytes(player);
        // **Whether the player is here, not whether the write happened.** A mod
        // setting values for somebody who has left should hear that, because
        // the alternative is a mod that keeps a HUD updated for nobody and
        // cannot tell.
        if !self.endpoint.is_online(&uuid) {
            return false;
        }
        self.endpoint.set_hud_values(&uuid, mod_id, values);
        true
    }
}
