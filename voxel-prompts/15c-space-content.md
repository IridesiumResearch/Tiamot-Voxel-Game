# TASK 15c — `core_space`: visitable stars, as a demonstration mod

Depends on: 15a, 15b, 10. Almost entirely Lua in `game/core_space/`, plus one deterministic
catalog function in core.

## What this task actually is
Every star in the sky being a real visitable place is **content**, not architecture. Under
charter rule 1 it belongs in a mod, and its job here is to be the hardest available proof
that the domain mechanism (15a) is complete and usable from the mod API. If building it
requires an engine change, that is a finding: fix the API in 15a and note it, do not
special-case core.

Treat this as an optional showcase. If the schedule is tight, 15a and 15b are the engine and
must ship; 15c can slip to the later default-mods phase without leaving anything unfinished.

## The one engine piece
`core::detgen::star_catalog(seed) -> Vec<StarRecord>` — formalises the function Task 10
already introduced for the skybox. Deterministic records:
`{ id, direction (as seen from the overworld), color, brightness, body_type, gen_params }`.
Capped catalog size (e.g. 2048). Sun and moon are catalog entries with reserved ids.

The catalog is in core rather than Lua for one reason: the sky renderer (Task 10) and the
mod must read the *same* records, or the sky and the map desync. That shared-source property
is the thing worth testing.

## The mod (`game/core_space/`, Lua)
- Registers `domain:space` (`kind = "sparse"`, kilo-yard scale) and one lazily-created
  `domain:body/<star_id>` voxel domain per visited body, via `game.register_domain`.
- Body worldgen from `gen_params`: sun = emissive white terrain (registers
  `core:starstuff`), moon = grey cratered variant, stars = colour variants. Small worlds.
- Transfer policy, entirely in Lua: flying above overworld build height + threshold →
  `game.transfer_entity` into `domain:space` at the mapped position; entering a body's
  approach radius → transfer into its body domain; leaving → back to space; approaching the
  overworld marker → home. Newtonian drift + thrust on existing input actions; the player is
  the ship (ships are a future mod's problem).
- Client rendering of a sparse domain: catalog-driven skybox plus nearby-body impostors
  (billboard → low-res mesh with approach). Uses 15b's LOD machinery for the approach
  transition; no voxels render in the sparse domain itself.

## Tests
- [A] No-desync property (the important one): the sky-render direction of star N equals the
  space-domain position direction of body N, asserted numerically from the shared catalog.
- [A] Catalog determinism: catalog hash in the cross-platform CI gate.
- [A] Round trip: bot flies up, crosses to space, reaches a body, lands on generated terrain,
  digs a block, returns to the overworld; restart the server; the body-domain edit persisted.
- [A] Zero voxel chunks exist for unvisited bodies (assert DB row counts — this is 15a's
  guarantee, verified through a real mod).
- [H] You do the same trip in the client, and it reads as travelling somewhere rather than a
  loading screen with extra steps.

## Acceptance criteria
- [A] Every transfer threshold and travel rule lives in `game/core_space` Lua. Grep the diff:
      no space, star, orbit, or altitude policy in `crates/`.
- [A] The no-desync assertion and catalog determinism gate are green.
- [A] The full round-trip test passes and the edit survives restart.
- [A] **The API-completeness finding is written down**: list every place the mod wanted an
      engine capability that did not exist. If the list is empty, say so — that is the
      strongest possible result for 15a. If it is not, each item is a 15a follow-up, not a
      core special case.
- [H] The trip feels like travel.
