// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Domains: more than one simulation space in one world.
//!
//! A world is a set of named domains, each with its own coordinate frame, its
//! own chunk store, and its own scale. An entity is in exactly one of them.
//! The engine provides the mechanism for moving between them; **mods decide
//! when that happens** (charter rule 1). Nothing here knows what a domain
//! represents — there are no altitude thresholds, no travel rules, and no named
//! domains beyond [`OVERWORLD`].
//!
//! # Registered domains and instanced ones
//!
//! [`Registry::register`] runs in the registration window, which is right for a
//! fixed set — an overworld, a space domain, the bodies in a star catalogue —
//! and wrong for anything a player makes. A ship whose interior is its own
//! domain is the case that breaks it: the fifty-first ship somebody welds
//! together needs a fifty-first domain, and no mod could have named it while
//! the registry was open.
//!
//! So a registration with [`Spec::instanced`] set declares a TEMPLATE rather
//! than a domain. No domain exists under that id and nothing is stored for it;
//! instances are made at runtime by [`Registry::create`] and are named
//! `template/key`.
//!
//! # Unknown domains are preserved, never dropped
//!
//! A world can contain a domain no mod registers any more — a mod removed, an
//! instance whose template is gone. Its chunks are left exactly where they are
//! and it stays in the list, the same rule charter rule 8 applies to an
//! unregistered material: **data a mod cannot currently name is still that
//! player's data.** Re-registering the mod restores access to it unchanged.

use std::collections::{BTreeMap, BTreeSet};

/// The domain every world has, and the one old worlds are entirely made of.
///
/// **Deliberately not namespaced**, alone among the engine's string ids. It is
/// the value `persist::schema`'s `domain` column has defaulted to since Task
/// 03, so every chunk and entity ever written already says `overworld`; giving
/// it a namespace now would rename the contents of every existing save, which
/// is a migration bought for nothing but tidiness.
pub const OVERWORLD: &str = "overworld";

/// What separates a template from an instance's key in an instance's id.
///
/// A character no registered id may contain, so `ship/17` cannot collide with
/// anything a mod registered and an instance is recognisable as one by looking
/// at it.
pub const INSTANCE_SEPARATOR: char = '/';

/// The longest an instance key may be.
///
/// Instance ids are written into the `domain` column of every chunk row an
/// instance owns, and the key half is chosen by a mod at runtime — so it is
/// bounded here rather than wherever it is first stored.
pub const MAX_KEY: usize = 64;

/// What a domain is made of.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    /// A normal chunked voxel space with its own worldgen.
    Voxel,
    /// Entities only, with no voxels at all.
    ///
    /// A chunk write to one is an error rather than a silent no-op: a mod
    /// building in a space that cannot hold blocks has misunderstood something,
    /// and finding out at the write is far cheaper than finding out when the
    /// building is not there.
    Sparse,
}

/// A domain, or a template for domains, as a mod declared it.
#[derive(Debug, Clone, PartialEq)]
pub struct Spec {
    /// What it is made of.
    pub kind: Kind,
    /// How big one of its units is against the overworld's.
    ///
    /// Carried and never interpreted: the engine does no conversion between
    /// frames, because the two frames never talk. It is here so a mod that
    /// wants to draw a map or convert a velocity has one place to read it from
    /// rather than keeping its own table.
    pub scale: f64,
    /// Whether this declares a template rather than a domain.
    ///
    /// A template has no domain of its own and no storage. See the module
    /// documentation for why the registration window cannot name every domain
    /// a world will need.
    pub instanced: bool,
}

impl Default for Spec {
    fn default() -> Self {
        Self {
            kind: Kind::Voxel,
            scale: 1.0,
            instanced: false,
        }
    }
}

/// Why a domain could not be registered, created, or destroyed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DomainError {
    /// A second registration under an id already taken.
    #[error(
        "domain `{id}` is already registered; a second registration would silently replace the first"
    )]
    Duplicate {
        /// The id in question.
        id: String,
    },

    /// Registration was attempted after the registration window closed.
    #[error("domain `{id}` was registered after the registries froze (charter rule 9)")]
    Frozen {
        /// The id in question.
        id: String,
    },

    /// An id that cannot be stored or told apart from an instance's.
    #[error("`{id}` is not a usable domain id: {reason}")]
    BadId {
        /// The id in question.
        id: String,
        /// What is wrong with it.
        reason: &'static str,
    },

    /// An instance was asked for from something that is not a template.
    #[error("`{id}` is not an instanced template, so it has no instances to create")]
    NotATemplate {
        /// The id in question.
        id: String,
    },

    /// A template was addressed as though it were a domain.
    #[error("`{id}` is a template rather than a domain; create an instance of it instead")]
    IsATemplate {
        /// The id in question.
        id: String,
    },

    /// A domain nothing has registered and nothing has created.
    #[error("there is no domain `{id}`")]
    NoSuchDomain {
        /// The id in question.
        id: String,
    },

    /// Destroying a domain somebody is standing in.
    #[error(
        "domain `{id}` still holds {occupants} thing(s); taking a room out from under \
         somebody is the same defect as breaking a container they have open"
    )]
    Occupied {
        /// The id in question.
        id: String,
        /// How many entities and players are inside.
        occupants: usize,
    },

    /// Destroying something that was never an instance.
    #[error("`{id}` was registered rather than created, so it cannot be destroyed")]
    NotAnInstance {
        /// The id in question.
        id: String,
    },
}

/// Every domain this world has, and every template it can make more from.
///
/// Registration happens during the registration window and then freezes
/// (charter rule 9). Instances are created and destroyed at runtime, which is
/// the one thing that keeps changing after the freeze — see the module
/// documentation for why.
#[derive(Debug)]
pub struct Registry {
    /// What each registered id declared. Templates included.
    specs: BTreeMap<String, Spec>,
    /// Instances that exist, and which template each came from.
    ///
    /// Persisted, so a ship somebody built is still there next week.
    instances: BTreeMap<String, String>,
    /// Domains found in the world that nothing currently registers.
    ///
    /// Kept so their chunks are never dropped and re-registering a mod gives
    /// them back. See the module documentation.
    unknown: BTreeSet<String>,
    /// Whether the registration window has closed.
    frozen: bool,
}

impl Default for Registry {
    /// The same as [`Registry::new`].
    ///
    /// Written out rather than derived: a derived `Default` would produce a
    /// registry with **no overworld in it**, which is not a world — every
    /// chunk ever written names a domain, and a registry that does not know
    /// the one they all name would call the whole save unknown.
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    /// A registry with only the overworld in it.
    #[must_use]
    pub fn new() -> Self {
        let mut specs = BTreeMap::new();
        specs.insert(OVERWORLD.to_owned(), Spec::default());
        Self {
            specs,
            instances: BTreeMap::new(),
            unknown: BTreeSet::new(),
            frozen: false,
        }
    }

    /// Registers a domain or a template.
    ///
    /// # Errors
    ///
    /// [`DomainError::Frozen`] after the registration window,
    /// [`DomainError::Duplicate`] for an id already taken, and
    /// [`DomainError::BadId`] for one that cannot be stored.
    pub fn register(&mut self, id: &str, spec: Spec) -> Result<(), DomainError> {
        if self.frozen {
            return Err(DomainError::Frozen { id: id.to_owned() });
        }
        check_registered_id(id)?;
        if self.specs.contains_key(id) {
            return Err(DomainError::Duplicate { id: id.to_owned() });
        }
        self.specs.insert(id.to_owned(), spec);
        // A domain the world already had and nothing could name is a domain
        // again. Its chunks were never touched, so this is the whole of
        // "re-registering it restores access".
        self.unknown.remove(id);
        Ok(())
    }

    /// Closes the registration window.
    pub const fn freeze(&mut self) {
        self.frozen = true;
    }

    /// Whether the registration window has closed.
    #[must_use]
    pub const fn frozen(&self) -> bool {
        self.frozen
    }

    /// Notes a domain the world contains that nothing registered.
    ///
    /// Called for every distinct domain found in the database at load. A domain
    /// that IS registered is not unknown, and one that is a live instance is
    /// not either — this is only for what is left over.
    pub fn note_unknown(&mut self, id: &str) {
        if !self.specs.contains_key(id) && !self.instances.contains_key(id) {
            self.unknown.insert(id.to_owned());
        }
    }

    /// Domains the world holds that nothing currently registers.
    #[must_use]
    pub fn unknown(&self) -> Vec<&str> {
        self.unknown.iter().map(String::as_str).collect()
    }

    /// The spec of a domain, if it has one.
    ///
    /// An instance answers with its template's spec, because an instance IS its
    /// template in everything but identity — which is what makes a runtime
    /// domain indistinguishable from a registered one downstream.
    #[must_use]
    pub fn spec(&self, id: &str) -> Option<&Spec> {
        if let Some(spec) = self.specs.get(id) {
            return (!spec.instanced).then_some(spec);
        }
        let template = self.instances.get(id)?;
        self.specs.get(template)
    }

    /// Whether a domain exists to be simulated, stored, or transferred into.
    ///
    /// An unknown domain counts: its chunks are real and a player standing in
    /// one when its mod was removed is really there. What it lacks is a
    /// generator, which is [`Self::spec`]'s answer being `None`.
    #[must_use]
    pub fn exists(&self, id: &str) -> bool {
        self.spec(id).is_some() || self.unknown.contains(id)
    }

    /// Creates an instance of a template, or returns the one already there.
    ///
    /// **Creating twice is not an error and must not empty anything.** A mod
    /// re-entering a ship it already made calls this every time, and a "create"
    /// that wiped would be a ship that emptied on its second visit.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotATemplate`] if `template` was not registered with
    /// [`Spec::instanced`], and [`DomainError::BadId`] for an unusable key.
    pub fn create(&mut self, template: &str, key: &str) -> Result<String, DomainError> {
        match self.specs.get(template) {
            Some(spec) if spec.instanced => {}
            Some(_) => {
                return Err(DomainError::NotATemplate {
                    id: template.to_owned(),
                });
            }
            None => {
                return Err(DomainError::NoSuchDomain {
                    id: template.to_owned(),
                });
            }
        }
        check_key(key)?;
        let id = format!("{template}{INSTANCE_SEPARATOR}{key}");
        // Idempotent by construction: the id is a pure function of the
        // template and the key, so "already there" needs no test beyond
        // declining to touch what is in the map.
        self.instances
            .entry(id.clone())
            .or_insert_with(|| template.to_owned());
        self.unknown.remove(&id);
        Ok(id)
    }

    /// Forgets an instance, given nothing is inside it.
    ///
    /// The caller says how many entities and players the domain holds; this
    /// does not know and must not guess. Removing its chunks is the caller's
    /// job too — this owns the list, not the storage.
    ///
    /// # Errors
    ///
    /// [`DomainError::NotAnInstance`] for anything registered rather than
    /// created, [`DomainError::NoSuchDomain`] if there is no such instance, and
    /// [`DomainError::Occupied`] if `occupants` is not zero.
    pub fn destroy(&mut self, id: &str, occupants: usize) -> Result<(), DomainError> {
        if !self.instances.contains_key(id) {
            return Err(if self.specs.contains_key(id) {
                DomainError::NotAnInstance { id: id.to_owned() }
            } else {
                DomainError::NoSuchDomain { id: id.to_owned() }
            });
        }
        if occupants > 0 {
            return Err(DomainError::Occupied {
                id: id.to_owned(),
                occupants,
            });
        }
        self.instances.remove(id);
        Ok(())
    }

    /// Every instance that exists, as `(instance, template)`.
    ///
    /// Sorted, because this is written to the world file and read back: charter
    /// rule 4's ban on `HashMap` iteration order applies to anything a
    /// determinism gate might ever hash.
    #[must_use]
    pub fn instances(&self) -> Vec<(&str, &str)> {
        self.instances
            .iter()
            .map(|(id, template)| (id.as_str(), template.as_str()))
            .collect()
    }

    /// Restores instances read from the world file.
    ///
    /// An instance whose template is no longer registered is kept as an unknown
    /// domain rather than dropped — the module rule, applied to the case a mod
    /// removal actually produces.
    pub fn restore(&mut self, instances: Vec<(String, String)>) {
        for (id, template) in instances {
            if self.specs.get(&template).is_some_and(|spec| spec.instanced) {
                self.instances.insert(id, template);
            } else {
                self.unknown.insert(id);
            }
        }
    }

    /// Every domain that can be simulated right now, overworld first.
    #[must_use]
    pub fn live(&self) -> Vec<&str> {
        let mut all: Vec<&str> = self
            .specs
            .iter()
            .filter(|(_, spec)| !spec.instanced)
            .map(|(id, _)| id.as_str())
            .chain(self.instances.keys().map(String::as_str))
            .chain(self.unknown.iter().map(String::as_str))
            .collect();
        all.sort_unstable();
        all.dedup();
        all
    }
}

/// Whether an id is one a mod may register.
fn check_registered_id(id: &str) -> Result<(), DomainError> {
    let bad = |reason| {
        Err(DomainError::BadId {
            id: id.to_owned(),
            reason,
        })
    };
    if id.is_empty() {
        return bad("a domain needs a name");
    }
    if id.len() > MAX_KEY {
        return bad("longer than a domain id may be");
    }
    if id.contains(INSTANCE_SEPARATOR) {
        // Otherwise a registered `ship/17` and an instance `ship/17` would be
        // the same string meaning two different things, and the one that lost
        // would lose its chunks.
        return bad("a registered id may not contain `/`, which marks an instance");
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == ':')
    {
        return bad("a domain id is ASCII letters, digits, `_` and `:`");
    }
    Ok(())
}

/// Whether a key is one an instance may be made under.
fn check_key(key: &str) -> Result<(), DomainError> {
    let bad = |reason| {
        Err(DomainError::BadId {
            id: key.to_owned(),
            reason,
        })
    };
    if key.is_empty() {
        return bad("an instance needs a key");
    }
    if key.len() > MAX_KEY {
        return bad("longer than an instance key may be");
    }
    if !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        // No `:` and no `/`: a key is the half a mod chooses at runtime, and
        // letting it carry either would let it forge an id belonging to
        // somebody else's namespace or to another template.
        return bad("an instance key is ASCII letters, digits and `_`");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn template() -> Spec {
        Spec {
            instanced: true,
            ..Spec::default()
        }
    }

    #[test]
    fn a_new_world_is_one_domain_and_it_is_the_overworld() {
        let registry = Registry::new();
        assert_eq!(registry.live(), vec![OVERWORLD]);
        assert_eq!(
            registry.spec(OVERWORLD).map(|spec| spec.kind),
            Some(Kind::Voxel)
        );
    }

    #[test]
    fn the_default_registry_is_a_world_and_not_an_empty_one() {
        // A derived `Default` would hand back a registry with no overworld,
        // and every chunk in every existing save names `overworld` — so the
        // whole world would read as a domain nothing registers.
        assert_eq!(Registry::default().live(), vec![OVERWORLD]);
    }

    #[test]
    fn registering_stops_when_the_window_closes() {
        // Charter rule 9: `register_*` after the freeze is a hard error, not a
        // late addition that half the engine has already been built without.
        let mut registry = Registry::new();
        registry
            .register("mod:space", Spec::default())
            .expect("during the window");
        registry.freeze();
        assert!(matches!(
            registry.register("mod:late", Spec::default()),
            Err(DomainError::Frozen { .. })
        ));
    }

    #[test]
    fn a_second_registration_is_refused_rather_than_replacing_the_first() {
        let mut registry = Registry::new();
        registry
            .register("mod:space", Spec::default())
            .expect("first");
        assert!(matches!(
            registry.register("mod:space", Spec::default()),
            Err(DomainError::Duplicate { .. })
        ));
    }

    #[test]
    fn a_template_is_not_itself_a_domain() {
        // The whole point of `instanced`: no domain exists under the template's
        // id and nothing is stored for it, so asking for its spec as a domain
        // answers nothing and it never turns up in the live list.
        let mut registry = Registry::new();
        registry.register("mod:ship", template()).expect("register");
        assert_eq!(registry.spec("mod:ship"), None);
        assert!(!registry.live().contains(&"mod:ship"));
    }

    #[test]
    fn creating_an_instance_twice_gives_the_same_one_back() {
        // **The case a mod hits every time somebody re-enters their ship.** A
        // "create" that emptied would be a ship that emptied on its second
        // visit, so this asserts the identity and that nothing was reset.
        let mut registry = Registry::new();
        registry.register("mod:ship", template()).expect("register");

        let first = registry.create("mod:ship", "17").expect("create");
        let again = registry.create("mod:ship", "17").expect("create again");
        assert_eq!(first, again);
        assert_eq!(first, "mod:ship/17");
        assert_eq!(
            registry.instances().len(),
            1,
            "creating twice made two instances"
        );

        // And an instance is a domain in every way a registered one is.
        assert_eq!(
            registry.spec(&first).map(|spec| spec.kind),
            Some(Kind::Voxel)
        );
        assert!(registry.live().contains(&first.as_str()));
    }

    #[test]
    fn only_a_template_has_instances() {
        let mut registry = Registry::new();
        registry
            .register("mod:space", Spec::default())
            .expect("register");
        assert!(matches!(
            registry.create("mod:space", "1"),
            Err(DomainError::NotATemplate { .. })
        ));
        assert!(matches!(
            registry.create("mod:nothing", "1"),
            Err(DomainError::NoSuchDomain { .. })
        ));
    }

    #[test]
    fn a_domain_with_anybody_in_it_is_not_destroyed() {
        // The same defect as breaking a container somebody has open.
        let mut registry = Registry::new();
        registry.register("mod:ship", template()).expect("register");
        let id = registry.create("mod:ship", "17").expect("create");

        assert!(matches!(
            registry.destroy(&id, 1),
            Err(DomainError::Occupied { occupants: 1, .. })
        ));
        assert!(
            registry.live().contains(&id.as_str()),
            "it was removed anyway"
        );

        registry.destroy(&id, 0).expect("empty, so it goes");
        assert!(!registry.live().contains(&id.as_str()));
    }

    #[test]
    fn a_registered_domain_cannot_be_destroyed() {
        let mut registry = Registry::new();
        registry
            .register("mod:space", Spec::default())
            .expect("register");
        assert!(matches!(
            registry.destroy("mod:space", 0),
            Err(DomainError::NotAnInstance { .. })
        ));
        assert!(matches!(
            registry.destroy(OVERWORLD, 0),
            Err(DomainError::NotAnInstance { .. })
        ));
    }

    #[test]
    fn a_domain_nothing_registers_any_more_is_kept_and_comes_back() {
        // Charter rule 8's rule for materials, applied to domains: data a mod
        // cannot currently name is still that player's data. A world with a
        // domain whose mod was removed must open, keep it, and hand it back
        // unchanged when the mod returns.
        let mut registry = Registry::new();
        registry.note_unknown("gone:place");
        assert!(registry.exists("gone:place"), "it was dropped");
        assert_eq!(registry.spec("gone:place"), None, "it has no generator");
        assert_eq!(registry.unknown(), vec!["gone:place"]);
        assert!(registry.live().contains(&"gone:place"));

        registry
            .register("gone:place", Spec::default())
            .expect("the mod came back");
        assert!(registry.spec("gone:place").is_some());
        assert!(registry.unknown().is_empty());
    }

    #[test]
    fn an_instance_whose_template_is_gone_is_preserved_rather_than_dropped() {
        // The case a mod removal actually produces: the instance list in the
        // world file names a template nothing registers now. Its chunks are
        // real, so it stays — as an unknown domain, exactly like any other.
        let mut registry = Registry::new();
        registry.restore(vec![("gone:ship/17".to_owned(), "gone:ship".to_owned())]);
        assert!(registry.exists("gone:ship/17"));
        assert_eq!(registry.unknown(), vec!["gone:ship/17"]);
        assert!(
            registry.instances().is_empty(),
            "it was restored as a live instance of a template that does not exist"
        );
    }

    #[test]
    fn instances_survive_a_round_trip_through_the_world_file() {
        let mut registry = Registry::new();
        registry.register("mod:ship", template()).expect("register");
        registry.create("mod:ship", "17").expect("create");
        registry.create("mod:ship", "18").expect("create");
        let saved: Vec<(String, String)> = registry
            .instances()
            .into_iter()
            .map(|(id, template)| (id.to_owned(), template.to_owned()))
            .collect();

        let mut reopened = Registry::new();
        reopened.register("mod:ship", template()).expect("register");
        reopened.restore(saved);
        assert_eq!(
            reopened.instances(),
            vec![("mod:ship/17", "mod:ship"), ("mod:ship/18", "mod:ship")]
        );
    }

    #[test]
    fn an_id_that_could_be_mistaken_for_an_instance_is_refused() {
        // A registered `ship/17` and a created `ship/17` would be one string
        // meaning two things, and whichever lost would lose its chunks with it.
        let mut registry = Registry::new();
        assert!(matches!(
            registry.register("mod:ship/17", Spec::default()),
            Err(DomainError::BadId { .. })
        ));
        assert!(matches!(
            registry.register("", Spec::default()),
            Err(DomainError::BadId { .. })
        ));
        assert!(matches!(
            registry.register("mod:a b", Spec::default()),
            Err(DomainError::BadId { .. })
        ));

        registry.register("mod:ship", template()).expect("register");
        for bad in ["", "a/b", "a:b", "a b", &"k".repeat(MAX_KEY + 1)] {
            assert!(
                matches!(
                    registry.create("mod:ship", bad),
                    Err(DomainError::BadId { .. })
                ),
                "`{bad}` was accepted as an instance key"
            );
        }
    }
}
