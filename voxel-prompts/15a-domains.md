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

## Acceptance criteria
- [A] All tests above green, including the failure-atomicity and unknown-domain cases.
- [A] `grep` shows no domain policy in core — no altitude thresholds, no travel rules, no
      named domains beyond `overworld` and whatever tests register.
- [A] Cross-domain interest leak test proves a player sees nothing from another domain.
- [A] Existing single-domain worlds from before this task open and play unchanged.
