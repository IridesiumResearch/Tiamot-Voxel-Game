// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Per-mod persistent key/value storage.
//!
//! A mod needs somewhere to keep a fact about the world that is not attached to
//! a block, a chunk or an entity — which world this is, whether something has
//! happened yet, who a thing belongs to. Without it every such fact has to be
//! smuggled into a block somewhere, and a mod's state ends up being something a
//! player can dig up.
//!
//! # Keyed by the CALLING mod, always
//!
//! The mod id is not a parameter. A mod reads and writes its own storage and
//! cannot name another's, which makes the isolation a property of the API
//! rather than of everyone's good behaviour — the same reason `require` cannot
//! leave a mod's own directory. Two mods may use the key `"seen"` and mean
//! entirely different things.
//!
//! # What a value may be
//!
//! A string, a number, or a flag. Not an arbitrary table, and that is a
//! deliberate limit rather than an unfinished one: a table would need a
//! serialisation format that is part of the mod API forever, and the engine
//! would then own questions like what happens to a cycle, a function, or a
//! userdata. A mod that wants structure encodes it into a string, which is
//! something it can change without an engine release.
//!
//! # Charter rule 13 lives here in practice
//!
//! The obvious thing to keep in storage is "which player did X", and the
//! obvious way to write it is by name. Names are a per-server claim bound to a
//! UUID and can be rebound; UUIDs are identity. Storage takes strings, so it
//! cannot enforce that — but [`Value::uuid`] and [`Value::as_uuid`] exist so
//! the right thing is also the easy thing.

use std::collections::BTreeMap;

use crate::identity::PlayerUuid;

/// What a mod can store.
///
/// **`postcard` variants are position-encoded**, so appending is safe and
/// inserting or reordering silently reinterprets every saved world. See
/// [`crate::persist::codec`].
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum Value {
    /// Text.
    Text(String),
    /// A number. `f64`, because that is what a Lua number is.
    Number(f64),
    /// A flag.
    Flag(bool),
}

impl Value {
    /// A player's UUID, as the hex form [`PlayerUuid`] displays.
    ///
    /// Charter rule 13: mod state that means "this player" keys on the UUID and
    /// never the display name, because a name can be rebound and a UUID cannot.
    #[must_use]
    pub fn uuid(uuid: PlayerUuid) -> Self {
        Self::Text(uuid.to_hex())
    }

    /// Reads a value back as a player UUID, or `None` if it is not one.
    #[must_use]
    pub fn as_uuid(&self) -> Option<PlayerUuid> {
        match self {
            Self::Text(text) => PlayerUuid::from_hex(text).ok(),
            _ => None,
        }
    }

    /// The text, if this is text.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            _ => None,
        }
    }

    /// The number, if this is a number.
    #[must_use]
    pub const fn as_number(&self) -> Option<f64> {
        match self {
            Self::Number(value) => Some(*value),
            _ => None,
        }
    }

    /// The flag, if this is a flag.
    #[must_use]
    pub const fn as_flag(&self) -> Option<bool> {
        match self {
            Self::Flag(value) => Some(*value),
            _ => None,
        }
    }
}

/// One mod's whole storage.
///
/// A `BTreeMap` rather than a `HashMap`, for the reason every ordered container
/// in this crate is one: `keys()` is mod-visible, and a mod iterating its own
/// storage must get the same order twice — on one machine and on three
/// platforms. `HashMap` order is not stable even run to run in one process.
pub type Bag = BTreeMap<String, Value>;

/// Where a mod's storage calls reach.
///
/// The same seam shape as [`crate::ent::Access`] and [`crate::fluid::Access`],
/// and for the same reason: the store lives above `core` and the VM lives
/// inside it (charter rule 3).
pub trait Access: Send + Sync {
    /// Reads one of a mod's own keys.
    fn get(&self, mod_id: &str, key: &str) -> Option<Value>;

    /// Writes one of a mod's own keys. `None` deletes it.
    fn set(&self, mod_id: &str, key: &str, value: Option<Value>);

    /// Every key a mod has, in order.
    fn keys(&self, mod_id: &str) -> Vec<String>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_uuid_round_trips_through_a_stored_value() {
        // The whole reason these two helpers exist: charter rule 13 says mod
        // state keys on the UUID, and a mod that has to hand-roll the hex is a
        // mod that will eventually store the name instead.
        let uuid = PlayerUuid::from_bytes([0xAB; 32]);
        let stored = Value::uuid(uuid);
        assert_eq!(stored.as_uuid(), Some(uuid));
    }

    #[test]
    fn a_value_that_is_not_a_uuid_reads_as_none_rather_than_as_a_wrong_one() {
        assert_eq!(Value::Text("Iridesium".into()).as_uuid(), None);
        assert_eq!(Value::Number(1.0).as_uuid(), None);
        assert_eq!(Value::Flag(true).as_uuid(), None);
    }

    #[test]
    fn values_round_trip_through_postcard() {
        for value in [
            Value::Text("hello".into()),
            Value::Number(-1.5),
            Value::Flag(false),
        ] {
            let bytes = postcard::to_allocvec(&value).expect("encode");
            let back: Value = postcard::from_bytes(&bytes).expect("decode");
            assert_eq!(back, value);
        }
    }
}
