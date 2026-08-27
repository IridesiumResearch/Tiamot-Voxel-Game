// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Named actions, and the physical inputs bound to them.
//!
//! Charter rule 11: **mods register named actions and the engine owns the key
//! bindings.** A mod never reads a key, and this module is why it never needs
//! to — it is the only place in the client that knows a keyboard exists.
//!
//! # One system, no special cases
//!
//! The engine's own controls are not privileged. Walking forward is
//! `engine:move_forward` and it goes through [`Actions::engine`], the same
//! registry a mod's `core_tools:chisel_mode` lands in, bound by the same
//! [`Bindings`] and rebindable by the same UI. A reserved action differs from a
//! mod's in exactly one way: its [`Source`], which is what the settings screen
//! groups by and nothing else reads.
//!
//! # Bindings are keyed by ACTION, not by key
//!
//! The file maps `"engine:jump" -> Space`, never `Space -> "engine:jump"`. A
//! server that adds, removes or renames a mod action therefore cannot scramble
//! what the player already bound: an id that is gone is an entry nobody looks
//! up, and a new id falls back to its default. The other direction would
//! renumber every binding whenever a mod list changed.
//!
//! # Physical, not logical
//!
//! [`Input::Key`] holds a `KeyCode` — a position on the keyboard — so a binding
//! made on QWERTY stays under the same finger on AZERTY. The cost is that the
//! settings screen must show a name for a position, which is `KeyCode`'s own
//! and is what a player sees printed on a US keyboard.

use std::collections::BTreeMap;
use winit::event::MouseButton;
use winit::keyboard::KeyCode;

/// A physical control: a key by position, or a mouse button.
///
/// Serialised as an externally tagged enum, so the config file reads
/// `"engine:jump" = { key = "Space" }` — self-describing, and the key name is
/// `KeyCode`'s own variant name rather than a table this crate would have to
/// keep in step with winit.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Input {
    /// A keyboard position, named by `KeyCode`.
    Key(KeyCode),
    /// A mouse button.
    Mouse(MouseButton),
}

impl std::fmt::Display for Input {
    /// What the settings screen shows. `KeyCode`'s debug name is the US-layout
    /// legend, which is the best answer available for a physical position.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Key(code) => write!(f, "{code:?}"),
            Self::Mouse(MouseButton::Left) => write!(f, "Left mouse"),
            Self::Mouse(MouseButton::Right) => write!(f, "Right mouse"),
            Self::Mouse(MouseButton::Middle) => write!(f, "Middle mouse"),
            Self::Mouse(button) => write!(f, "Mouse {button:?}"),
        }
    }
}

/// Who registered an action, and therefore who the settings screen credits.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Source {
    /// The engine's own controls. Reserved: a mod may not register these ids.
    Engine,
    /// A mod, by its id.
    Mod(String),
}

impl Source {
    /// The heading this action appears under.
    #[must_use]
    pub fn label(&self) -> &str {
        match self {
            Self::Engine => "engine",
            Self::Mod(id) => id,
        }
    }
}

/// One named thing a player can do.
#[derive(Debug, Clone)]
pub struct Action {
    /// Namespaced id, `"engine:jump"` or `"core_tools:chisel_mode"`.
    pub id: String,
    /// One line, shown in the settings screen.
    pub description: String,
    /// Who registered it.
    pub source: Source,
    /// What it is bound to until a player says otherwise.
    ///
    /// `None` is a legitimate answer — an action a mod ships unbound, which the
    /// player must bind before it does anything.
    pub default: Option<Input>,
}

/// Why a registration was refused.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RegisterError {
    /// The id is already taken.
    #[error("action `{0}` is already registered")]
    Duplicate(String),
    /// A mod tried to register an `engine:` id.
    #[error("action `{0}` is reserved: `engine:` belongs to the engine")]
    Reserved(String),
}

/// Turns a mod's `default_key` name into a physical key.
///
/// **The one place a string from a server becomes a `KeyCode`.** `crates/core`
/// must not depend on winit (charter rule 3), so the mod API and the protocol
/// carry the NAME — `"KeyW"`, `"Space"` — and this is where it lands.
///
/// Implemented through winit's own `Deserialize` rather than a table written
/// here, so the set of names this accepts is exactly the set winit defines and
/// cannot drift out of step with it as winit adds keys.
///
/// `None` for a name this build does not know, which is a mod written against a
/// newer winit and is a warning rather than an error — the action still works
/// the moment a player binds it.
#[must_use]
pub fn parse_key(name: &str) -> Option<Input> {
    if name.is_empty() {
        return None;
    }
    // serde's own string deserialiser: a bare string is exactly what a unit
    // variant looks like, and `KeyCode` is one. No JSON crate, and — since the
    // name comes from a server — no formatting an untrusted string into a
    // document that would then have to be escaped.
    let name: serde::de::value::StrDeserializer<'_, serde::de::value::Error> =
        serde::de::IntoDeserializer::into_deserializer(name);
    <KeyCode as serde::Deserialize>::deserialize(name)
        .ok()
        .map(Input::Key)
}

/// Why a bindings file could not be read or written.
#[derive(Debug, thiserror::Error)]
pub enum BindingsError {
    /// The file exists and could not be read, or could not be written.
    #[error("bindings file `{path}`: {source}")]
    Read {
        /// The file.
        path: std::path::PathBuf,
        /// What the filesystem said.
        source: std::io::Error,
    },
    /// The file exists and is not valid TOML, or names something unknown.
    #[error("bindings file `{path}` is not valid: {source}")]
    Parse {
        /// The file.
        path: std::path::PathBuf,
        /// What the parser said.
        source: Box<toml::de::Error>,
    },
    /// The bindings could not be turned into TOML.
    #[error("bindings could not be written to `{path}`: {source}")]
    Write {
        /// The file.
        path: std::path::PathBuf,
        /// What the serialiser said.
        source: Box<toml::ser::Error>,
    },
}

/// Every action the client knows about, engine and mod alike.
#[derive(Debug, Clone, Default)]
pub struct Actions {
    /// Ordered so the settings screen is stable between runs: engine actions in
    /// the order they are declared below, then each mod's in registration
    /// order. A `BTreeMap` would sort `hotbar_10` before `hotbar_2`.
    actions: Vec<Action>,
}

impl Actions {
    /// The engine's reserved actions, in the order the settings screen shows.
    ///
    /// Every control the client had hard-coded before this existed, so routing
    /// the window through the registry changes no behaviour. Charter rule 11
    /// wants one system: these are declared here rather than special-cased at
    /// the point of use.
    #[must_use]
    pub fn engine() -> Self {
        let mut actions = Self::default();
        for (id, description, default) in ENGINE_ACTIONS {
            actions.actions.push(Action {
                id: (*id).to_owned(),
                description: (*description).to_owned(),
                source: Source::Engine,
                default: *default,
            });
        }
        actions
    }

    /// Adds a mod's action.
    ///
    /// # Errors
    ///
    /// [`RegisterError::Reserved`] if the id is in the `engine:` namespace, and
    /// [`RegisterError::Duplicate`] if it is already registered.
    pub fn register(&mut self, action: Action) -> Result<(), RegisterError> {
        if matches!(action.source, Source::Mod(_)) && action.id.starts_with("engine:") {
            return Err(RegisterError::Reserved(action.id));
        }
        if self.get(&action.id).is_some() {
            return Err(RegisterError::Duplicate(action.id));
        }
        self.actions.push(action);
        Ok(())
    }

    /// Looks one up by id.
    #[must_use]
    pub fn get(&self, id: &str) -> Option<&Action> {
        self.actions.iter().find(|action| action.id == id)
    }

    /// Every action, in display order.
    pub fn iter(&self) -> impl Iterator<Item = &Action> {
        self.actions.iter()
    }

    /// How many there are.
    #[must_use]
    pub fn len(&self) -> usize {
        self.actions.len()
    }

    /// Whether there are none at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.actions.is_empty()
    }

    /// Actions grouped by who registered them, for the settings screen.
    ///
    /// **The attribution criterion**: every binding a player sees says which
    /// mod asked for it, and this is the one place that decides. Groups come
    /// out in first-appearance order, so the engine is first and mods follow in
    /// load order rather than alphabetically.
    #[must_use]
    pub fn by_source(&self) -> Vec<(&Source, Vec<&Action>)> {
        let mut groups: Vec<(&Source, Vec<&Action>)> = Vec::new();
        for action in &self.actions {
            if let Some(group) = groups
                .iter_mut()
                .find(|(source, _)| *source == &action.source)
            {
                group.1.push(action);
            } else {
                groups.push((&action.source, vec![action]));
            }
        }
        groups
    }

    /// Forgets every mod's actions, keeping the engine's.
    ///
    /// Called when leaving a server: the next one has its own mods, and an
    /// action from the last one is a binding nothing can trigger.
    pub fn clear_mods(&mut self) {
        self.actions
            .retain(|action| matches!(action.source, Source::Engine));
    }
}

/// What each action is bound to.
///
/// Absent means "the action's own default"; present means the player chose it.
/// Keeping the two apart is what lets the settings screen offer a reset and
/// what stops a default change in a later version being silently overridden by
/// a file that only ever recorded the old default.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct Bindings {
    /// Player-chosen bindings only, by action id.
    #[serde(default)]
    bound: BTreeMap<String, Input>,
}

impl Bindings {
    /// What this action is bound to: the player's choice, else its default.
    #[must_use]
    pub fn get(&self, actions: &Actions, id: &str) -> Option<Input> {
        if let Some(input) = self.bound.get(id) {
            return Some(*input);
        }
        actions.get(id).and_then(|action| action.default)
    }

    /// Binds an action, replacing whatever it had.
    pub fn bind(&mut self, id: &str, input: Input) {
        self.bound.insert(id.to_owned(), input);
    }

    /// Returns an action to its default.
    pub fn reset(&mut self, id: &str) {
        self.bound.remove(id);
    }

    /// Returns every action to its default.
    pub fn reset_all(&mut self) {
        self.bound.clear();
    }

    /// Whether this action has been bound by the player rather than defaulted.
    #[must_use]
    pub fn is_custom(&self, id: &str) -> bool {
        self.bound.contains_key(id)
    }

    /// Which action this input triggers, if any.
    ///
    /// The lookup the window does on every key event, so it walks the registry
    /// rather than keeping a reverse map that would need invalidating on every
    /// rebind. The registry is tens of entries, once per key press.
    ///
    /// **First match wins, in registry order**, which makes a conflict
    /// deterministic rather than arbitrary — see [`Bindings::conflicts`], which
    /// is how a player is told about one.
    #[must_use]
    pub fn action_for<'a>(&self, actions: &'a Actions, input: Input) -> Option<&'a Action> {
        actions
            .iter()
            .find(|action| self.get(actions, &action.id) == Some(input))
    }

    /// Reads a bindings file, or the defaults if there is not one yet.
    ///
    /// A missing file is not an error — a fresh install has no bindings and the
    /// defaults are exactly right. A file that exists and is malformed IS an
    /// error, for the same reason `Config::load_or_default` treats one that
    /// way: silently ignoring it leaves a player wondering why the keys they
    /// set do nothing.
    ///
    /// # Errors
    ///
    /// [`BindingsError`] for anything but the file being absent.
    pub fn load_or_default(path: &std::path::Path) -> Result<Self, BindingsError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(source) => {
                return Err(BindingsError::Read {
                    path: path.to_path_buf(),
                    source,
                });
            }
        };
        toml::from_str(&text).map_err(|source| BindingsError::Parse {
            path: path.to_path_buf(),
            source: Box::new(source),
        })
    }

    /// Writes the player's choices out.
    ///
    /// # Errors
    ///
    /// [`BindingsError`] if the file cannot be written or the map cannot be
    /// serialised.
    pub fn save(&self, path: &std::path::Path) -> Result<(), BindingsError> {
        let text = toml::to_string_pretty(self).map_err(|source| BindingsError::Write {
            path: path.to_path_buf(),
            source: Box::new(source),
        })?;
        std::fs::write(path, text).map_err(|source| BindingsError::Read {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Every input bound to more than one action.
    ///
    /// Reported rather than refused. A player mid-rebind is briefly in conflict
    /// by definition, and refusing the keystroke that causes it means they
    /// cannot swap two bindings without a scratch key.
    #[must_use]
    pub fn conflicts(&self, actions: &Actions) -> Vec<(Input, Vec<String>)> {
        let mut by_input: BTreeMap<Input, Vec<String>> = BTreeMap::new();
        for action in actions.iter() {
            if let Some(input) = self.get(actions, &action.id) {
                by_input.entry(input).or_default().push(action.id.clone());
            }
        }
        by_input
            .into_iter()
            .filter(|(_, ids)| ids.len() > 1)
            .collect()
    }
}

/// The engine's reserved actions: id, description, default.
///
/// Every one of these was a `KeyCode` arm in `main.rs` before Task 13. The
/// duplicate bindings there — a letter beside each function key — are kept as
/// the DEFAULT of a second action rather than as two keys for one action,
/// because a binding a player can see and change is worth more than a fallback
/// they cannot. The reason those fallbacks exist is still true: F-keys sit
/// under vendor overlays on many laptops and simply never arrive.
const ENGINE_ACTIONS: &[(&str, &str, Option<Input>)] = &[
    (
        "engine:move_forward",
        "Walk forward",
        Some(Input::Key(KeyCode::KeyW)),
    ),
    (
        "engine:move_back",
        "Walk back",
        Some(Input::Key(KeyCode::KeyS)),
    ),
    (
        "engine:move_left",
        "Strafe left",
        Some(Input::Key(KeyCode::KeyA)),
    ),
    (
        "engine:move_right",
        "Strafe right",
        Some(Input::Key(KeyCode::KeyD)),
    ),
    ("engine:jump", "Jump", Some(Input::Key(KeyCode::Space))),
    (
        "engine:sneak",
        "Sneak",
        Some(Input::Key(KeyCode::ShiftLeft)),
    ),
    // **No default any more**, because sprint took Left Control. It stays a
    // registered action so a bindings file naming it still resolves and so it
    // can be put back on a key by hand — removing it outright would silently
    // drop a binding somebody had chosen.
    ("engine:sneak_alt", "Sneak (alternate)", None),
    // **Left Control, not Right Shift.** Sprinting has existed since Task 09
    // and was reported from the window as missing, because nobody presses the
    // right-hand shift key: it is the one modifier a hand on WASD cannot reach.
    // Left Control is where every other game puts it, and Left Shift is
    // already sneak.
    (
        "engine:sprint",
        "Sprint",
        Some(Input::Key(KeyCode::ControlLeft)),
    ),
    // **Only does anything for an operator.** The server decides and says so
    // at join; for everybody else this key is inert rather than absent, because
    // an action that vanished from the controls list depending on who you are
    // would be a controls screen that changed shape between servers.
    (
        "engine:fly",
        "Fly (operators only)",
        Some(Input::Key(KeyCode::KeyN)),
    ),
    (
        "engine:dig",
        "Break what you are looking at",
        Some(Input::Mouse(MouseButton::Left)),
    ),
    (
        "engine:place",
        "Place what you are carrying",
        Some(Input::Mouse(MouseButton::Right)),
    ),
    (
        "engine:settings",
        "Controls and settings",
        Some(Input::Key(KeyCode::F1)),
    ),
    // **Engine, not a mod.** Moderation and RCON depend on chat existing
    // whatever mods a server runs, so the key that opens it is the engine's
    // and works with zero mods loaded.
    (
        "engine:chat",
        "Say something",
        Some(Input::Key(KeyCode::KeyT)),
    ),
    // **Charter rule 18's instrument, and it ships.** Not a developer-only
    // overlay: a player on hardware nobody here will ever own is the person
    // best placed to measure frame pacing, and they need a way to read it. Also
    // in the settings screen, because a key nobody discovers is a key nobody
    // presses.
    (
        "engine:debug_overlay",
        "Show the debug overlay",
        Some(Input::Key(KeyCode::F3)),
    ),
    (
        "engine:menu",
        "Release the cursor",
        Some(Input::Key(KeyCode::Escape)),
    ),
    (
        "engine:next_tool",
        "Cycle tool",
        Some(Input::Key(KeyCode::KeyR)),
    ),
    (
        "engine:offhand",
        "Swap what you are holding with the off-hand",
        Some(Input::Key(KeyCode::KeyF)),
    ),
    (
        "engine:hotbar_1",
        "Hotbar slot 1",
        Some(Input::Key(KeyCode::Digit1)),
    ),
    (
        "engine:hotbar_2",
        "Hotbar slot 2",
        Some(Input::Key(KeyCode::Digit2)),
    ),
    (
        "engine:hotbar_3",
        "Hotbar slot 3",
        Some(Input::Key(KeyCode::Digit3)),
    ),
    (
        "engine:hotbar_4",
        "Hotbar slot 4",
        Some(Input::Key(KeyCode::Digit4)),
    ),
    (
        "engine:hotbar_5",
        "Hotbar slot 5",
        Some(Input::Key(KeyCode::Digit5)),
    ),
    (
        "engine:hotbar_6",
        "Hotbar slot 6",
        Some(Input::Key(KeyCode::Digit6)),
    ),
    (
        "engine:hotbar_7",
        "Hotbar slot 7",
        Some(Input::Key(KeyCode::Digit7)),
    ),
    (
        "engine:hotbar_8",
        "Hotbar slot 8",
        Some(Input::Key(KeyCode::Digit8)),
    ),
    (
        "engine:hotbar_9",
        "Hotbar slot 9",
        Some(Input::Key(KeyCode::Digit9)),
    ),
    (
        "engine:lighting_mode",
        "Cycle lighting mode",
        Some(Input::Key(KeyCode::F5)),
    ),
    (
        "engine:lighting_mode_alt",
        "Cycle lighting mode (laptop)",
        Some(Input::Key(KeyCode::KeyL)),
    ),
    (
        "engine:shadow_quality",
        "Cycle shadow resolution",
        Some(Input::Key(KeyCode::KeyK)),
    ),
    (
        "engine:third_person",
        "Third person",
        Some(Input::Key(KeyCode::F6)),
    ),
    (
        "engine:third_person_alt",
        "Third person (laptop)",
        Some(Input::Key(KeyCode::KeyV)),
    ),
    (
        "engine:chunk_borders",
        "Show chunk borders",
        Some(Input::Key(KeyCode::KeyB)),
    ),
    (
        "engine:time_back",
        "Wind the sky back",
        Some(Input::Key(KeyCode::BracketLeft)),
    ),
    (
        "engine:time_back_alt",
        "Wind the sky back (laptop)",
        Some(Input::Key(KeyCode::PageDown)),
    ),
    (
        "engine:time_forward",
        "Wind the sky on",
        Some(Input::Key(KeyCode::BracketRight)),
    ),
    (
        "engine:time_forward_alt",
        "Wind the sky on (laptop)",
        Some(Input::Key(KeyCode::PageUp)),
    ),
    (
        "engine:time_resync",
        "Return the sky to the server's hour",
        Some(Input::Key(KeyCode::Backslash)),
    ),
    (
        "engine:time_resync_alt",
        "Return the sky to the server's hour (laptop)",
        Some(Input::Key(KeyCode::Home)),
    ),
    (
        "engine:teleport_far",
        "Debug: teleport to the far edge",
        Some(Input::Key(KeyCode::F8)),
    ),
    // **Moved off `T` when chat arrived.** `T` is what a player reaches for to
    // say something, and a debug laptop fallback is not what should own it.
    // Changing a DEFAULT is safe by design: the bindings file records only what
    // a player chose, so anybody who had rebound this keeps their key and
    // everybody else gets the better one (see `Bindings`).
    (
        "engine:teleport_far_alt",
        "Debug: teleport to the far edge (laptop)",
        Some(Input::Key(KeyCode::KeyY)),
    ),
    (
        "engine:teleport_home",
        "Debug: teleport home",
        Some(Input::Key(KeyCode::F7)),
    ),
    (
        "engine:teleport_home_alt",
        "Debug: teleport home (laptop)",
        Some(Input::Key(KeyCode::KeyH)),
    ),
    (
        "engine:material_row",
        "Debug: lay out one of every block",
        Some(Input::Key(KeyCode::KeyG)),
    ),
];

#[cfg(test)]
mod default_binding_tests {
    use super::*;

    /// Every engine action that ships with a key, as `(id, key)`.
    fn bound() -> Vec<(&'static str, Input)> {
        ENGINE_ACTIONS
            .iter()
            .filter_map(|(id, _, default)| default.map(|key| (*id, key)))
            .collect()
    }

    #[test]
    fn sprint_is_on_a_key_a_hand_on_wasd_can_reach() {
        // **Reported from the window as missing.** Sprinting has existed since
        // Task 09 and was bound to RIGHT shift — the one modifier a hand on
        // WASD cannot reach, so nobody ever pressed it.
        let key = bound()
            .into_iter()
            .find(|(id, _)| *id == "engine:sprint")
            .map(|(_, key)| key)
            .expect("sprint ships with a key");
        assert_ne!(
            key,
            Input::Key(KeyCode::ShiftRight),
            "sprint is back on the key nobody found"
        );
    }

    #[test]
    fn no_two_engine_actions_ship_on_one_key() {
        // Two actions on one default key is a control that silently does
        // something else — the clash `35f75c1` made the client say out loud,
        // asserted here so the shipped set never has one to warn about.
        let mut seen: std::collections::BTreeMap<Input, &str> = std::collections::BTreeMap::new();
        for (id, key) in bound() {
            if let Some(held) = seen.insert(key, id) {
                panic!("`{held}` and `{id}` both default to {key:?}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mod_action(id: &str, default: Option<Input>) -> Action {
        Action {
            id: id.to_owned(),
            description: "a mod's action".to_owned(),
            source: Source::Mod(id.split(':').next().unwrap_or("mod").to_owned()),
            default,
        }
    }

    #[test]
    fn a_rebind_moves_the_action_and_leaves_every_other_one_alone() {
        // **The settings flow, without the settings screen.** The task asks for
        // the binding model to be factored so the rebinding path is testable
        // headlessly, and this is that test: what the screen does is call these
        // three methods and read the list back.
        let mut actions = Actions::engine();
        actions
            .register(mod_action(
                "core_tools:chisel_mode",
                Some(Input::Key(KeyCode::KeyC)),
            ))
            .expect("register");
        let mut bindings = Bindings::default();

        // Rebind the mod's action onto a key the engine already uses. Allowed:
        // a conflict is reported, not refused, or a player could never swap two
        // bindings without a scratch key.
        bindings.bind("core_tools:chisel_mode", Input::Key(KeyCode::KeyW));
        let conflicts = bindings.conflicts(&actions);
        assert_eq!(conflicts.len(), 1, "{conflicts:?}");

        // Move it somewhere free, and the clash goes with it.
        bindings.bind("core_tools:chisel_mode", Input::Key(KeyCode::KeyM));
        assert!(bindings.conflicts(&actions).is_empty());
        assert_eq!(
            bindings
                .action_for(&actions, Input::Key(KeyCode::KeyM))
                .map(|action| action.id.as_str()),
            Some("core_tools:chisel_mode")
        );
        // And walking forward is untouched throughout, which is the property a
        // player actually cares about when they rebind one thing.
        assert_eq!(
            bindings.get(&actions, "engine:move_forward"),
            Some(Input::Key(KeyCode::KeyW))
        );

        // Reset-all returns everything, including the mod's, to its default.
        bindings.reset_all();
        assert_eq!(
            bindings.get(&actions, "core_tools:chisel_mode"),
            Some(Input::Key(KeyCode::KeyC))
        );
        assert!(!bindings.is_custom("core_tools:chisel_mode"));
    }

    #[test]
    fn a_binding_file_survives_the_mod_that_asked_for_it_going_away() {
        // **Why bindings key on the ACTION.** A player rebinds a mod's control,
        // then joins a server without that mod. The entry is one nobody looks
        // up rather than one that renumbers everything after it, and the
        // engine's own bindings are undisturbed.
        let mut bindings = Bindings::default();
        bindings.bind("core_tools:chisel_mode", Input::Key(KeyCode::KeyM));
        bindings.bind("engine:jump", Input::Key(KeyCode::KeyJ));
        let written = toml::to_string(&bindings).expect("serialise");

        // A server with no mods at all.
        let actions = Actions::engine();
        let read: Bindings = toml::from_str(&written).expect("parse");
        assert_eq!(
            read.get(&actions, "engine:jump"),
            Some(Input::Key(KeyCode::KeyJ)),
            "an engine binding was disturbed by a mod going away"
        );
        assert!(
            read.action_for(&actions, Input::Key(KeyCode::KeyM))
                .is_none(),
            "a key bound to an absent mod's action still triggers something"
        );
        // And the entry is still there for when that server is joined again.
        assert!(read.is_custom("core_tools:chisel_mode"));
    }

    #[test]
    fn a_mods_key_name_becomes_a_key_and_a_strange_one_does_not() {
        // **The one place a string from a server becomes a `KeyCode`.** Charter
        // rule 3 keeps winit out of `crates/core`, so the mod API and the
        // protocol carry the name and this is where it lands.
        assert_eq!(parse_key("KeyF"), Some(Input::Key(KeyCode::KeyF)));
        assert_eq!(parse_key("Space"), Some(Input::Key(KeyCode::Space)));

        // Empty means "the mod shipped it unbound", not "the mod is broken".
        assert_eq!(parse_key(""), None);

        // A name this build does not know is a mod written against a newer
        // winit: the action still works the moment a player binds it, so this
        // is a warning at the call site rather than a refused join.
        assert_eq!(parse_key("KeyThatDoesNotExist"), None);

        // And the reason this goes through serde rather than a format string:
        // the name comes from a server, so it is hostile input. None of these
        // may panic, escape into a document, or be mistaken for a real key.
        for hostile in ["\"", "\\", "a\nb", "{}", "[]", "\0", "KeyW\"]"] {
            assert_eq!(parse_key(hostile), None, "accepted {hostile:?}");
        }
    }

    #[test]
    fn a_binding_file_round_trips_through_toml() {
        // **The format is the promise.** A player's bindings survive a restart
        // or they do not, and the only way that is true is if what is written
        // parses back to the same thing. Both arms of `Input` are in here
        // because the mouse one has a different shape on the wire.
        let mut bindings = Bindings::default();
        bindings.bind("engine:jump", Input::Key(KeyCode::KeyJ));
        bindings.bind("engine:dig", Input::Mouse(MouseButton::Middle));

        let written = toml::to_string(&bindings).expect("serialise");
        let read: Bindings = toml::from_str(&written).expect("parse");

        let actions = Actions::engine();
        assert_eq!(
            read.get(&actions, "engine:jump"),
            Some(Input::Key(KeyCode::KeyJ)),
            "a rebound key did not survive the round trip: {written}"
        );
        assert_eq!(
            read.get(&actions, "engine:dig"),
            Some(Input::Mouse(MouseButton::Middle)),
            "a rebound mouse button did not survive the round trip: {written}"
        );
        // The key's name in the file is `KeyCode`'s own, which is what a mod
        // writes in `default_key` and what the settings screen shows.
        assert!(
            written.contains("KeyJ"),
            "the file does not name the key in the form a mod would write: {written}"
        );
    }

    #[test]
    fn a_file_records_choices_and_not_defaults() {
        // **Absent means "the default", and that is not the same as recording
        // what the default currently is.** If the file stored every binding,
        // improving a default in a later version would be silently overridden
        // for everyone who had ever launched the game.
        let actions = Actions::engine();
        let mut bindings = Bindings::default();
        let untouched = toml::to_string(&bindings).expect("serialise");
        assert!(
            !untouched.contains("engine:"),
            "an untouched config wrote bindings nobody chose: {untouched}"
        );

        bindings.bind("engine:jump", Input::Key(KeyCode::KeyJ));
        assert!(bindings.is_custom("engine:jump"));
        assert!(!bindings.is_custom("engine:move_forward"));

        bindings.reset("engine:jump");
        assert_eq!(
            bindings.get(&actions, "engine:jump"),
            Some(Input::Key(KeyCode::Space)),
            "resetting did not return the action to its default"
        );
    }

    #[test]
    fn an_input_finds_the_action_it_was_bound_to() {
        let mut actions = Actions::engine();
        actions
            .register(mod_action(
                "core_tools:chisel_mode",
                Some(Input::Key(KeyCode::KeyC)),
            ))
            .expect("register");
        let bindings = Bindings::default();

        let found = bindings
            .action_for(&actions, Input::Key(KeyCode::KeyC))
            .expect("the mod's default is reachable");
        assert_eq!(found.id, "core_tools:chisel_mode");

        // And after a rebind it follows the action rather than the key.
        let mut bindings = bindings;
        bindings.bind("core_tools:chisel_mode", Input::Key(KeyCode::KeyM));
        assert_eq!(
            bindings
                .action_for(&actions, Input::Key(KeyCode::KeyM))
                .map(|action| action.id.as_str()),
            Some("core_tools:chisel_mode")
        );
        assert!(
            bindings
                .action_for(&actions, Input::Key(KeyCode::KeyC))
                .is_none(),
            "the old key still triggers the action it was rebound away from"
        );
    }

    #[test]
    fn a_conflict_is_reported_and_not_refused() {
        // Reported rather than refused, because a player swapping two bindings
        // passes through a conflict on the way and refusing the keystroke that
        // causes it means they need a scratch key to get anywhere.
        let actions = Actions::engine();
        let mut bindings = Bindings::default();
        assert!(
            bindings.conflicts(&actions).is_empty(),
            "the engine's own defaults conflict with each other"
        );

        bindings.bind("engine:jump", Input::Key(KeyCode::KeyW));
        let conflicts = bindings.conflicts(&actions);
        assert_eq!(
            conflicts.len(),
            1,
            "the clash was not reported: {conflicts:?}"
        );
        let (input, ids) = &conflicts[0];
        assert_eq!(*input, Input::Key(KeyCode::KeyW));
        assert!(ids.contains(&"engine:jump".to_owned()));
        assert!(ids.contains(&"engine:move_forward".to_owned()));
    }

    #[test]
    fn the_engines_own_defaults_do_not_clash() {
        // The list in `ENGINE_ACTIONS` is written by hand and is long enough
        // that two entries can quietly claim the same key. That would make one
        // of them dead on arrival, and the symptom is a control that does
        // nothing rather than an error.
        let actions = Actions::engine();
        let conflicts = Bindings::default().conflicts(&actions);
        assert!(
            conflicts.is_empty(),
            "two engine actions share a default binding: {conflicts:?}"
        );
    }

    #[test]
    fn a_mod_may_not_register_in_the_engines_namespace() {
        let mut actions = Actions::engine();
        let err = actions
            .register(Action {
                id: "engine:jump".to_owned(),
                description: "mine now".to_owned(),
                source: Source::Mod("greedy".to_owned()),
                default: None,
            })
            .expect_err("a mod claimed an engine id");
        assert_eq!(err, RegisterError::Reserved("engine:jump".to_owned()));
    }

    #[test]
    fn the_same_action_cannot_be_registered_twice() {
        let mut actions = Actions::engine();
        actions
            .register(mod_action("core_tools:chisel_mode", None))
            .expect("first");
        let err = actions
            .register(mod_action("core_tools:chisel_mode", None))
            .expect_err("the duplicate was accepted");
        assert_eq!(
            err,
            RegisterError::Duplicate("core_tools:chisel_mode".to_owned())
        );
    }

    #[test]
    fn every_action_is_attributable_to_who_registered_it() {
        // **The attribution criterion.** Every binding the settings screen shows
        // names the mod that asked for it, and the engine's own are a group like
        // any other rather than an unlabelled remainder.
        let mut actions = Actions::engine();
        actions
            .register(mod_action("core_tools:chisel_mode", None))
            .expect("register");
        actions
            .register(mod_action("core_milk:pour", None))
            .expect("register");

        let groups = actions.by_source();
        assert_eq!(
            groups.first().map(|(source, _)| source.label()),
            Some("engine"),
            "the engine's actions are not the first group"
        );
        let labels: Vec<&str> = groups.iter().map(|(source, _)| source.label()).collect();
        assert_eq!(labels, vec!["engine", "core_tools", "core_milk"]);
        assert!(
            actions
                .iter()
                .all(|action| !action.source.label().is_empty()),
            "an action came out with nobody to attribute it to"
        );
    }

    #[test]
    fn leaving_a_server_forgets_its_mods_actions_and_keeps_the_engines() {
        let mut actions = Actions::engine();
        let engine_count = actions.len();
        actions
            .register(mod_action("core_tools:chisel_mode", None))
            .expect("register");

        actions.clear_mods();
        assert_eq!(actions.len(), engine_count);
        assert!(actions.get("core_tools:chisel_mode").is_none());
        assert!(actions.get("engine:jump").is_some());
    }
}
