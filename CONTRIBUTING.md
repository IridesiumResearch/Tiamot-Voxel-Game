<!-- SPDX-FileCopyrightText: Iridesium -->
<!-- SPDX-License-Identifier: GPL-3.0-only -->

# Contributing

## Licensing and provenance

Contributions are accepted under the **Developer Certificate of Origin** (DCO)
version 1.1, reproduced below. **Authors retain copyright in their
contributions** and license them under GPLv3-only (MIT for anything under
`api/`). There is no copyright assignment and no CLA.

One consequence worth stating plainly: because copyright stays with each author,
the project cannot be relicensed, and the section 7 Additional Permission in
[`LICENSE.EXCEPTION`](LICENSE.EXCEPTION) cannot be amended, without the sign-off
of every contributor whose code is affected. **Only Iridesium can grant or amend
that exception**, and it can only do so for code it holds copyright in. This is
a deliberate trade — contributor-friendly, but it makes licensing changes hard
on purpose. If the project ever needs a CLA, that decision has to be made before
outside contributions are accepted, not after.

### Sign-off is required

Every commit must carry a `Signed-off-by` trailer matching its author:

```
Signed-off-by: Your Name <your.email@example.com>
```

`git commit -s` adds it. To get it automatically, point git at the repo's hooks
once after cloning:

```
git config core.hooksPath .githooks
```

CI rejects any commit in a pull request without a valid, author-matching
sign-off.

## Before you open a pull request

Run what CI runs:

```
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
./scripts/check-dep-firewall.sh
./scripts/check-spdx.sh
cargo deny check
```

## Changing the wire protocol

`postcard` encodes an enum variant as its **ordinal**. Inserting a variant
anywhere but the end silently reinterprets every later message on every existing
peer — no error, no checksum, just wrong messages. The same applies to the chunk
blob format.

Before changing anything in `crates/core/src/proto/`:

1. **Append. Never insert, remove, or reorder a variant.** Deprecate in place.
2. **Bump `PROTOCOL_VERSION`.** Peers exchange it first thing so a mismatch is a
   clean rejection rather than a mysterious decode failure.
3. **Update `variant_ordinals_are_pinned`**, the test that pins every ordinal.
   If it fails and you did not mean to move anything, something moved.
4. **Re-seed the fuzz corpus** if you added a message shape, so the fuzzer
   starts from valid framing for it.
5. For chunk blobs specifically, add a migration step — see
   `crates/core/src/persist/migrate.rs`.

## House rules

These come from [`CLAUDE.md`](CLAUDE.md), the project charter. Read it before
proposing anything structural — it is short and it is binding.

- **The engine is mechanisms.** Content is Lua mods. `game/` holds reference
  mods and test fixtures, not a shipped game. Don't add content, tune game feel,
  or design progression here.
- **The mod API is the only API.** If a feature can't be built through it, fix
  the API rather than special-casing core.
- **The dependency firewall is enforced.** `core` and `server` must never
  transitively depend on `wgpu`, `winit`, `kira`, or `egui`. CI checks this
  across all target platforms.
- **Determinism gates are sacred.** Never resolve a cross-platform hash mismatch
  by loosening the test. Simulation floats stay inside the Deterministic Float
  Subset (charter rule 4); the clippy `disallowed-methods` lint enforces it and
  must not be silenced. Changing the subset means editing
  `docs/float-determinism.md` first.
- **Every parser or decoder that touches untrusted bytes ships its fuzz target
  in the same change** — not deferred to a hardening pass.
- **Every source file carries an SPDX header.** `GPL-3.0-only` everywhere except
  under `api/`, which is `MIT`. CI checks this.
- Rust edition 2024, stable toolchain (pinned in `rust-toolchain.toml`).
  `thiserror` for errors. No `unwrap()` outside tests. Public items documented.
- Conventional-commit messages, one working increment per commit.

## Developer Certificate of Origin 1.1

```
Developer Certificate of Origin
Version 1.1

Copyright (C) 2004, 2006 The Linux Foundation and its contributors.

Everyone is permitted to copy and distribute verbatim copies of this
license document, but changing it is not allowed.


Developer's Certificate of Origin 1.1

By making a contribution to this project, I certify that:

(a) The contribution was created in whole or in part by me and I
    have the right to submit it under the open source license
    indicated in the file; or

(b) The contribution is based upon previous work that, to the best
    of my knowledge, is covered under an appropriate open source
    license and I have the right under that license to submit that
    work with modifications, whether created in whole or in part
    by me, under the same open source license (unless I am
    permitted to submit under a different license), as indicated
    in the file; or

(c) The contribution was provided directly to me by some other
    person who certified (a), (b) or (c) and I have not modified
    it.

(d) I understand and agree that this project and the contribution
    are public and that a record of the contribution (including all
    personal information I submit with it, including my sign-off) is
    maintained indefinitely and may be redistributed consistent with
    this project or the open source license(s) involved.
```
