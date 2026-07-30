# TASK 06 — Networking, identity, server tick loop, embedded server, content push

Depends on: 03, 05. Protocol types in `crates/core::proto`; loop in `crates/server` plus a
reusable `ServerHandle` in core so the client can embed it.

## Objective
A running authoritative server: QUIC transport, recoverable cryptographic identity, fixed
tick, join flow, chunk streaming, mod-manifest + content push. Singleplayer is this server
embedded over loopback.

## Design
- Transport: `quinn` (QUIC) — mature, widely used for exactly this, and it hands us
  reliability, encryption, congestion control and stream multiplexing rather than making us
  build a UDP reliability layer. (`s2n-quic` is the fallback if quinn ever stalls; keep the
  transport behind a thin interface so that stays a possibility, but do not abstract
  speculatively.) Channels: reliable-ordered control stream, reliable chunk-data stream,
  unreliable datagrams for state deltas. Self-signed cert generated per server with
  fingerprint surfaced to clients (TOFU model; document).
- Messages: `postcard` enums with a protocol version const. Version mismatch ⇒ clean reject
  with message. Enum variants are position-encoded — **append new variants only, never
  reorder or insert**; state this in `proto`'s module docs and in the CONTRIBUTING protocol
  bump checklist. Define now: Hello/HelloAck, AuthChallenge/AuthResponse, ModManifest,
  ContentRequest/ContentChunk, JoinWorld, ChunkData, ChunkUnload, BlockDelta (block or
  subnode edit), PlayerInput, EntityStateDelta, Chat, Disconnect{reason}.
- Tick loop: fixed 20 tps (`const TICK_HZ`), monotonic-clock accumulator, tick counter is the
  authoritative timebase. Per tick: drain network → apply inputs → run mod tick hooks
  (`game.register_on_tick(fn(dt_ticks))` — add to the script API) → dirty-chunk save batching
  (debounced, plus periodic full flush) → broadcast deltas. Simulation threads call
  `detgen::assert_ieee_mode()` at spawn (charter rule 4).

### Identity (charter rule 13) — implement the whole model, not just the handshake
A key you can only lose once is a design defect. All of this lands here, because retrofitting
identity after the protocol and the players table are frozen is a breaking change.

- **Seed and key.** Client generates 256 bits of entropy on first run. An Ed25519 secret key
  *is* 32 bytes of entropy (`ed25519-dalek`), so the seed is the key directly — no derivation
  path, no ambiguity about which key a phrase produces.
- **Recovery phrase.** That seed is rendered as a checksummed BIP-39 24-word mnemonic
  (`bip39` crate) and shown once on first run with a "write this down, it is the only way
  back" prompt. Client commands: `--show-recovery-phrase` (re-display, local only) and
  `--restore-from-phrase` (reconstruct the identity on a new machine). The checksum means a
  mistyped word is caught rather than silently producing a stranger's identity.
- **Key file at rest.** Platform data dir, restrictive permissions (0600 / Windows ACL
  equivalent). Deliberately NOT passphrase-encrypted by default: a prompt on every launch is
  the kind of friction that makes people stop playing, and the phrase is the real backup.
  Document that choice rather than leaving it implicit.
- **Canonical UUID** = BLAKE3(root pubkey). It never changes, whatever happens to the key set.
- **Key sets.** An identity is a set of authorised pubkeys (`player_keys`, Task 03). Adding a
  device requires `AddKey{new_pubkey, next_key_hash}` signed by an existing authorised key.
  This is the passkey model: many credentials, one account. Losing one device does not lose
  the identity.
- **Pre-committed rotation.** Each key registers `next_key_hash` = BLAKE3 of its designated
  successor pubkey. `RotateKey{new_pubkey, new_next_key_hash}` is accepted only if
  BLAKE3(new_pubkey) equals the stored commitment AND the message is signed by the current
  key. A stolen current key therefore cannot rotate the identity away from its owner — the
  thief does not have the pre-committed successor. Rotation records persist and are
  replayable from `player_keys`.
- **Join handshake.** Hello (protocol ver, claimed pubkey, claimed display name) → server
  replies AuthChallenge with a 32-byte random nonce → client returns AuthResponse: signature
  over `(nonce ‖ server cert fingerprint ‖ protocol version)` → server verifies the key is in
  the identity's authorised set before ANY world state flows. Binding the cert fingerprint
  into the signed payload stops a MITM relay splicing a handshake captured on another server.
- **Name binding.** `player_names` maps display name → UUID, first-come. A Hello claiming a
  bound name under a different identity is rejected with a clear reason. Name changes are an
  explicit server operation (RCON `rename`).
- **Admin escape hatch.** RCON `rebind <uuid> <new-root-pubkey>` — for the player with no
  phrase and no second device. Writes an audit row; deliberately manual and loud.
- **`AuthProvider` trait.** The verification step sits behind a trait with the built-in
  self-sovereign implementation as the default. A community identity service can be added
  later without a protocol break. Do not implement a second provider now.
- **Honest sybil note**, in code comments and in the Task 16 docs: keypairs are free, so
  UUID bans are trivially evaded. Complements available to admins: IP/subnet bans and
  allowlist mode (implement allowlist here — it is ten lines and it is what small private
  servers actually use). Do not claim key identity solves moderation.
- All engine and mod state (inventory, bans, mod storage, later the mimic imprint) keys on
  the UUID, never the name.

- Join flow (after identity verifies): → ModManifest (resolved mod set: ids, versions,
  content hashes of each mod dir's client-relevant files) → client requests missing content
  by hash → ContentChunks (chunked transfer, zstd) → JoinWorld → server streams chunks in a
  radius around spawn, nearest-first, with per-client send budget per tick.
- Hostile-input posture (charter rule 14) starts HERE: the protocol decoder treats every
  inbound message as adversarial — length caps before allocation, postcard decode errors are
  per-connection disconnects (never panics), content transfers enforce declared-size vs
  actual-size and per-client rate/quota. Add `fuzz/` with a `cargo fuzz` target for the
  message decoder in THIS task; CI runs it for a bounded smoke duration from now on.
- Content addressing: hash every distributable file (BLAKE3); clients cache by hash on disk;
  never resend cached content. (Client side lands in Task 08 — implement the server half and
  exercise it via the bot.)
- Interest management: per-player loaded-chunk set from view distance (config), load/generate
  on demand through the Task 05 worldgen path, unload notifications, server-side chunk cache
  with LRU eviction of clean chunks.
- Block edits: validate (player range, chunk loaded), apply via core, persist-dirty, broadcast
  BlockDelta to interested players. Expose `game.register_on_block_change(fn(pos, old, new, actor))`.
  If Task 02b measured sub-node chiselling deltas as expensive, apply its recommended compact
  encoding here.
- `ServerHandle::start_embedded(config) -> (handle, local_addr)` — in-process server on
  loopback for singleplayer; identical code path to the standalone binary.
- Admin: line-based RCON on localhost-only TCP (config-gated, token auth): `status`, `save`,
  `stop`, `kick <name>`, `mods`, `rename`, `rebind`, `allowlist`.

## Tests
- [A] In `crates/bot`: minimal protocol client (connect, complete join flow, request chunks,
  send PlayerInput/BlockDelta, record received messages) — scaffolding for Task 07, keep it
  library-shaped.
- [A] Integration (real loopback server): join flow completes; mod manifest matches Task 05
  resolved set; bot edits a block → persisted (restart server, chunk reloads with the edit)
  and broadcast to a second bot; version-mismatch client rejected cleanly; abrupt bot
  disconnect doesn't leak the player.
- [A] Identity suite:
  - bot B presenting bot A's bound name with a different identity is rejected;
  - replayed handshake signature (stale nonce) rejected;
  - signature bound to a different server fingerprint rejected;
  - identity persists across server restart (same UUID reclaims name and inventory);
  - **phrase round-trip**: generate identity → derive phrase → wipe key file → restore from
    phrase → same UUID, same inventory, rejoins cleanly;
  - **key set**: second key added via AddKey signed by the first can join as the same UUID;
    an AddKey signed by an unauthorised key is rejected;
  - **pre-rotation**: RotateKey matching the stored commitment succeeds; one not matching is
    rejected even when correctly signed by the current key;
  - **rebind**: RCON rebind lets a new root key claim an existing UUID and writes the audit
    row; a rebind attempt over RCON without the admin token fails.
- [A] Fuzz: decoder fuzz target runs clean for the CI smoke duration; a corpus of captured
  real sessions committed as seeds.
- [A] Tick stability: 200 ticks under load of 4 bots streaming inputs; no tick exceeds 2×
  budget on CI (soft-log, hard-fail at 5×).
- [A] Bench: chunk serialize+send path; join-flow wall time with cold vs warm content cache.

## Acceptance criteria
- [A] Two bots on one server see each other's block edits.
- [A] The full identity suite passes: no identity theft by name, and no unrecoverable
      identity — phrase restore, key-set addition, pre-rotation, and admin rebind all proven.
- [A] Server restart preserves edits (persistence integration proven over the wire).
- [A] Embedded server runs the same tests via `ServerHandle` in-process.
- [A] RCON `save`/`stop` work; SIGTERM = flush + clean exit under active connections.
