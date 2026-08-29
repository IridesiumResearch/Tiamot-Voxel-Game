// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The live domain registry, and where a mod's runtime calls reach it.
//!
//! The registry itself is [`tiamot_core::domain::Registry`]; this is the shared
//! handle around it, the same arrangement `ent::Shared` and `fluid::Shared`
//! use and behind a lock for the same reason: `game.create_domain` runs inside
//! a tick, on the simulation thread, and cannot borrow what the tick is
//! holding.
//!
//! # Destroying takes two things this does not have
//!
//! Refusing to destroy a domain somebody is standing in needs a count of who is
//! inside, and removing its chunks needs the world. Both belong to the tick, so
//! a destroy is QUEUED here and performed there — exactly as a transfer is, and
//! for a second reason as well: a mod destroying the domain it is currently
//! running a callback about should not have the ground removed underneath that
//! callback.

use std::sync::{Arc, Mutex, RwLock};

use tiamot_core::domain::{Access, Registry};

/// A handle on the domain registry, for the mod API.
pub struct Shared {
    registry: Arc<RwLock<Registry>>,
    /// Instances a mod has asked to destroy, waiting for the tick.
    ///
    /// See the module documentation: the refusal and the chunk removal both
    /// need things only the tick has.
    doomed: Mutex<Vec<String>>,
}

impl Shared {
    /// Wraps the registry the simulation thread owns.
    #[must_use]
    pub fn new(registry: Arc<RwLock<Registry>>) -> Self {
        Self {
            registry,
            doomed: Mutex::new(Vec::new()),
        }
    }

    /// Takes every instance asked to be destroyed since the last tick.
    #[must_use]
    pub fn take_doomed(&self) -> Vec<String> {
        self.doomed
            .lock()
            .map(|mut queued| std::mem::take(&mut *queued))
            .unwrap_or_default()
    }
}

impl Access for Shared {
    fn create(&self, template: &str, key: &str) -> Option<String> {
        // Creating is immediate: it makes an entry and touches no storage, so
        // there is nothing for the tick to do and a mod that has just made a
        // ship can move somebody into it in the same breath.
        self.registry
            .write()
            .ok()?
            .create(template, key)
            .inspect_err(|err| tracing::warn!(%err, "a mod could not make a domain"))
            .ok()
    }

    fn destroy(&self, id: &str) -> bool {
        // Only the answers that are knowable here. Whether anybody is inside is
        // the tick's to say, so this reports whether the request was worth
        // queueing at all.
        let known = self
            .registry
            .read()
            .is_ok_and(|registry| registry.instances().iter().any(|(name, _)| *name == id));
        if !known {
            return false;
        }
        self.doomed
            .lock()
            .map(|mut queued| queued.push(id.to_owned()))
            .is_ok()
    }

    fn exists(&self, id: &str) -> bool {
        self.registry
            .read()
            .is_ok_and(|registry| registry.exists(id))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiamot_core::domain::Spec;

    fn registry_with_a_template() -> Arc<RwLock<Registry>> {
        let mut registry = Registry::new();
        registry
            .register(
                "mod:ship",
                Spec {
                    instanced: true,
                    ..Spec::default()
                },
            )
            .expect("register");
        registry.freeze();
        Arc::new(RwLock::new(registry))
    }

    #[test]
    fn making_a_ship_answers_with_its_id() {
        // The id and not a boolean: a mod transfers things in by name, and
        // making it rebuild `template/key` itself would mean teaching every mod
        // a spelling that belongs to the engine.
        let shared = Shared::new(registry_with_a_template());
        assert_eq!(
            shared.create("mod:ship", "17").as_deref(),
            Some("mod:ship/17")
        );
        assert!(shared.exists("mod:ship/17"));
        assert!(!shared.exists("mod:ship/18"));
    }

    #[test]
    fn making_the_same_ship_twice_answers_the_same_way() {
        // The case a mod hits every time somebody re-enters a ship it already
        // made. Nothing is emptied and nothing is refused.
        let shared = Shared::new(registry_with_a_template());
        let first = shared.create("mod:ship", "17");
        let again = shared.create("mod:ship", "17");
        assert_eq!(first, again);
        assert_eq!(first.as_deref(), Some("mod:ship/17"));
    }

    #[test]
    fn asking_for_an_instance_of_something_that_is_not_a_template_answers_nothing() {
        let registry = registry_with_a_template();
        let shared = Shared::new(registry);
        assert_eq!(shared.create("mod:nothing", "17"), None);
        assert_eq!(shared.create(tiamot_core::domain::OVERWORLD, "17"), None);
    }

    #[test]
    fn destroying_is_queued_for_the_tick_rather_than_done_here() {
        // **Refusing needs a count of who is inside, and removing the chunks
        // needs the world.** Both belong to the tick, so this only decides
        // whether the request is worth carrying.
        let shared = Shared::new(registry_with_a_template());
        let id = shared.create("mod:ship", "17").expect("create");

        assert!(shared.destroy(&id));
        assert!(
            shared.exists(&id),
            "the domain went the moment it was asked for, before anything could \\
             check whether somebody was standing in it"
        );
        assert_eq!(shared.take_doomed(), vec![id]);
        assert!(
            shared.take_doomed().is_empty(),
            "draining left the request in the queue"
        );
    }

    #[test]
    fn destroying_something_that_was_never_an_instance_is_refused_here() {
        let shared = Shared::new(registry_with_a_template());
        assert!(!shared.destroy("mod:ship/17"), "it was never created");
        assert!(!shared.destroy("mod:ship"), "a template is not an instance");
        assert!(!shared.destroy(tiamot_core::domain::OVERWORLD));
        assert!(shared.take_doomed().is_empty());
    }
}
