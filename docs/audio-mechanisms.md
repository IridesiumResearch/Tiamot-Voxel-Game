<!-- SPDX-FileCopyrightText: Iridesium -->
<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Audio — what the API can express, and what it cannot yet

**Status: a record of gaps, not a plan.** Written 2026-08-20, at the end of
Task 13, from four things a future game will want — mob noises, wind in wooded
country, crickets in fields, and music. One of the four is already expressible.
The other three are not, and this says exactly what is missing and why the
obvious workaround does not do.

Nothing here is scheduled. Tasks 14–16 are UI, domains, chunk LOD, space content
and polish, and none of them touches audio. This exists so the reasoning is not
re-derived from scratch whenever it is picked up.

Charter rule 1 governs all of it: the engine holds mechanisms, mods hold
content. Every gap below is phrased as *a mod cannot express X*, never as *the
game needs X*.

---

## The exhibit

`client::audio::Bus` has four variants. `Ambient`'s doc comment reads:

> Looping background noise — wind, a river.

That bus exists, it has a volume slider on the settings screen, the player can
set it — and **no mod can put a sound on it**, because `register_sound` has no
`bus` field and everything a mod plays is hard-coded to `Effects`. There is also
no looping sound of any kind. The bus is named for a feature that does not
exist.

That is the shape of this whole document.

---

## 1. Mob noises — expressible today

```lua
game.play_sound{ sound = "growl", entity = mob_id, radius = 24 }
```

The sound is placed at the entity's **interpolated** position on each client
rather than the server's last known one, so it moves with what the player can
see. Only players inside `radius` are sent it at all. `core_mimic` is the
working reference.

No engine work. This one is done.

## 2. Sustained sound — the missing primitive

**Blocks:** wind, crickets, a river, a machine hum, an engine — anything held
rather than struck.

Every sound today is one-shot. `game.play_sound` returns *how many players were
told* and nothing else: there is no handle, so there is nothing to stop, fade,
or change while it plays.

**Why re-triggering on a timer is not the workaround.** It drifts, it cannot
crossfade, and it makes the mod guess a file's duration — which the mod cannot
read. The seam between repeats is audible, and it is audible in exactly the
sounds this is for, because a loop that clicks once a second is worse than
silence.

**The mechanism:** a play call that returns a handle, and operations on it —
stop, fade, set gain. That handle has to survive on both sides: the server knows
it started a sound, the client owns the voice actually playing. A mod that
crashes or unloads must not leave a sound running for ever, so a handle has to
be owned by something that ends — the entity, the player's session, the mod
itself — and be cleaned up when that does. It is the OPPOSITE of
`game.storage`, which is deliberately persistent and survives restarts: a held
sound is session state and must never outlive the thing holding it.

## 3. A bus a mod can name

**Blocks:** the player's Ambient and Music sliders meaning anything.

`register_sound` should take `bus`, defaulting to `effects`. The reason it does
not already is recorded in `App::play_heard` and is a good one — the field lands
when a mod wants it, rather than being guessed at first — and a mod wanting wind
and music is that moment arriving.

**Not merely cosmetic.** A player who turns music down and hears crickets stop
has been told a lie by the interface.

## 4. Streaming, for anything longer than a minute

**Blocks:** music.

`Limits` caps a sound at 4 MiB and at one minute of 48 kHz audio, and the whole
file is decoded into memory before anything plays. That is correct for effects
and wrong for a track: a three-minute stereo piece is tens of megabytes of `f32`
resident for something played once.

**The mechanism:** a second path that decodes as it plays, with its own limits —
and it stays inside charter rule 14. A streamed file is still bytes from a
server nobody trusts, so it wants the same pre-decode caps, the same panic
isolation, and its own fuzz target. **Streaming makes rule 14 harder, not
easier**: a decoder fed in slices has more states than one fed a whole file, and
the fuzz target has to drive those slices rather than hand over a buffer.

## 5. Ambience belongs on the client, which needs client scripts

**Blocks:** wind and crickets specifically, and this is the deep one.

Wind depends on what is around *you*. Modelled as server-broadcast sounds it
costs 50 players × every emitter in earshot, every repeat, for something with no
event behind it and no consequence for anyone else — and it puts a network round
trip in front of a noise the client already had everything it needed to make.

Compare footsteps, which Task 13 already settled this way: a player's own steps
are played client-side from the client's own movement, because the round trip is
audible. Ambience is the same argument at larger scale.

So the mechanism is **a mod script running on the client**, deciding from local
terrain what it should sound like. That is charter rule 10's client scripts —
"pushed by servers: hard sandbox — no fs, no net, no `os`/`io`/binary `load`,
instruction + memory caps" — and **nothing implements them yet**. It is a large
piece of work that audio merely happens to want, and it unlocks considerably
more than audio.

## 6. There are no biomes, and that is deliberate

"Wind in tree biomes" has no engine-side referent. `crates/core/src/detgen`
provides seeded noise and nothing else, and
`determinism.rs`'s `detgen_contains_no_terrain_policy` reads every source file
in `src/detgen` and fails if any of `grass dirt stone sand water biome tree ore
cave terrain` appears in one — a test whose whole purpose is to keep content out
of `core`, "because this is exactly the kind of thing that arrives one
convenient helper at a time".

A mod that wants wind in wooded country answers "is this wooded" from its own
worldgen data. The engine's job is to make the *sound* expressible, and to let a
mod ask what is around a point. Nothing here should add a biome concept to the
engine.

---

## Order, if it is ever picked up

Roughly dependency order rather than value order:

1. **A `bus` field.** Small, additive, no protocol shape change beyond a field.
2. **Sustained sounds with handles.** The missing primitive; wind and crickets
   are unbuildable without it, however they are triggered.
3. **Streaming.** Independent of the other two, and carries its own rule 14
   work — a fuzz target in the same task, per the charter.
4. **Client scripts.** Much larger than audio, wanted by much more than audio,
   and the thing that makes ambience *right* rather than merely possible.

1–3 are useful on their own; 4 is a project.

**When.** After the 18, on the argument the charter already makes: the engine is
deliberately small, and the requirements for ambience are the kind you learn by
building a real game on top rather than by predicting. Points 1 and 2 are cheap
enough to pull forward if a mod is blocked on them sooner.

## See also

- [`float-determinism.md`](float-determinism.md) — the Audio section, and why a
  one-way sound API keeps every float here outside charter rule 4.
- [`performance-targets.md`](performance-targets.md) — the 50 ms tick shared by
  all simulation for all players, which is what point 5's traffic argument is
  measured against.
- `api/stubs/game.lua` — `register_sound` and `play_sound` as they stand,
  including the formats and limits.
