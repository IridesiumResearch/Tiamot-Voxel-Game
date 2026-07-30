// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Dependency resolution and load ordering.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use crate::modload::manifest::{DiscoveredMod, parse_requirement};

/// A mod in the resolved set, with everything the loader needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedMod {
    /// The mod's id.
    pub id: String,
    /// Its version.
    pub version: semver::Version,
    /// Its directory.
    pub dir: PathBuf,
    /// Ids this mod must load after, resolved through aliases.
    pub after: Vec<String>,
}

/// The resolved mod set, in load order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSet {
    /// Mods in the order they must be loaded.
    pub order: Vec<ResolvedMod>,
}

impl ResolvedSet {
    /// Ids in load order.
    #[must_use]
    pub fn ids(&self) -> Vec<&str> {
        self.order.iter().map(|entry| entry.id.as_str()).collect()
    }

    /// A stable fingerprint of the resolved set.
    ///
    /// Becomes the server's mod manifest: a client can compare it to know
    /// whether it is looking at the same mod set, without transferring the list.
    /// Covers id, version, and order — order matters because it decides numeric
    /// ids (charter rule 8).
    #[must_use]
    pub fn fingerprint(&self) -> u64 {
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        for entry in &self.order {
            for byte in entry
                .id
                .as_bytes()
                .iter()
                .chain(b"@")
                .chain(entry.version.to_string().as_bytes())
                .chain(b";")
            {
                hash ^= u64::from(*byte);
                hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
            }
        }
        hash
    }
}

/// Resolution failed.
#[derive(Debug, thiserror::Error)]
pub enum ResolveError {
    /// Two mods claim the same id.
    #[error("mod id `{id}` is claimed by both `{first}` and `{second}`")]
    DuplicateId {
        /// The contested id.
        id: String,
        /// First directory.
        first: PathBuf,
        /// Second directory.
        second: PathBuf,
    },

    /// A dependency is not installed.
    #[error(
        "mod `{requirer}` requires `{requirement}`, but no mod provides `{missing}`{available}"
    )]
    MissingDependency {
        /// Who needs it.
        requirer: String,
        /// The full requirement text.
        requirement: String,
        /// The id that could not be found.
        missing: String,
        /// A hint listing what is installed, when that might help.
        available: String,
    },

    /// A dependency is installed at an incompatible version.
    #[error(
        "mod `{requirer}` requires `{requirement}`, but `{found_id}` is installed at version \
         {found_version}"
    )]
    VersionConflict {
        /// Who needs it.
        requirer: String,
        /// The full requirement text.
        requirement: String,
        /// What was found.
        found_id: String,
        /// At what version.
        found_version: semver::Version,
    },

    /// The dependency graph contains a cycle.
    #[error("dependency cycle: {}", cycle.join(" -> "))]
    Cycle {
        /// The full cycle, first id repeated at the end.
        cycle: Vec<String>,
    },

    /// A manifest declared a requirement that will not parse.
    #[error("mod `{requirer}` declares an unparseable requirement `{requirement}`")]
    BadRequirement {
        /// Who declared it.
        requirer: String,
        /// The offending text.
        requirement: String,
    },
}

/// Resolves a discovered mod set into a deterministic load order.
///
/// Rules, in the order they are applied:
///
/// 1. Exactly one active version per id — duplicates are an error, not a
///    silent pick.
/// 2. `provides` aliases satisfy dependencies.
/// 3. Semver ranges must match.
/// 4. Cycles are reported in full.
/// 5. Load order is topological with an **alphabetical tiebreak**, so it is
///    identical on every machine.
///
/// # Errors
///
/// [`ResolveError`] naming the mod, the requirement, and what was found.
pub fn resolve(mods: &[DiscoveredMod]) -> Result<ResolvedSet, ResolveError> {
    // -- 1. one mod per id -------------------------------------------------
    let mut by_id: BTreeMap<&str, &DiscoveredMod> = BTreeMap::new();
    for found in mods {
        if let Some(existing) = by_id.insert(found.manifest.id.as_str(), found) {
            return Err(ResolveError::DuplicateId {
                id: found.manifest.id.clone(),
                first: existing.dir.clone(),
                second: found.dir.clone(),
            });
        }
    }

    // -- 2. alias table ----------------------------------------------------
    // An alias resolves to the real mod providing it. A mod's own id always
    // wins over an alias — otherwise installing a compatibility shim could
    // shadow the real thing.
    let mut aliases: BTreeMap<&str, &str> = BTreeMap::new();
    for found in mods {
        for alias in &found.manifest.provides {
            if by_id.contains_key(alias.as_str()) {
                // A real mod owns this id; the alias is ignored rather than
                // being allowed to shadow it.
                continue;
            }
            aliases.insert(alias.as_str(), found.manifest.id.as_str());
        }
    }

    let lookup = |name: &str| -> Option<&DiscoveredMod> {
        by_id
            .get(name)
            .copied()
            .or_else(|| aliases.get(name).and_then(|real| by_id.get(real).copied()))
    };

    // -- 3. edges, with version checks -------------------------------------
    let mut edges: BTreeMap<&str, BTreeSet<String>> = BTreeMap::new();
    for found in mods {
        let entry = edges.entry(found.manifest.id.as_str()).or_default();

        for (requirement, required) in found
            .manifest
            .depends
            .iter()
            .map(|text| (text, true))
            .chain(
                found
                    .manifest
                    .optional_depends
                    .iter()
                    .map(|text| (text, false)),
            )
        {
            let (name, range) =
                parse_requirement(requirement).ok_or_else(|| ResolveError::BadRequirement {
                    requirer: found.manifest.id.clone(),
                    requirement: requirement.clone(),
                })?;

            let Some(target) = lookup(&name) else {
                if !required {
                    // An absent optional dependency is the normal case, not a
                    // problem. It simply contributes no ordering edge.
                    continue;
                }
                return Err(ResolveError::MissingDependency {
                    requirer: found.manifest.id.clone(),
                    requirement: requirement.clone(),
                    missing: name,
                    available: describe_available(&by_id),
                });
            };

            let version = target
                .manifest
                .semver()
                .map_err(|_| ResolveError::BadRequirement {
                    requirer: target.manifest.id.clone(),
                    requirement: target.manifest.version.clone(),
                })?;

            if !range.matches(&version) {
                // Version conflicts apply to optional dependencies too: an
                // optional dependency that IS installed at an incompatible
                // version is a real problem, not something to ignore.
                return Err(ResolveError::VersionConflict {
                    requirer: found.manifest.id.clone(),
                    requirement: requirement.clone(),
                    found_id: target.manifest.id.clone(),
                    found_version: version,
                });
            }

            // Self-dependency via an alias would be a one-node cycle. Drop it
            // rather than reporting a cycle the operator cannot act on.
            if target.manifest.id != found.manifest.id {
                entry.insert(target.manifest.id.clone());
            }
        }
    }

    // -- 4 & 5. topological order, alphabetical tiebreak -------------------
    let order = topological_order(&by_id, &edges)?;

    Ok(ResolvedSet {
        order: order
            .into_iter()
            .map(|id| {
                let found = by_id[id.as_str()];
                ResolvedMod {
                    id: id.clone(),
                    version: found
                        .manifest
                        .semver()
                        .unwrap_or_else(|_| semver::Version::new(0, 0, 0)),
                    dir: found.dir.clone(),
                    after: edges
                        .get(id.as_str())
                        .map(|set| set.iter().cloned().collect())
                        .unwrap_or_default(),
                }
            })
            .collect(),
    })
}

/// Kahn's algorithm with a sorted ready set.
///
/// The sorted ready set is what makes the order deterministic. A plain queue
/// would produce an order that depended on insertion sequence, which depends on
/// the scan, which depends on the filesystem.
fn topological_order(
    by_id: &BTreeMap<&str, &DiscoveredMod>,
    edges: &BTreeMap<&str, BTreeSet<String>>,
) -> Result<Vec<String>, ResolveError> {
    // remaining[id] = how many of its dependencies are not yet loaded.
    let mut remaining: BTreeMap<&str, usize> = by_id
        .keys()
        .map(|id| (*id, edges.get(id).map_or(0, BTreeSet::len)))
        .collect();

    // dependants[x] = mods that must load after x.
    let mut dependants: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
    for (id, deps) in edges {
        for dep in deps {
            dependants.entry(dep.as_str()).or_default().push(id);
        }
    }

    let mut order = Vec::with_capacity(by_id.len());
    // A BTreeSet as the ready set: always pops the alphabetically first mod
    // whose dependencies are satisfied.
    let mut ready: BTreeSet<&str> = remaining
        .iter()
        .filter(|(_, count)| **count == 0)
        .map(|(id, _)| *id)
        .collect();

    while let Some(&next) = ready.iter().next() {
        ready.remove(next);
        order.push(next.to_owned());

        for dependant in dependants.get(next).into_iter().flatten() {
            let count = remaining
                .get_mut(*dependant)
                .expect("every dependant is a known mod");
            *count -= 1;
            if *count == 0 {
                ready.insert(dependant);
            }
        }
    }

    if order.len() != by_id.len() {
        // Whatever is left is in, or downstream of, a cycle.
        let stuck: BTreeSet<&str> = remaining
            .iter()
            .filter(|(id, _)| !order.iter().any(|done| done == *id))
            .map(|(id, _)| *id)
            .collect();
        return Err(ResolveError::Cycle {
            cycle: find_cycle(&stuck, edges),
        });
    }

    Ok(order)
}

/// Extracts one concrete cycle from the stuck set.
///
/// Reporting "there is a cycle" is not actionable. Reporting
/// `a -> b -> c -> a` is: it names every mod the operator has to look at, in
/// the order they depend on each other.
fn find_cycle(stuck: &BTreeSet<&str>, edges: &BTreeMap<&str, BTreeSet<String>>) -> Vec<String> {
    // Walk dependency edges from the alphabetically first stuck mod until a
    // node repeats. Every node here has at least one unsatisfied dependency
    // inside the set, so a repeat is guaranteed.
    let Some(&start) = stuck.iter().next() else {
        return Vec::new();
    };

    let mut path: Vec<&str> = Vec::new();
    let mut current = start;
    loop {
        if let Some(at) = path.iter().position(|seen| *seen == current) {
            let mut cycle: Vec<String> = path[at..].iter().map(|id| (*id).to_owned()).collect();
            // Repeat the first id at the end so the loop is visible as a loop.
            cycle.push(current.to_owned());
            return cycle;
        }
        path.push(current);

        let Some(next) = edges
            .get(current)
            .and_then(|deps| deps.iter().find(|dep| stuck.contains(dep.as_str())))
        else {
            // Ran out of stuck dependencies without repeating — this node is
            // downstream of a cycle rather than in one. Report the path.
            return path.iter().map(|id| (*id).to_owned()).collect();
        };
        current = next.as_str();
    }
}

/// A short "what is installed" hint for a missing-dependency error.
fn describe_available(by_id: &BTreeMap<&str, &DiscoveredMod>) -> String {
    if by_id.is_empty() {
        return " (no mods are installed)".to_owned();
    }
    let names: Vec<&str> = by_id.keys().copied().collect();
    format!(" (installed: {})", names.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::modload::manifest::ModManifest;

    /// Builds a discovered mod without touching the filesystem.
    fn make(id: &str, version: &str, depends: &[&str]) -> DiscoveredMod {
        DiscoveredMod {
            manifest: ModManifest {
                id: id.to_owned(),
                name: id.to_owned(),
                version: version.to_owned(),
                depends: depends.iter().map(|d| (*d).to_owned()).collect(),
                optional_depends: Vec::new(),
                provides: Vec::new(),
                description: String::new(),
                license: String::new(),
            },
            dir: PathBuf::from(format!("/mods/{id}")),
        }
    }

    fn with_provides(mut found: DiscoveredMod, provides: &[&str]) -> DiscoveredMod {
        found.manifest.provides = provides.iter().map(|p| (*p).to_owned()).collect();
        found
    }

    fn with_optional(mut found: DiscoveredMod, optional: &[&str]) -> DiscoveredMod {
        found.manifest.optional_depends = optional.iter().map(|o| (*o).to_owned()).collect();
        found
    }

    #[test]
    fn a_simple_chain_loads_in_dependency_order() {
        let mods = [
            make("c", "1.0.0", &["b"]),
            make("b", "1.0.0", &["a"]),
            make("a", "1.0.0", &[]),
        ];
        let resolved = resolve(&mods).expect("resolve");
        assert_eq!(resolved.ids(), vec!["a", "b", "c"]);
    }

    #[test]
    fn a_diamond_resolves_with_an_alphabetical_tiebreak() {
        //     base
        //     /  \
        //  left  right
        //     \  /
        //      top
        let mods = [
            make("top", "1.0.0", &["left", "right"]),
            make("right", "1.0.0", &["base"]),
            make("left", "1.0.0", &["base"]),
            make("base", "1.0.0", &[]),
        ];
        let resolved = resolve(&mods).expect("resolve");
        // base first, top last; the two middles are tied and sort alphabetically.
        assert_eq!(resolved.ids(), vec!["base", "left", "right", "top"]);
    }

    #[test]
    fn order_is_identical_however_the_input_is_shuffled() {
        // THE property this module exists for. Load order decides numeric
        // material ids, which go into every chunk blob; an order that depended
        // on filesystem enumeration would give two servers incompatible worlds
        // from identical inputs.
        let base = [
            make("zebra", "1.0.0", &["alpha"]),
            make("alpha", "1.0.0", &[]),
            make("mango", "1.0.0", &["alpha"]),
            make("kiwi", "1.0.0", &["mango", "zebra"]),
        ];
        let expected = resolve(&base).expect("resolve").ids().join(",");

        // Every rotation of the input.
        for rotation in 0..base.len() {
            let mut shuffled = base.to_vec();
            shuffled.rotate_left(rotation);
            let got = resolve(&shuffled).expect("resolve").ids().join(",");
            assert_eq!(got, expected, "rotation {rotation} changed the order");
        }

        // And reversed.
        let mut reversed = base.to_vec();
        reversed.reverse();
        assert_eq!(
            resolve(&reversed).expect("resolve").ids().join(","),
            expected
        );
    }

    #[test]
    fn independent_mods_load_alphabetically() {
        let mods = [
            make("zulu", "1.0.0", &[]),
            make("alpha", "1.0.0", &[]),
            make("mike", "1.0.0", &[]),
        ];
        assert_eq!(
            resolve(&mods).expect("resolve").ids(),
            vec!["alpha", "mike", "zulu"]
        );
    }

    #[test]
    fn a_cycle_is_reported_in_full() {
        let mods = [
            make("a", "1.0.0", &["c"]),
            make("b", "1.0.0", &["a"]),
            make("c", "1.0.0", &["b"]),
        ];
        let err = resolve(&mods).expect_err("cycle should be rejected");
        let ResolveError::Cycle { cycle } = &err else {
            panic!("expected a cycle error, got {err:?}");
        };

        // Every member named, and the loop closed so it reads as a loop.
        assert!(cycle.len() >= 4, "cycle should show the loop: {cycle:?}");
        assert_eq!(
            cycle.first(),
            cycle.last(),
            "the cycle should close: {cycle:?}"
        );
        for member in ["a", "b", "c"] {
            assert!(
                cycle.iter().any(|id| id == member),
                "`{member}` missing from {cycle:?}"
            );
        }
        assert!(err.to_string().contains("->"), "{err}");
    }

    #[test]
    fn a_two_mod_cycle_is_reported() {
        let mods = [make("a", "1.0.0", &["b"]), make("b", "1.0.0", &["a"])];
        let err = resolve(&mods).expect_err("cycle");
        assert!(matches!(err, ResolveError::Cycle { .. }), "{err:?}");
    }

    #[test]
    fn a_missing_dependency_names_everything_needed_to_fix_it() {
        let mods = [make("mine", "1.0.0", &["absent >=1.0"])];
        let err = resolve(&mods).expect_err("missing dependency");
        let text = err.to_string();
        assert!(text.contains("mine"), "should name the requirer: {text}");
        assert!(
            text.contains("absent"),
            "should name the missing mod: {text}"
        );
        assert!(
            text.contains(">=1.0"),
            "should name the requirement: {text}"
        );
        assert!(
            text.contains("installed"),
            "should say what IS there: {text}"
        );
    }

    #[test]
    fn a_version_conflict_names_what_was_found() {
        let mods = [
            make("mine", "1.0.0", &["other >=2.0"]),
            make("other", "1.0.0", &[]),
        ];
        let err = resolve(&mods).expect_err("version conflict");
        let text = err.to_string();
        assert!(text.contains("mine"), "{text}");
        assert!(text.contains(">=2.0"), "{text}");
        assert!(
            text.contains("1.0.0"),
            "should name the found version: {text}"
        );
    }

    #[test]
    fn a_provides_alias_satisfies_a_dependency() {
        let mods = [
            make("mine", "1.0.0", &["oldmod"]),
            with_provides(make("newmod", "2.0.0", &[]), &["oldmod"]),
        ];
        let resolved = resolve(&mods).expect("alias should satisfy");
        assert_eq!(resolved.ids(), vec!["newmod", "mine"]);
    }

    #[test]
    fn an_alias_cannot_shadow_a_real_mod() {
        // If it could, installing a compatibility shim would silently redirect
        // every dependant away from the mod they actually asked for.
        let mods = [
            make("mine", "1.0.0", &["target"]),
            make("target", "1.0.0", &[]),
            with_provides(make("impostor", "9.0.0", &[]), &["target"]),
        ];
        let resolved = resolve(&mods).expect("resolve");
        let mine = resolved
            .order
            .iter()
            .find(|entry| entry.id == "mine")
            .expect("mine");
        assert_eq!(
            mine.after,
            vec!["target".to_owned()],
            "the real mod must win over the alias"
        );
    }

    #[test]
    fn an_aliased_version_is_still_range_checked() {
        let mods = [
            make("mine", "1.0.0", &["oldmod >=3.0"]),
            with_provides(make("newmod", "2.0.0", &[]), &["oldmod"]),
        ];
        assert!(matches!(
            resolve(&mods).expect_err("version conflict"),
            ResolveError::VersionConflict { .. }
        ));
    }

    #[test]
    fn an_absent_optional_dependency_is_fine() {
        let mods = [with_optional(make("mine", "1.0.0", &[]), &["maybe"])];
        let resolved = resolve(&mods).expect("absent optional deps are normal");
        assert_eq!(resolved.ids(), vec!["mine"]);
    }

    #[test]
    fn a_present_optional_dependency_orders_before_its_dependant() {
        // The point of optional_depends: if it IS installed, it must load first
        // so the dependant can see what it registered.
        let mods = [
            with_optional(make("addon", "1.0.0", &[]), &["maybe"]),
            make("maybe", "1.0.0", &[]),
        ];
        let resolved = resolve(&mods).expect("resolve");
        assert_eq!(resolved.ids(), vec!["maybe", "addon"]);
    }

    #[test]
    fn a_present_optional_dependency_at_a_bad_version_is_still_a_conflict() {
        // "Optional" means it may be absent, not that any version will do.
        let mods = [
            with_optional(make("addon", "1.0.0", &[]), &["maybe >=5.0"]),
            make("maybe", "1.0.0", &[]),
        ];
        assert!(matches!(
            resolve(&mods).expect_err("conflict"),
            ResolveError::VersionConflict { .. }
        ));
    }

    #[test]
    fn duplicate_ids_are_an_error_not_a_silent_pick() {
        let mut second = make("dupe", "2.0.0", &[]);
        second.dir = PathBuf::from("/other/dupe");
        let mods = [make("dupe", "1.0.0", &[]), second];
        let err = resolve(&mods).expect_err("duplicate");
        assert!(matches!(err, ResolveError::DuplicateId { .. }), "{err:?}");
        assert!(err.to_string().contains("/other/dupe"), "{err}");
    }

    #[test]
    fn the_fingerprint_tracks_identity_version_and_order() {
        let a = resolve(&[make("a", "1.0.0", &[]), make("b", "1.0.0", &[])]).expect("resolve");
        let same = resolve(&[make("b", "1.0.0", &[]), make("a", "1.0.0", &[])]).expect("resolve");
        assert_eq!(
            a.fingerprint(),
            same.fingerprint(),
            "shuffling the input must not change the fingerprint"
        );

        let bumped = resolve(&[make("a", "1.0.1", &[]), make("b", "1.0.0", &[])]).expect("resolve");
        assert_ne!(
            a.fingerprint(),
            bumped.fingerprint(),
            "a version bump must show"
        );

        // Order matters because it decides numeric ids.
        let reordered =
            resolve(&[make("a", "1.0.0", &["b"]), make("b", "1.0.0", &[])]).expect("resolve");
        assert_ne!(a.fingerprint(), reordered.fingerprint());
    }

    #[test]
    fn an_empty_mod_set_resolves_to_nothing() {
        let resolved = resolve(&[]).expect("an empty set is valid");
        assert!(resolved.order.is_empty());
    }

    #[test]
    fn a_self_dependency_via_an_alias_does_not_become_a_cycle() {
        // A mod that provides an alias and also depends on it is odd but not
        // broken; reporting a one-node cycle would be an error nobody can act
        // on.
        let mods = [with_provides(make("mine", "1.0.0", &["shim"]), &["shim"])];
        let resolved = resolve(&mods).expect("self-alias should not deadlock");
        assert_eq!(resolved.ids(), vec!["mine"]);
    }
}
