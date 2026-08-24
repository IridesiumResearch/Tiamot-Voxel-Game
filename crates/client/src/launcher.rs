// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The front screen's model: which worlds exist and which mods are on.
//!
//! # Why this is a module and not part of `main.rs`
//!
//! Everything the front screen decides is a question about files: which worlds
//! this player has, which mods are installed, which of them are ticked, and
//! whether joining a world with the current selection is going to surprise
//! them. None of that needs a window, and all of it is worth testing — so it
//! lives here, and the egui that draws it is a separate concern that reads and
//! writes this state.
//!
//! # The mod-set question
//!
//! A world is generated and then extended by whatever mods were loaded when it
//! ran. Charter rule 8 makes that survivable — string ids are canonical and an
//! unregistered id becomes `engine:unknown` and round-trips byte for byte — so
//! joining a world with a different mod set is *safe*, not corrupting. It is
//! still a surprise, which is why [`Library::mismatch`] exists: the player is
//! told what changed and decides.
//!
//! For a server the player joins, none of this applies: the **server owner
//! decides which mods are on**, and the client is told. The selection here is
//! for worlds this machine runs.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Where a world lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A world on this machine, run by an embedded server.
    Local {
        /// Its directory, relative to the client's data directory.
        path: PathBuf,
    },
    /// Somebody else's server.
    Remote {
        /// Host and port, as typed.
        address: String,
    },
}

/// One line of the world list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// What the player calls it.
    pub name: String,
    /// Local or remote.
    pub kind: Kind,
    /// For a local world, the mods it was last played with.
    ///
    /// Recorded so a player who turns a mod off and reopens an old world is
    /// told. Empty for a remote server, where the mod set is not this
    /// machine's to choose.
    #[serde(default)]
    pub mods: Vec<String>,
}

impl Entry {
    /// Whether this is a world this machine runs.
    #[must_use]
    pub const fn is_local(&self) -> bool {
        matches!(self.kind, Kind::Local { .. })
    }
}

/// The player's worlds and servers, in the order they were added.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Library {
    /// Every world and server, newest last.
    #[serde(default, rename = "world")]
    pub entries: Vec<Entry>,
}

/// What changed between a world's mod set and the one that is ticked now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mismatch {
    /// Mods that were on and are now off.
    pub missing: Vec<String>,
    /// Mods that are on now and were not before.
    pub added: Vec<String>,
}

impl Library {
    /// Reads the list, or an empty one if there is no file yet.
    ///
    /// **A missing file is not an error** — it is what a first run looks like.
    /// A malformed one is reported, because silently replacing a player's world
    /// list with an empty one is how a list gets lost.
    ///
    /// # Errors
    ///
    /// The parse error, with the path, if the file exists and is not valid.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(format!("could not read `{}`: {err}", path.display())),
        };
        toml::from_str(&text).map_err(|err| format!("`{}` is not valid: {err}", path.display()))
    }

    /// Writes the list, creating the directory if it is not there.
    ///
    /// # Errors
    ///
    /// The write error, with the path.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("could not create `{}`: {err}", parent.display()))?;
        }
        let text = toml::to_string_pretty(self)
            .map_err(|err| format!("could not write the world list: {err}"))?;
        std::fs::write(path, text)
            .map_err(|err| format!("could not write `{}`: {err}", path.display()))
    }

    /// Adds a world, replacing any entry with the same name.
    ///
    /// Replacing rather than appending: two worlds with one name are two lines
    /// a player cannot tell apart, and the newer one is the one they meant.
    pub fn add(&mut self, entry: Entry) {
        self.entries.retain(|existing| existing.name != entry.name);
        self.entries.push(entry);
    }

    /// Removes a world from the list. Does not touch its files.
    pub fn forget(&mut self, name: &str) {
        self.entries.retain(|entry| entry.name != name);
    }

    /// A name nothing in the list is using, derived from `wanted`.
    ///
    /// `New World`, then `New World 2`, and so on. A player who makes three
    /// worlds without naming them gets three worlds rather than one world
    /// overwritten twice.
    #[must_use]
    pub fn unused_name(&self, wanted: &str) -> String {
        let taken: BTreeSet<&str> = self
            .entries
            .iter()
            .map(|entry| entry.name.as_str())
            .collect();
        if !taken.contains(wanted) {
            return wanted.to_owned();
        }
        // Bounded by how many entries there are plus one: with `n` names taken,
        // one of the first `n + 1` candidates must be free. An unbounded search
        // would be an infinite loop if the set ever stopped being finite.
        (2..=self.entries.len() + 2)
            .map(|suffix| format!("{wanted} {suffix}"))
            .find(|name| !taken.contains(name.as_str()))
            .unwrap_or_else(|| wanted.to_owned())
    }

    /// How the ticked mods differ from what a world was last played with.
    ///
    /// `None` when they match, or when the entry is a server — a server's mod
    /// set is the owner's decision and nothing here can disagree with it.
    #[must_use]
    pub fn mismatch(entry: &Entry, enabled: &[String]) -> Option<Mismatch> {
        if !entry.is_local() {
            return None;
        }
        let before: BTreeSet<&str> = entry.mods.iter().map(String::as_str).collect();
        let now: BTreeSet<&str> = enabled.iter().map(String::as_str).collect();
        let missing: Vec<String> = before.difference(&now).map(|id| (*id).to_owned()).collect();
        let added: Vec<String> = now.difference(&before).map(|id| (*id).to_owned()).collect();
        if missing.is_empty() && added.is_empty() {
            None
        } else {
            Some(Mismatch { missing, added })
        }
    }
}

/// One installed mod, and whether it is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listing {
    /// Its namespace, and what the enabled set records.
    pub id: String,
    /// Its display name.
    pub name: String,
    /// Its one-line description.
    pub description: String,
    /// Whether it loads.
    pub enabled: bool,
}

/// Every mod installed on this machine, and which are ticked.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Catalogue {
    /// Installed mods, sorted by id.
    pub mods: Vec<Listing>,
    /// Why the scan came back short, if it did.
    ///
    /// **A broken mod must be visible.** `scan_directory` refuses the whole
    /// directory when any manifest in it is invalid — deliberately, because
    /// silently ignoring a mod somebody installed is worse than refusing — and
    /// a launcher cannot refuse to start over it. So the list comes back empty
    /// and this says why, for the screen to show.
    pub problem: Option<String>,
}

/// What was saved about which mods are on.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Enabled {
    #[serde(default)]
    disabled: Vec<String>,
}

impl Catalogue {
    /// Scans a directory of mods and applies the saved selection.
    ///
    /// **Everything is on unless it was explicitly turned off**, which is why
    /// the file records what is DISABLED. A player who installs a new mod
    /// expects it to be there; recording the enabled set instead would leave
    /// every new mod silently off until they noticed.
    ///
    /// A directory that cannot be scanned gives an empty catalogue rather than
    /// an error: a client with no `game/` beside it is a client that only ever
    /// joins other people's servers, which is a perfectly ordinary way to run
    /// one.
    #[must_use]
    pub fn scan(mods_dir: &Path, selection: &Path) -> Self {
        let off: BTreeSet<String> = std::fs::read_to_string(selection)
            .ok()
            .and_then(|text| toml::from_str::<Enabled>(&text).ok())
            .map(|saved| saved.disabled.into_iter().collect())
            .unwrap_or_default();
        let (found, problem) = match tiamot_core::modload::scan_directory(mods_dir) {
            Ok(found) => (found, None),
            // A directory that is not there at all is not a problem worth
            // reporting: a client that only ever joins other people's servers
            // has no `game/` beside it, and that is an ordinary way to run one.
            Err(_) if !mods_dir.is_dir() => (Vec::new(), None),
            Err(err) => (Vec::new(), Some(err.to_string())),
        };
        Self {
            problem,
            mods: found
                .into_iter()
                .map(|discovered| Listing {
                    enabled: !off.contains(&discovered.manifest.id),
                    id: discovered.manifest.id,
                    name: discovered.manifest.name,
                    description: discovered.manifest.description,
                })
                .collect(),
        }
    }

    /// Writes which mods are off.
    ///
    /// # Errors
    ///
    /// The write error, with the path.
    pub fn save(&self, selection: &Path) -> Result<(), String> {
        if let Some(parent) = selection.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|err| format!("could not create `{}`: {err}", parent.display()))?;
        }
        let off = Enabled {
            disabled: self
                .mods
                .iter()
                .filter(|listing| !listing.enabled)
                .map(|listing| listing.id.clone())
                .collect(),
        };
        let text = toml::to_string_pretty(&off)
            .map_err(|err| format!("could not write the mod selection: {err}"))?;
        std::fs::write(selection, text)
            .map_err(|err| format!("could not write `{}`: {err}", selection.display()))
    }

    /// The ids that are on, sorted.
    #[must_use]
    pub fn enabled(&self) -> Vec<String> {
        self.mods
            .iter()
            .filter(|listing| listing.enabled)
            .map(|listing| listing.id.clone())
            .collect()
    }

    /// Turns one mod on or off.
    pub fn set(&mut self, id: &str, on: bool) {
        for listing in &mut self.mods {
            if listing.id == id {
                listing.enabled = on;
            }
        }
    }

    /// Turns everything on or off, which is what the box at the top does.
    pub fn set_all(&mut self, on: bool) {
        for listing in &mut self.mods {
            listing.enabled = on;
        }
    }

    /// Whether every installed mod is on.
    #[must_use]
    pub fn all_on(&self) -> bool {
        self.mods.iter().all(|listing| listing.enabled)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("tiamot-launcher-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        dir
    }

    fn local(name: &str, mods: &[&str]) -> Entry {
        Entry {
            name: name.to_owned(),
            kind: Kind::Local {
                path: PathBuf::from("worlds").join(name),
            },
            mods: mods.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    /// Writes a mod directory with a manifest and nothing else.
    fn write_mod(root: &Path, id: &str) {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).expect("mod dir");
        // A manifest with no `init.lua` beside it is not a mod, and
        // `scan_directory` refuses the whole directory over it.
        std::fs::write(dir.join("init.lua"), "-- nothing\n").expect("entry");
        std::fs::write(
            dir.join("mod.toml"),
            format!(
                "id = \"{id}\"\nname = \"{id} name\"\nversion = \"0.1.0\"\n\
                 license = \"GPL-3.0-only\"\ndescription = \"about {id}\"\n"
            ),
        )
        .expect("manifest");
    }

    #[test]
    fn a_missing_world_list_is_a_first_run_and_not_a_failure() {
        let dir = scratch("first-run");
        let library = Library::load(&dir.join("worlds.toml")).expect("a missing file is empty");
        assert!(library.entries.is_empty());
    }

    #[test]
    fn a_malformed_world_list_is_reported_rather_than_replaced() {
        // **The alternative loses the player's worlds.** A parse failure that
        // fell back to an empty list would be saved over the real one the next
        // time anything was added.
        let dir = scratch("malformed");
        let path = dir.join("worlds.toml");
        std::fs::write(&path, "this is not toml [[[").expect("write");
        assert!(Library::load(&path).is_err());
    }

    #[test]
    fn the_world_list_survives_a_round_trip() {
        let dir = scratch("round-trip");
        let path = dir.join("worlds.toml");
        let mut library = Library::default();
        library.add(local("Home", &["core_blocks", "core_ui"]));
        library.add(Entry {
            name: "Friend's server".to_owned(),
            kind: Kind::Remote {
                address: "example.com:4433".to_owned(),
            },
            mods: Vec::new(),
        });
        library.save(&path).expect("save");
        assert_eq!(Library::load(&path).expect("load"), library);
    }

    #[test]
    fn one_name_is_one_world() {
        let mut library = Library::default();
        library.add(local("Home", &[]));
        library.add(local("Home", &["core_ui"]));
        assert_eq!(
            library.entries.len(),
            1,
            "two lines a player cannot tell apart"
        );
        assert_eq!(library.entries[0].mods, vec!["core_ui".to_owned()]);
    }

    #[test]
    fn a_new_world_never_overwrites_the_last_one() {
        let mut library = Library::default();
        assert_eq!(library.unused_name("New World"), "New World");
        library.add(local("New World", &[]));
        assert_eq!(library.unused_name("New World"), "New World 2");
        library.add(local("New World 2", &[]));
        assert_eq!(library.unused_name("New World"), "New World 3");
    }

    #[test]
    fn turning_a_mod_off_is_a_difference_a_player_is_warned_about() {
        let world = local("Home", &["core_blocks", "core_ui"]);
        assert_eq!(
            Library::mismatch(&world, &["core_blocks".to_owned(), "core_ui".to_owned()]),
            None,
            "the same set is not a warning"
        );
        let changed = Library::mismatch(&world, &["core_blocks".to_owned(), "core_sky".to_owned()])
            .expect("a difference");
        assert_eq!(changed.missing, vec!["core_ui".to_owned()]);
        assert_eq!(changed.added, vec!["core_sky".to_owned()]);
    }

    #[test]
    fn a_servers_mod_set_is_not_this_machines_to_disagree_with() {
        // Charter: the server owner decides which mods are on. A warning about
        // the local selection would be about nothing.
        let server = Entry {
            name: "Somebody's".to_owned(),
            kind: Kind::Remote {
                address: "example.com:4433".to_owned(),
            },
            mods: Vec::new(),
        };
        assert_eq!(Library::mismatch(&server, &["core_ui".to_owned()]), None);
    }

    #[test]
    fn a_newly_installed_mod_is_on_without_being_asked_about() {
        // **Why the file records what is DISABLED.** Recording the enabled set
        // would leave every mod installed after it was written silently off.
        let dir = scratch("new-mod");
        let mods = dir.join("game");
        write_mod(&mods, "core_blocks");
        let selection = dir.join("mods.toml");

        let mut catalogue = Catalogue::scan(&mods, &selection);
        catalogue.set("core_blocks", false);
        catalogue.save(&selection).expect("save");

        write_mod(&mods, "core_ui");
        let reloaded = Catalogue::scan(&mods, &selection);
        assert_eq!(reloaded.enabled(), vec!["core_ui".to_owned()]);
    }

    #[test]
    fn the_box_at_the_top_turns_everything_on_and_off() {
        let dir = scratch("all");
        let mods = dir.join("game");
        write_mod(&mods, "core_blocks");
        write_mod(&mods, "core_ui");
        let mut catalogue = Catalogue::scan(&mods, &dir.join("mods.toml"));
        assert!(catalogue.all_on(), "everything starts on");

        catalogue.set_all(false);
        assert!(catalogue.enabled().is_empty());
        assert!(!catalogue.all_on());
        catalogue.set_all(true);
        assert_eq!(catalogue.enabled().len(), 2);
    }

    #[test]
    fn a_broken_mod_is_said_out_loud_rather_than_quietly_dropped() {
        // `scan_directory` refuses the whole directory when one manifest in it
        // is bad. A launcher cannot refuse to start over that, so it has to
        // SAY so — an empty mod list with no explanation is how somebody
        // spends an evening wondering where their mods went.
        let dir = scratch("broken");
        let mods = dir.join("game");
        write_mod(&mods, "core_blocks");
        std::fs::create_dir_all(mods.join("broken")).expect("dir");
        std::fs::write(mods.join("broken/mod.toml"), "id = \"Not An Id\"\n").expect("manifest");

        let catalogue = Catalogue::scan(&mods, &dir.join("mods.toml"));
        assert!(
            catalogue.problem.is_some(),
            "a directory that would not scan came back looking empty and fine"
        );
    }

    #[test]
    fn a_client_with_no_mods_beside_it_still_runs() {
        // Joining other people's servers is a perfectly ordinary way to run a
        // client, and it must not be an error to have nothing installed.
        let dir = scratch("no-mods");
        let catalogue = Catalogue::scan(&dir.join("nowhere"), &dir.join("mods.toml"));
        assert!(catalogue.mods.is_empty());
        assert!(
            catalogue.problem.is_none(),
            "having no mods installed is not a fault to report"
        );
        assert!(catalogue.all_on(), "nothing is off when there is nothing");
    }
}
