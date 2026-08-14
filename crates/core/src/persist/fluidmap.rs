// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Stable world ids for fluids, and the translation to this session's.
//!
//! # Why this exists
//!
//! Charter rule 8: string ids are canonical, numeric ids are per session, and
//! the world database owns the mapping between them. [`super::idmap`] does that
//! for materials. Fluid needs its own because the two id spaces are not the same
//! shape: a material id is a `u16` and a fluid id is **four bits**, so a fluid
//! cannot borrow the material table's numbering — id 500 does not fit in a
//! [`crate::fluid::Fluid`] byte.
//!
//! Without this, a saved pond carried whatever number the running session
//! happened to give its fluid. [`crate::fluid::Fluids::register`] numbers
//! positionally in registration order, so the stored id was correct only for as
//! long as the mod set and its load order were: add a mod that registers a fluid
//! ahead of an existing one and every pond in the world becomes a different
//! fluid, silently, because the byte is still perfectly valid.
//!
//! # Fifteen ids, shared
//!
//! Four bits with zero reserved for "no fluid" leaves fifteen, and **the world's
//! history shares them with the current session**. A world that has at some
//! point held fifteen distinct fluids has no id left for a sixteenth, whatever
//! is registered now. That is a real limit rather than an implementation
//! detail — it is the price of a one-byte fluid — and reconciliation reports it
//! rather than quietly reusing an id somebody's chunks still refer to.

use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::fluid::{FluidId, Fluids, MAX_FLUIDS};

/// A fluid id mapping could not be established.
#[derive(Debug, thiserror::Error)]
pub enum FluidMapError {
    /// A SQL failure.
    #[error("fluid id table")]
    Sql(#[from] rusqlite::Error),

    /// A stored id is not a value a fluid byte can hold.
    #[error("the world stores fluid `{name}` as id {numeric}, which is outside 1..={MAX_FLUIDS}")]
    IdOutOfRange {
        /// The fluid's string id.
        name: String,
        /// What was stored.
        numeric: i64,
    },

    /// All fifteen ids are spoken for.
    #[error(
        "this world has already used all {MAX_FLUIDS} fluid ids, so `{name}` cannot be given \
         one. Fluid ids are four bits and a world's history shares them with the current mod \
         set; an id already in use cannot be reassigned without changing what stored chunks \
         mean."
    )]
    Exhausted {
        /// The fluid that could not be given an id.
        name: String,
    },

    /// A stand-in for an absent mod's fluid could not be registered.
    #[error("could not register a placeholder for the world's fluid `{name}`")]
    Placeholder {
        /// The fluid's string id.
        name: String,
        /// Why.
        #[source]
        source: crate::fluid::RegisterError,
    },
}

/// The world's fluid name ⇄ world id table, as stored.
#[derive(Debug, Clone, Default)]
pub struct FluidIdTable {
    by_name: BTreeMap<String, u8>,
    by_id: BTreeMap<u8, String>,
}

impl FluidIdTable {
    /// Reads the table from an open world database.
    ///
    /// # Errors
    ///
    /// Any SQL failure, or a stored id a fluid byte could not hold.
    pub fn load(conn: &Connection) -> Result<Self, FluidMapError> {
        let mut table = Self::default();
        let mut statement = conn.prepare("SELECT string_id, numeric_id FROM fluid_ids")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;

        for row in rows {
            let (name, numeric) = row?;
            // Checked rather than truncated. Everything on disk is untrusted
            // (see the `persist` module docs), and a hand-edited 300 here would
            // otherwise become id 44 and rename somebody's lake.
            let numeric = u8::try_from(numeric)
                .ok()
                .filter(|id| (1..=MAX_FLUIDS as u8).contains(id))
                .ok_or(FluidMapError::IdOutOfRange {
                    name: name.clone(),
                    numeric,
                })?;
            table.by_name.insert(name.clone(), numeric);
            table.by_id.insert(numeric, name);
        }
        Ok(table)
    }

    /// How many fluids this world has ids for.
    #[must_use]
    pub fn len(&self) -> usize {
        self.by_name.len()
    }

    /// Whether the world has never stored a fluid.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Lowest unused id, or `None` if all fifteen are taken.
    fn next_free(&self) -> Option<u8> {
        // From one, because zero means "no fluid" and is not an id.
        (1..=MAX_FLUIDS as u8).find(|candidate| !self.by_id.contains_key(candidate))
    }

    /// Reconciles the world's table against the fluids a session registered.
    ///
    /// - Names already in the table keep their world ids, always. That is the
    ///   whole point: a stored byte must not change meaning.
    /// - Names registered this session but absent from the table get the lowest
    ///   free id, written back immediately.
    /// - **Names in the table that no mod registered are never removed.** They
    ///   are registered into `fluids` as inert placeholders so the bytes
    ///   referring to them round-trip — see
    ///   [`Fluids::register_placeholder`].
    ///
    /// # Errors
    ///
    /// [`FluidMapError`] on a SQL failure, on exhaustion of the fifteen ids, or
    /// if a placeholder cannot be registered.
    pub fn reconcile(
        &mut self,
        conn: &Connection,
        fluids: &mut Fluids,
    ) -> Result<FluidMap, FluidMapError> {
        // Registered names the world has not seen before.
        let registered: Vec<String> = fluids.iter().map(|(_, fluid)| fluid.name.clone()).collect();
        for name in &registered {
            if self.by_name.contains_key(name.as_str()) {
                continue;
            }
            let numeric = self
                .next_free()
                .ok_or_else(|| FluidMapError::Exhausted { name: name.clone() })?;
            self.insert(conn, name, numeric)?;
        }

        // Names the world knows that this session does not. Registering them
        // keeps the round-trip exact.
        let orphans: Vec<String> = self
            .by_name
            .keys()
            .filter(|name| fluids.id_of(name).is_none())
            .cloned()
            .collect();
        for name in orphans {
            fluids
                .register_placeholder(&name)
                .map_err(|source| FluidMapError::Placeholder {
                    name: name.clone(),
                    source,
                })?;
        }

        Ok(FluidMap::build(self, fluids))
    }

    fn insert(&mut self, conn: &Connection, name: &str, numeric: u8) -> Result<(), FluidMapError> {
        conn.execute(
            "INSERT OR REPLACE INTO fluid_ids (string_id, numeric_id) VALUES (?1, ?2)",
            rusqlite::params![name, i64::from(numeric)],
        )?;
        self.by_name.insert(name.to_owned(), numeric);
        self.by_id.insert(numeric, name.to_owned());
        Ok(())
    }
}

/// Bidirectional session ⇄ world fluid id translation.
///
/// Built by [`FluidIdTable::reconcile`], and sits in the encode and decode path
/// of every stored fluid layer — so both directions are a flat array index
/// rather than a map lookup. Sixteen bytes each way.
#[derive(Debug, Clone)]
pub struct FluidMap {
    /// Indexed by session id; zero marks "no world id", which cannot happen
    /// after a reconcile and is a bug rather than a state.
    to_world: [u8; MAX_FLUIDS + 1],
    /// Indexed by world id, the other way.
    to_session: [u8; MAX_FLUIDS + 1],
}

impl Default for FluidMap {
    fn default() -> Self {
        Self::identity()
    }
}

impl FluidMap {
    /// A map that changes nothing.
    ///
    /// For a world with no fluid at all, and for callers that hold no database
    /// — the client, whose ids come off the wire already in session terms.
    #[must_use]
    pub const fn identity() -> Self {
        let mut table = [0u8; MAX_FLUIDS + 1];
        let mut id = 0;
        while id <= MAX_FLUIDS {
            table[id] = id as u8;
            id += 1;
        }
        Self {
            to_world: table,
            to_session: table,
        }
    }

    fn build(table: &FluidIdTable, fluids: &Fluids) -> Self {
        let mut to_world = [0u8; MAX_FLUIDS + 1];
        let mut to_session = [0u8; MAX_FLUIDS + 1];
        for (session, fluid) in fluids.iter() {
            let Some(&world) = table.by_name.get(&fluid.name) else {
                // Every registered fluid was just given an id, and every world
                // fluid was just registered, so this cannot happen. Leaving the
                // entry zero makes the translation report it rather than
                // inventing a mapping.
                continue;
            };
            to_world[session.0 as usize] = world;
            to_session[world as usize] = session.0;
        }
        Self {
            to_world,
            to_session,
        }
    }

    /// The world id a session id is stored as.
    ///
    /// # Errors
    ///
    /// [`UnmappedFluid`] for a session id this world has no id for, which after
    /// a reconcile means the caller invented an id.
    pub fn to_world(&self, session: FluidId) -> Result<u8, UnmappedFluid> {
        if session.is_none() {
            return Ok(0);
        }
        match self.to_world.get(session.0 as usize).copied() {
            Some(0) | None => Err(UnmappedFluid {
                id: session.0,
                direction: "session",
            }),
            Some(world) => Ok(world),
        }
    }

    /// The session id a stored world id means.
    ///
    /// # Errors
    ///
    /// [`UnmappedFluid`] for a stored id the world's own table does not list —
    /// a corrupt or hand-edited row, since reconciliation gives every listed
    /// name a session id even when its mod is gone.
    pub fn to_session(&self, world: u8) -> Result<FluidId, UnmappedFluid> {
        if world == 0 {
            return Ok(FluidId::NONE);
        }
        match self.to_session.get(world as usize).copied() {
            Some(0) | None => Err(UnmappedFluid {
                id: world,
                direction: "world",
            }),
            Some(session) => Ok(FluidId(session)),
        }
    }
}

/// A fluid id with no counterpart in the other id space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{direction} fluid id {id} has no counterpart in this world's fluid table")]
pub struct UnmappedFluid {
    /// The id that could not be translated.
    pub id: u8,
    /// Which space it came from.
    pub direction: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fluid::Registered;
    use crate::material::MaterialId;

    fn fluid(name: &str) -> Registered {
        Registered {
            name: name.to_owned(),
            flow_range: 7,
            waterlogs_at: 14,
            tick_rate: 1,
            renews_from: 0,
            color: [255, 255, 255],
            material: MaterialId(4),
        }
    }

    fn database() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        crate::persist::schema::create(&conn).expect("schema");
        conn
    }

    #[test]
    fn a_world_id_survives_a_change_of_registration_order() {
        // **The whole reason this module exists.** `Fluids::register` numbers
        // positionally, so loading a mod ahead of an existing one renumbers
        // everything after it — and before this, that renumbering went straight
        // to disk and every stored pond became a different fluid.
        let conn = database();

        let mut first = Fluids::new();
        first.register(fluid("core_milk:milk")).expect("register");
        let mut table = FluidIdTable::load(&conn).expect("load");
        let map = table.reconcile(&conn, &mut first).expect("reconcile");
        let milk_was = map
            .to_world(first.id_of("core_milk:milk").expect("registered"))
            .expect("mapped");

        // A second session where another mod loads first and takes id 1.
        let mut second = Fluids::new();
        second.register(fluid("other:acid")).expect("register");
        second.register(fluid("core_milk:milk")).expect("register");
        assert_eq!(
            second.id_of("core_milk:milk"),
            Some(FluidId(2)),
            "the staging is wrong: milk was supposed to be renumbered"
        );

        let mut table = FluidIdTable::load(&conn).expect("load");
        let map = table.reconcile(&conn, &mut second).expect("reconcile");
        let milk_now = map
            .to_world(second.id_of("core_milk:milk").expect("registered"))
            .expect("mapped");

        assert_eq!(
            milk_now, milk_was,
            "milk's WORLD id moved when another mod loaded ahead of it, so every \
             stored pond would have changed fluid"
        );
        // And the stored byte still reads back as milk in the new session.
        assert_eq!(
            map.to_session(milk_was).expect("mapped"),
            second.id_of("core_milk:milk").expect("registered")
        );
    }

    #[test]
    fn a_fluid_whose_mod_is_gone_keeps_its_id_and_its_bytes() {
        // Charter rule 8's round trip. A world that has held a fluid must be
        // able to read its own chunks after that mod is removed, or disabling a
        // mod deletes somebody's lake.
        let conn = database();

        let mut with_mod = Fluids::new();
        with_mod.register(fluid("other:acid")).expect("register");
        let mut table = FluidIdTable::load(&conn).expect("load");
        let map = table.reconcile(&conn, &mut with_mod).expect("reconcile");
        let acid_world = map
            .to_world(with_mod.id_of("other:acid").expect("registered"))
            .expect("mapped");

        // The mod is gone. Nothing registers acid.
        let mut without = Fluids::new();
        without.register(fluid("core_milk:milk")).expect("register");
        let mut table = FluidIdTable::load(&conn).expect("load");
        let map = table.reconcile(&conn, &mut without).expect("reconcile");

        let stood_in = map
            .to_session(acid_world)
            .expect("a stored id must still resolve when its mod is gone");
        assert!(
            without.is_placeholder(stood_in),
            "acid came back as a real fluid rather than a stand-in"
        );
        assert_eq!(
            map.to_world(stood_in).expect("mapped"),
            acid_world,
            "the stand-in does not write back the id it read, so the round trip is lossy"
        );

        // And it is inert: it spreads nowhere and is not offered to clients.
        let entry = without.get(stood_in).expect("registered");
        assert_eq!(entry.flow_range, 0);
        assert_eq!(entry.material, MaterialId::AIR);
        assert!(
            without
                .iter_registered()
                .all(|(id, _)| !without.is_placeholder(id)),
            "a placeholder leaked into the fluids a mod actually registered"
        );
    }

    #[test]
    fn a_placeholder_does_not_take_the_first_fluids_place() {
        // **The interaction that made this more than a storage change.** The
        // server takes its solver tuning from the FIRST registered fluid, so a
        // stand-in registered ahead of milk would silently replace milk's flow
        // rules with an inert fluid's. `iter_registered` is what the server
        // filters through.
        let conn = database();

        let mut first = Fluids::new();
        first.register(fluid("other:acid")).expect("register");
        let mut table = FluidIdTable::load(&conn).expect("load");
        table.reconcile(&conn, &mut first).expect("reconcile");

        let mut second = Fluids::new();
        second.register(fluid("core_milk:milk")).expect("register");
        let mut table = FluidIdTable::load(&conn).expect("load");
        table.reconcile(&conn, &mut second).expect("reconcile");

        let (_, leading) = second
            .iter_registered()
            .next()
            .expect("milk is still registered");
        assert_eq!(
            leading.name, "core_milk:milk",
            "a stand-in for an absent mod became the fluid the solver takes its \
             tuning from"
        );
    }

    #[test]
    fn ids_are_handed_out_from_one_and_reused_when_free() {
        let conn = database();
        let mut fluids = Fluids::new();
        for name in ["a:one", "b:two", "c:three"] {
            fluids.register(fluid(name)).expect("register");
        }
        let mut table = FluidIdTable::load(&conn).expect("load");
        let map = table.reconcile(&conn, &mut fluids).expect("reconcile");

        // Zero is never handed out — it is "no fluid".
        for (session, _) in fluids.iter() {
            let world = map.to_world(session).expect("mapped");
            assert!((1..=MAX_FLUIDS as u8).contains(&world), "id {world}");
        }
        assert_eq!(table.len(), 3);
    }

    #[test]
    fn a_world_that_has_used_every_id_says_so_rather_than_reusing_one() {
        // Reusing an id would change what stored chunks mean, which is the one
        // thing this table exists to prevent. A loud error is the only honest
        // answer.
        let conn = database();
        let mut fluids = Fluids::new();
        for index in 0..MAX_FLUIDS {
            fluids
                .register(fluid(&format!("full:fluid{index}")))
                .expect("register");
        }
        let mut table = FluidIdTable::load(&conn).expect("load");
        table.reconcile(&conn, &mut fluids).expect("reconcile");
        assert_eq!(table.len(), MAX_FLUIDS);

        // A later session brings one more.
        let mut newcomer = Fluids::new();
        newcomer.register(fluid("late:arrival")).expect("register");
        let mut table = FluidIdTable::load(&conn).expect("load");
        assert!(matches!(
            table.reconcile(&conn, &mut newcomer),
            Err(FluidMapError::Exhausted { .. })
        ));
    }

    #[test]
    fn a_stored_id_outside_the_four_bits_is_refused_rather_than_truncated() {
        // Everything on disk is untrusted. A hand-edited 300 truncates to 44,
        // which is not even a valid id — and a silent truncation to something
        // that IS valid would rename a lake.
        let conn = database();
        conn.execute(
            "INSERT INTO fluid_ids (string_id, numeric_id) VALUES (?1, ?2)",
            rusqlite::params!["bad:fluid", 300],
        )
        .expect("insert");

        assert!(matches!(
            FluidIdTable::load(&conn),
            Err(FluidMapError::IdOutOfRange { .. })
        ));
    }

    #[test]
    fn the_identity_map_changes_nothing() {
        // What a client uses: its ids arrive already in session terms, so there
        // is nothing to translate and nothing to get wrong.
        let map = FluidMap::identity();
        for id in 1..=MAX_FLUIDS as u8 {
            assert_eq!(map.to_world(FluidId(id)).expect("mapped"), id);
            assert_eq!(map.to_session(id).expect("mapped"), FluidId(id));
        }
        assert_eq!(map.to_world(FluidId::NONE).expect("none"), 0);
        assert_eq!(map.to_session(0).expect("none"), FluidId::NONE);
    }
}
