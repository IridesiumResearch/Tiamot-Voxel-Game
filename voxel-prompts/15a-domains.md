# TASK 15a — Domains: multiple simulation spaces in one world

Depends on: 12. All engine work, in `crates/core`. **No content in this task.**

## Why this is its own task
The original plan bundled domains, chunk LOD, and a space-travel demo into one session. They
are three unrelated pieces of work: a persistence/replication generalisation, a rendering
system, and a Lua mod. Bundling them meant the engine mechanism could not be reviewed,
tested, or shipped without also building a solar system. They are now 15a, 15b, and 15c, and
each stands alone.

Task 03 already reserved the `domain` column in `chunks` and `entities`, so this is a feature
change, not a schema retrofit — no live-format migration is needed, and the migration chain
is already proven by Task 03's synthetic v0→v1 test.

## What this has to leave room for
The nearest real use, agreed 2026-08-28: **a spaceship whose interior is its own domain.**
Standing inside it you build normally and cannot touch the world outside — which is not a
rule a mod enforces but a consequence of interest being domain-scoped — and the ship is
stationary in its own frame while an entity carrying its id moves around the overworld. No
counter-motion, no moving voxels: the two frames never talk.

Three consequences to design with eyes open, none of them this task's job to solve:
windows show nothing (seeing another domain means rendering two at once), entering is a
handoff with a loading state rather than a walk, and nothing crosses the boundary unless a
mod carries it.

## Objective
A world contains multiple named simulation domains with independent coordinate frames, chunk
stores, and scales. Entities and players live in exactly one domain. The engine provides the
mechanism for moving between them; mods decide when that happens.

## Design
- `DomainId` (string id, `engine:` and mod namespaces as elsewhere). A world has
  `domain:overworld` by default. `game.register_domain{ id, kind, scale, generator }` during
  the registration window:
  - `kind = "voxel"` — a normal chunked voxel space with its own worldgen callback.
  - `kind = "sparse"` — no voxels, entities only, with its own unit scale (this is what a
    space-like domain would use; the engine does not know or care what it represents).
- **Domains a mod could not name at load.** `register_domain` at the registration window is
  right for a fixed set — `overworld`, a space domain, a star catalogue's bodies — and wrong
  for anything a player makes. A voxel ship whose interior is its own domain is the case:
  the fifty-first ship somebody welds together needs a fifty-first domain, and no mod could
  have named it while the registry was open. So:
  - `register_domain{ ..., instanced = true }` declares a TEMPLATE rather than a domain. No
    domain exists under that id and nothing is stored for it.
  - `game.create_domain(template, key)` at runtime returns the instance's id — `template/key`
    — creating it if absent and returning the existing one if not. Creating one twice is not
    an error and must not empty it: a mod re-entering a ship it already made will call this
    every time, and "create" that wiped would be a ship that emptied on the second visit.
  - `game.destroy_domain(id)` removes an instance and its chunks. **Refused while anything is
    in it**, entity or player, and returns whether it happened — taking a room out from under
    somebody is the same defect as breaking a container they have open.
  - Which instances exist is persisted, so they survive a restart. An instance whose template
    is no longer registered is preserved rather than dropped, exactly as an unknown domain is.
- Persistence: chunk and entity accessors already take a domain (Task 03). Add domain
  registration to `meta` so an unknown domain in the DB is preserved rather than dropped,
  mirroring the `engine:unknown` material rule (charter rule 8). Lazily-instantiated domains
  cost zero storage until first written.
- Replication: chunk interest, entity interest, and delta broadcast are all domain-scoped.
  A player in domain A receives nothing from domain B. Audit every interest set added in
  Tasks 06 and 12 and confirm the scoping — this is where cross-domain leaks would hide.
- Transfers: `game.transfer_entity(entity, domain, pos)` — atomic server-side handoff. Tear
  down chunk interest in the old domain, move the entity record, rebuild interest, notify the
  client with a domain-switch message carrying a loading state. Must be safe mid-tick and
  must not lose the entity if the target domain fails to generate.
- Hooks: `on_domain_enter(entity, domain)`, `on_domain_exit(entity, domain)`, both able to
  veto by returning false (a mod can refuse a transfer).
- Client: handle the domain-switch message — flush the chunk store, show a loading state,
  rebuild from the new domain's stream. The renderer needs to tolerate an empty voxel set
  (a sparse domain renders only entities and whatever skybox a mod supplies).
- Physics/lighting/fluid: all already operate per-chunk; verify none of them hold global
  state that would bleed across domains. Where they do, fix it here.

## Tests
- [A] Two-domain world: bot transfers between them; assert interest teardown/rebuild, that
  the bot receives no chunk or entity data from the domain it left, and that its inventory
  and identity survive the move.
- [A] Persistence: edits in a second domain round-trip through restart, stored under the
  correct domain key; overworld untouched.
- [A] Zero-cost invariant: a registered but never-visited domain has zero chunk rows
  (assert row counts).
- [A] Unknown-domain preservation: a DB containing a domain no longer registered loads, is
  not dropped, and re-registering it restores access.
- [A] Veto: `on_domain_exit` returning false blocks the transfer, entity state unchanged.
- [A] Failure atomicity: a transfer whose target generation errors leaves the entity intact
  in its original domain (fault-inject the generator).
- [A] Sparse-domain sanity: a `kind = "sparse"` domain accepts entities, rejects chunk
  writes with a clear error, and replicates entity deltas normally.
- [A] Runtime instances: a mod creates one from a template mid-session, a bot transfers in,
  builds, and the edit round-trips through a restart under the instance's own domain key.
- [A] Creating an instance twice returns the same domain and does not empty it — the case a
  mod hits every time somebody re-enters their ship.
- [A] Destroying one removes its chunk rows, leaves its siblings untouched, and is REFUSED
  while a player or entity is inside it.
- [A] A runtime instance is interest-scoped like any other: a player inside receives nothing
  from the overworld, and a player outside receives nothing from it.

## Acceptance criteria
- [A] All tests above green, including the failure-atomicity and unknown-domain cases.
- [A] `grep` shows no domain policy in core — no altitude thresholds, no travel rules, no
      named domains beyond `overworld` and whatever tests register.
- [A] Cross-domain interest leak test proves a player sees nothing from another domain.
- [A] Existing single-domain worlds from before this task open and play unchanged.
- [A] A domain instance created at runtime is indistinguishable from a registered one to
      everything downstream — persistence, interest, transfers and hooks.
