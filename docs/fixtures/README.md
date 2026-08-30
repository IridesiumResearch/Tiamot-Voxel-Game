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
