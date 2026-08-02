// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! String ⇄ numeric material id mapping, owned by the world (charter rule 8).
//!
//! # The problem this solves
//!
//! Runtime [`MaterialId`]s are a per-session interning of string ids and are
//! **not stable across runs** — load two different sets of mods and `stone`
//! gets a different number. Chunk blobs on disk hold numeric ids. If those were
//! runtime ids, removing one mod would silently reinterpret every block in the
//! world as some other material.
//!
//! So the world owns its own numeric id space, persisted in `id_map`, and the
//! codec translates between the two on every read and write.
//!
//! # Mod churn is the hard case
//!
//! A player removes a mod. Their world is full of blocks referencing ids that
//! no longer resolve to anything. The naive outcomes are both unacceptable:
//! deleting them destroys the player's build, and remapping them to a live
//! material corrupts it more subtly.
//!
//! Charter rule 8's answer is that unregistered ids map to a preserved
//! `engine:unknown` placeholder and **round-trip byte-for-byte**. Getting that
//! right needs one non-obvious move: the absent mod's string ids are registered
//! in the *runtime* registry anyway, as aliases with no behaviour. They occupy
//! runtime ids, so the translation back to world ids on save is exact, and the
//! blocks survive untouched until the mod returns.
//!
//! Without that, the world→runtime direction would collapse every absent
//! material onto one placeholder id and the save would write that placeholder
//! back over the player's build.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::material::{MaterialId, MaterialRegistry, Registry};

/// The world's persistent string ⇄ numeric id table.
///
/// Numeric ids are `u16`, the same width as [`MaterialId`], so translation is
/// total in both directions and a world can hold as many materials as a session
/// can.
#[derive(Debug, Clone, Default)]
pub struct IdTable {
    by_name: BTreeMap<String, u16>,
    by_id: BTreeMap<u16, String>,
}

impl IdTable {
    /// Reads the table from an open world database.
    ///
    /// # Errors
    ///
    /// Any SQL failure, or a stored id outside `u16`.
    pub fn load(conn: &Connection) -> Result<Self, IdMapError> {
        let mut table = Self::default();
        let mut statement = conn.prepare("SELECT string_id, numeric_id FROM id_map")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        for row in rows {
            let (name, numeric) = row?;
            let numeric = u16::try_from(numeric).map_err(|_| IdMapError::IdOutOfRange {
                name: name.clone(),
                numeric,
            })?;
            table.by_name.insert(name.clone(), numeric);
            table.by_id.insert(numeric, name);
        }
        Ok(table)
    }

    /// World numeric id for a string id.
    #[must_use]
    pub fn id_of(&self, name: &str) -> Option<u16> {
        self.by_name.get(name).copied()
    }

    /// String id for a world numeric id.
    #[must_use]
    pub fn name_of(&self, id: u16) -> Option<&str> {
        self.by_id.get(&id).map(String::as_str)
    }

    /// How many materials this world knows about.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether the table is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Lowest unused numeric id.
    fn next_free(&self) -> Option<u16> {
        // Reserved ids 0 and 1 are always present after `reconcile`, so this
        // walks up from 2. Linear rather than max+1 so ids freed by a future
        // compaction would be reused rather than leaked.
        (2..=u16::MAX).find(|candidate| !self.by_id.contains_key(candidate))
    }

    /// Reconciles the world's table against the materials a session registered.
    ///
    /// - Names already in the table keep their numeric ids, always.
    /// - Names registered this session but absent from the table get the next
    ///   free id, and the table is written back.
    /// - **Names in the table that no mod registered are never removed.** They
    ///   are registered into `registry` as behaviourless aliases so their blocks
    ///   round-trip, and reported in [`MaterialMap::unknown`].
    ///
    /// # Errors
    ///
    /// Any SQL failure, or exhaustion of the numeric id space.
    pub fn reconcile(
        &mut self,
        conn: &Connection,
        registry: &mut Registry,
    ) -> Result<MaterialMap, IdMapError> {
        // The reserved materials always occupy the same ids in both spaces, so
        // air is air whatever else changed.
        self.ensure(conn, crate::material::AIR_NAME, MaterialId::AIR.get())?;
        self.ensure(
            conn,
            crate::material::UNKNOWN_NAME,
            MaterialId::UNKNOWN.get(),
        )?;

        // Registered names that the world has not seen before.
        let registered: Vec<String> = registry.iter().map(|(_, name)| name.to_owned()).collect();
        for name in &registered {
            if self.by_name.contains_key(name.as_str()) {
                continue;
            }
            let numeric = self.next_free().ok_or(IdMapError::Exhausted)?;
            self.insert(conn, name, numeric)?;
        }

        // Names the world knows that this session does not. Registering them
        // keeps the round-trip exact; see the module docs.
        let mut unknown = BTreeSet::new();
        let orphans: Vec<String> = self
            .by_name
            .keys()
            .filter(|name| registry.id_of(name).is_none())
            .cloned()
            .collect();
        for name in orphans {
            let runtime = registry
                .register(&name)
                .map_err(|source| IdMapError::Alias { name, source })?;
            unknown.insert(runtime);
        }

        Ok(MaterialMap::build(self, registry, unknown))
    }

    fn ensure(&mut self, conn: &Connection, name: &str, numeric: u16) -> Result<(), IdMapError> {
        if self.by_name.contains_key(name) {
            return Ok(());
        }
        self.insert(conn, name, numeric)
    }

    fn insert(&mut self, conn: &Connection, name: &str, numeric: u16) -> Result<(), IdMapError> {
        conn.execute(
            "INSERT OR REPLACE INTO id_map (string_id, numeric_id) VALUES (?1, ?2)",
            rusqlite::params![name, i64::from(numeric)],
        )?;
        self.by_name.insert(name.to_owned(), numeric);
        self.by_id.insert(numeric, name.to_owned());
        Ok(())
    }
}

/// Bidirectional runtime ⇄ world id translation for one session.
///
/// Built by [`IdTable::reconcile`]. Both directions are direct lookups: this
/// sits in the encode and decode path of every chunk, and a map lookup per
/// palette entry per chunk is not somewhere to be clever.
#[derive(Debug, Clone, Default)]
pub struct MaterialMap {
    /// Indexed by runtime id. `u16::MAX` marks a runtime id with no world id,
    /// which cannot happen for anything reachable from a chunk but is
    /// representable.
    runtime_to_world: Vec<u16>,
    world_to_runtime: BTreeMap<u16, MaterialId>,
    unknown: BTreeSet<MaterialId>,
    /// Whether the two id spaces are the same space. See [`MaterialMap::passthrough`].
    passthrough: bool,
}

impl MaterialMap {
    const UNMAPPED: u16 = u16::MAX;

    /// A map for a reader that has no world database.
    ///
    /// World ids and runtime ids are the same number, and translation is the
    /// identity. This is not a shortcut — it is the correct model for a
    /// **client**, which receives chunk blobs over the wire and has no
    /// `id_map` table to reconcile against. The names behind those numbers
    /// arrive separately, in
    /// [`ServerMessage::MaterialTable`](crate::proto::ServerMessage::MaterialTable),
    /// because charter rule 8 makes the string id canonical and the number
    /// per-session.
    ///
    /// Deliberately **not** usable for writing a world: a passthrough map on
    /// the encode side would write a session's runtime ids into a database that
    /// means something else by them.
    #[must_use]
    pub const fn passthrough() -> Self {
        Self {
            runtime_to_world: Vec::new(),
            world_to_runtime: BTreeMap::new(),
            unknown: BTreeSet::new(),
            passthrough: true,
        }
    }

    fn build(table: &IdTable, registry: &Registry, unknown: BTreeSet<MaterialId>) -> Self {
        let mut runtime_to_world = vec![Self::UNMAPPED; registry.len()];
        let mut world_to_runtime = BTreeMap::new();

        for (runtime, name) in registry.iter() {
            if let Some(world) = table.id_of(name) {
                runtime_to_world[runtime.get() as usize] = world;
                world_to_runtime.insert(world, runtime);
            }
        }

        Self {
            runtime_to_world,
            world_to_runtime,
            unknown,
            passthrough: false,
        }
    }

    /// Runtime id → world id, for writing.
    ///
    /// # Errors
    ///
    /// [`IdMapError::UnmappedRuntimeId`] if the material was never reconciled,
    /// which means something registered a material after the world was opened.
    /// That is a lifecycle bug (charter rule 9 freezes registries before world
    /// load), so it is an error rather than a silent substitution.
    pub fn to_world(&self, runtime: MaterialId) -> Result<u16, IdMapError> {
        if self.passthrough {
            return Ok(runtime.get());
        }
        match self.runtime_to_world.get(runtime.get() as usize) {
            Some(&world) if world != Self::UNMAPPED => Ok(world),
            _ => Err(IdMapError::UnmappedRuntimeId { id: runtime.get() }),
        }
    }

    /// World id → runtime id, for reading.
    ///
    /// # Errors
    ///
    /// [`IdMapError::UnmappedWorldId`] if the blob references an id the world's
    /// own table does not contain — a corrupt or foreign chunk.
    pub fn to_runtime(&self, world: u16) -> Result<MaterialId, IdMapError> {
        if self.passthrough {
            return Ok(MaterialId(world));
        }
        self.world_to_runtime
            .get(&world)
            .copied()
            .ok_or(IdMapError::UnmappedWorldId { id: world })
    }

    /// Runtime ids that resolve to no loaded material this session.
    ///
    /// These behave as `engine:unknown` — no collision shape, no texture, no
    /// mod behaviour — but keep their identity so a save preserves them and
    /// re-adding the mod restores the content exactly.
    #[must_use]
    pub fn unknown(&self) -> &BTreeSet<MaterialId> {
        &self.unknown
    }

    /// Whether a runtime id is a placeholder for an absent mod's material.
    #[must_use]
    pub fn is_unknown(&self, id: MaterialId) -> bool {
        self.unknown.contains(&id)
    }
}

/// Something went wrong mapping material ids.
#[derive(Debug, thiserror::Error)]
pub enum IdMapError {
    /// A SQL failure.
    #[error("world database error while mapping material ids")]
    Sql(#[from] rusqlite::Error),

    /// A stored numeric id does not fit in `u16`.
    #[error("material `{name}` has stored numeric id {numeric}, which is out of range")]
    IdOutOfRange {
        /// The material's string id.
        name: String,
        /// The out-of-range value.
        numeric: i64,
    },

    /// The numeric id space is full.
    #[error("world material id space is exhausted")]
    Exhausted,

    /// An absent mod's material could not be aliased into the registry.
    #[error("could not alias absent material `{name}`")]
    Alias {
        /// The material's string id.
        name: String,
        /// Why registration failed.
        #[source]
        source: crate::material::RegistryError,
    },

    /// A runtime id has no world id.
    #[error("runtime material id {id} was never reconciled against this world")]
    UnmappedRuntimeId {
        /// The offending id.
        id: u16,
    },

    /// A world id has no runtime id.
    #[error("world material id {id} is not in this world's id table")]
    UnmappedWorldId {
        /// The offending id.
        id: u16,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::schema;

    fn world() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        schema::create(&conn).expect("schema");
        conn
    }

    #[test]
    fn reconcile_assigns_ids_and_persists_them() {
        let conn = world();
        let mut registry = Registry::new();
        registry.register("core:stone").expect("register");
        registry.register("core:dirt").expect("register");

        let mut table = IdTable::load(&conn).expect("load");
        table.reconcile(&conn, &mut registry).expect("reconcile");

        let reloaded = IdTable::load(&conn).expect("reload");
        assert_eq!(reloaded.id_of("core:stone"), table.id_of("core:stone"));
        assert_eq!(reloaded.len(), 4, "air, unknown, stone, dirt");
    }

    #[test]
    fn reserved_materials_keep_their_ids() {
        let conn = world();
        let mut registry = Registry::new();
        let mut table = IdTable::load(&conn).expect("load");
        table.reconcile(&conn, &mut registry).expect("reconcile");

        assert_eq!(table.id_of(crate::material::AIR_NAME), Some(0));
        assert_eq!(table.id_of(crate::material::UNKNOWN_NAME), Some(1));
    }

    #[test]
    fn existing_names_keep_their_ids_when_the_mod_set_changes() {
        // The property the whole design rests on: a world's numeric ids never
        // move, whatever happens to the mod list.
        let conn = world();

        let mut first = Registry::new();
        first.register("core:stone").expect("register");
        first.register("extra:gold").expect("register");
        let mut table = IdTable::load(&conn).expect("load");
        table.reconcile(&conn, &mut first).expect("reconcile");
        let stone_before = table.id_of("core:stone").expect("stone");
        let gold_before = table.id_of("extra:gold").expect("gold");

        // Second session: gold's mod is gone, and a new mod arrives. Registered
        // in a different order, too.
        let mut second = Registry::new();
        second.register("new:copper").expect("register");
        second.register("core:stone").expect("register");
        let mut table = IdTable::load(&conn).expect("reload");
        table.reconcile(&conn, &mut second).expect("reconcile");

        assert_eq!(table.id_of("core:stone"), Some(stone_before));
        assert_eq!(
            table.id_of("extra:gold"),
            Some(gold_before),
            "an absent mod's ids must be preserved, not reclaimed"
        );
        assert!(table.id_of("new:copper").is_some());
    }

    #[test]
    fn absent_materials_are_aliased_and_translate_both_ways() {
        let conn = world();

        let mut first = Registry::new();
        first.register("gone:thing").expect("register");
        let mut table = IdTable::load(&conn).expect("load");
        table.reconcile(&conn, &mut first).expect("reconcile");
        let world_id = table.id_of("gone:thing").expect("mapped");

        // Second session without that mod.
        let mut second = Registry::new();
        let mut table = IdTable::load(&conn).expect("reload");
        let map = table.reconcile(&conn, &mut second).expect("reconcile");

        let runtime = map.to_runtime(world_id).expect("still resolvable");
        assert!(
            map.is_unknown(runtime),
            "an absent mod's material must be flagged as unknown"
        );
        assert_eq!(
            map.to_world(runtime).expect("round trip"),
            world_id,
            "translating back must give the ORIGINAL world id, not the placeholder's"
        );
        assert_ne!(
            runtime,
            MaterialId::UNKNOWN,
            "aliasing onto the shared placeholder id would destroy the round trip"
        );
    }

    #[test]
    fn a_material_that_returns_resolves_normally_again() {
        let conn = world();
        let mut first = Registry::new();
        first.register("mod:thing").expect("register");
        let mut table = IdTable::load(&conn).expect("load");
        table.reconcile(&conn, &mut first).expect("reconcile");
        let world_id = table.id_of("mod:thing").expect("mapped");

        // Absent.
        let mut without = Registry::new();
        let mut table = IdTable::load(&conn).expect("reload");
        let map = table.reconcile(&conn, &mut without).expect("reconcile");
        assert!(map.is_unknown(map.to_runtime(world_id).expect("resolvable")));

        // Back again.
        let mut with = Registry::new();
        with.register("mod:thing").expect("register");
        let mut table = IdTable::load(&conn).expect("reload");
        let map = table.reconcile(&conn, &mut with).expect("reconcile");
        let runtime = map.to_runtime(world_id).expect("resolvable");
        assert!(!map.is_unknown(runtime), "the material is loaded again");
        assert_eq!(map.to_world(runtime).expect("round trip"), world_id);
    }

    #[test]
    fn air_translates_to_itself() {
        let conn = world();
        let mut registry = Registry::new();
        let mut table = IdTable::load(&conn).expect("load");
        let map = table.reconcile(&conn, &mut registry).expect("reconcile");
        assert_eq!(map.to_world(MaterialId::AIR).expect("air"), 0);
        assert_eq!(map.to_runtime(0).expect("air"), MaterialId::AIR);
    }

    #[test]
    fn an_unknown_world_id_is_an_error_not_a_substitution() {
        let conn = world();
        let mut registry = Registry::new();
        let mut table = IdTable::load(&conn).expect("load");
        let map = table.reconcile(&conn, &mut registry).expect("reconcile");
        assert!(matches!(
            map.to_runtime(9999),
            Err(IdMapError::UnmappedWorldId { .. })
        ));
    }

    #[test]
    fn a_passthrough_map_translates_every_id_to_itself() {
        // What a client uses: it has no `id_map` table to reconcile against,
        // and the numbers in a chunk blob are the only numbers it has. An id
        // it has never heard of must still decode — the names arrive in a
        // separate message, and a chunk that refused to decode until they did
        // would leave the world blank.
        let map = MaterialMap::passthrough();
        for id in [0u16, 1, 2, 9999, u16::MAX] {
            assert_eq!(map.to_runtime(id).expect("passthrough"), MaterialId(id));
            assert_eq!(map.to_world(MaterialId(id)).expect("passthrough"), id);
        }
        assert!(map.unknown().is_empty());
    }
}
