# Fixtures you copy in by hand

Mods here are **not loaded**. `game/` is what the client and the server read, and
these are deliberately outside it — a fixture that shipped in `game/` would
change the mod set every test and every run loads, which is a large blast radius
for something meant to be looked at once and deleted.

To use one:

```console
cp -r docs/fixtures/places game/places
```

and delete it again when you are finished.

## `places` — four simulation spaces (Task 15b's human gate)

Registers `places:attic` (its own floor), `places:ship` (an instanced template
with a hull), `places:space` (sparse — entities only) and `places:void` (no
generator, so genuinely empty). Say `attic`, `ship`, `space`, `void` or `home`
in chat to be moved.

**What it is for.** Task 15a's one [H] criterion: whether a domain switch reads
as a pause rather than a fault. The client throws its whole world away on a
switch, and for a moment the player is standing in nothing — nothing looks
exactly like a server that has stopped answering, so there is an overlay saying
where you are going.

Watch that it appears the instant the world goes, names the space, and clears as
the new ground draws rather than blinking or lingering.

`void` is the case worth seeing on purpose: with no generator there is no
terrain to arrive, so you fall for ever and **the overlay never clears**. That
is correct and it is where "waiting" and "broken" look most alike — it is what a
mod author will hit the first time they register a domain and forget its
generator. If it reads as a hang, that is worth saying.

## `relief` — a domain with hills (Task 15b's human gate)

Registers `relief:hills`, one domain of fractal-noise terrain. Say `hills` in
chat to go, `home` to come back.

**What it is for.** Task 15b's T6 asks whether the resolution change reads as
detail rather than a pop, and A3 asks for a frame rate at 32 chunks of view.
Neither can be answered in the reference world: `core_worldgen` is a constant
surface at y = 0 on purpose — terrain is content and content is a later phase —
and a flat plain is the one world in which a level-1 summary and a level-3
summary are identical. There is nothing to see the LOD do.

It is a DOMAIN rather than a replacement generator so that nothing else in the
mod set can tell it is loaded: `core_worldgen` keeps the overworld's
`register_on_generate`, and this composes with `places` instead of fighting it
for the same callback. That it also exercises the per-domain summary cache is a
bonus rather than the reason.

Spawns you at y = 96, which is above the highest peak the shape can reach, so
you arrive in open air and fall. Tune `SHAPE` in `init.lua` for bigger or
smaller country.
