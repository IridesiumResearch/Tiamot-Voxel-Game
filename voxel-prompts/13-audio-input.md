# TASK 13 — Audio (kira) + action-based input & rebinding

Depends on: 09, 12. Audio in `crates/client::audio` (event types in core::proto);
input in `crates/client::input` with registration API already stubbed in Task 05.

## Objective
Positional, mod-driven sound and a complete action/binding system. Mods never read keys;
mods never play sounds directly on clients they don't control — everything flows through
events.

## Design
### Audio
- `kira` backend. Mixer buses: master, effects, ambient, music, ui — volumes in settings,
  persisted.
- Sound registration (registration window): `game.register_sound{ id, file="break.ogg",
  gain, pitch_variance }` — ogg vorbis via the content pipeline (hashed, cached, pushed).
- OGG ingest is hostile input (charter rule 14): pure-Rust decode via **Symphonia** — it is
  actively maintained and covers ogg/vorbis without C codecs (do not use lewton, which is
  effectively unmaintained); caps before decode (file size, and duration/channel/sample-
  rate limits from stream headers, aborting past a decoded-bytes budget); decode fully on a
  worker with panic isolation — a poisoned sound is silently dropped with a per-server
  warning entry, never a crash or audio-thread stall. Add `fuzz/ogg_ingest` in THIS task,
  wired into the CI fuzz smoke job.
- Server-originated positional events: `game.play_sound{ sound, pos, radius, gain }` →
  broadcast to players in radius → client spatializes (distance attenuation + stereo pan;
  simple low-pass with distance; no HRTF). Attach-to-entity variant follows the entity.
- Client-local sounds (UI clicks, own-footsteps) playable from client-side scripts/engine
  directly.
- Wire defaults in `game/` mods: `register_block` gains `sounds = { break=..., place=...,
  step=... }`; core_tools plays dig-progress ticks and break/place; footsteps by material
  under the player (client-side, from own movement); milk gains pour/swim loops; a soft
  wind ambient loop in core_sky as the music-bus placeholder.
- Determinism note: audio is presentation-only; nothing in core simulation may depend on it.
### Input
- Actions: `game.register_action{ id, default = "KeyF", description }` (from Task 05, now
  live). Engine-reserved actions (move/look/jump/sneak/sprint/dig/place/hotbar1-9/menu) are
  pre-registered by the engine with the same machinery — one system, no special cases.
- Binding layer: physical input (keyboard scancodes, mouse buttons/axes) → action mapping;
  conflict detection with clear UI warning; chords not required; per-device sensitivity for
  look. Bindings persisted in the client config dir as TOML keyed by ACTION id, so mod
  updates don't scramle user bindings.
- Rebinding UI: settings screen (egui) listing engine + mod actions grouped by mod, click-to-
  rebind with capture, reset-to-default per action and globally.
- Delivery: client-side scripts receive `on_action(id, pressed)` for their registered
  actions; server mods receive action events for actions they registered via the input
  message stream (rate-limited server-side; document limits). Bots inject actions by id —
  update `crates/bot` so bot input and human input converge on the same path.

## Tests
- Audio: event → in-radius delivery integration test (bot asserts receipt of sound events;
  actual DSP not asserted); out-of-radius bot receives nothing; entity-attached sound follows.
- Content pipeline: ogg push/cache round-trip by hash.
- Input unit: binding persistence round-trip; conflict detection; unknown-action events from
  a hostile client rejected; rate limiting enforced.
- Integration: rebind dig to another key via the settings flow driven headlessly (factor the
  binding model so it's testable without the UI), bot uses action-id path throughout the
  Task 07/09 scripts unchanged.
- [H] Manual checklist: spatialization pan/attenuation sanity; no audio thread underruns
  during chunk-load hitches (kira runs its own thread — decoupling is [A] testable; the
  listening check is yours).

## Acceptance criteria
- [A] Sound EVENTS are delivered positionally to nearby players only (bot-asserted);
      footstep/splash event selection by material verified in tests.
- [H] It actually sounds right: positional pan/attenuation, footstep variety, milk splashes.
- [A] Every sound and every binding is attributable to a mod in the UI.
- [A] A mod-registered action ("core_tools:chisel_mode") is rebindable in settings and works
      end to end.
- [A] All prior bot scripts still pass on the converged action pathway.
