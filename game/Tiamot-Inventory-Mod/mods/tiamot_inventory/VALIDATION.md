# Validation — ornate revision 0.2

Checks were run on Linux with the pinned Rust 1.97.1 toolchain. ALSA development
files were supplied locally for the client's audio build. A clean target directory
on the local filesystem was used because an earlier scratch-backed build produced
corrupted compiler artifacts.

## Executed checks

- `cargo check -p client --tests`: passed.
- `cargo fmt --all --check`: passed.
- Diff whitespace check: passed for project files. The unchanged upstream
  font license retains one trailing space in `OFL.txt`.
- Lua 5.4 via `lupa.lua54`: all 10 inventory scenarios and HUD checks passed.
  These cover crafting costs/refunds, material depletion, per-player isolation,
  sparse inventory paging, off-hand, UTF-8 truncation and the HUD sandbox/budget.
- The preview was regenerated from the real Lua widget trees and HUD commands
  using the bundled font and two artwork files, then visually inspected.

- `cargo test -p client --lib`: 341 passed, including PNG decode/upload,
  image request budgets, nine-slice geometry, cached asset decode and dialogs.
- `cargo clippy -p client --tests -- -D warnings`: passed.
- `cargo check --manifest-path fuzz/Cargo.toml --bin ui_artwork`: passed.
  This compiles the new fuzz target; no timed fuzz campaign was run.
- `cargo run -p server -- --check-mods mods`: passed. The standalone UI mod
  registers no blocks; the expected warning means content mods are also needed
  for a playable world.

- `cargo test -p client --test connection ornate_inventory_dialog`: passed
  after correcting artwork addresses. A real loopback server loads the mod,
  opens its inventory, verifies both widget hashes against the shipped PNGs,
  serves both files, and the client decodes both 1254×1254 images.
- Both generated installation patches passed `git apply --reverse --check`
  against this working tree. ZIP integrity was checked.

The connection test originally exposed incorrect artwork addresses. Both Lua
scripts now use the engine's domain-separated BLAKE3 scheme. The preview renderer
also resolves actual addresses, so stale inventory or HUD hashes fail rendering.

## Awaiting human verification

The preview is an approximate layout render with representative items. It is not
an in-game screenshot or a claim that gameplay feel has been verified.

After applying the appropriate engine patch and rebuilding, enable Tiamot
Inventory with Core UI disabled. Join and press E. Verify:

1. Both tabs, larger labels, ornate frames and hotbar appear at the intended scale.
2. E/Escape closes the panel; left-click transfer and right-click split/place work.
3. Shape crafter: select material, carve/restore, rotate, then craft one and a stack.
4. Deplete the chosen material and confirm another material is not silently spent.
5. Try a small window/high UI scale and scroll to all controls.
6. Reconnect and confirm both artwork files load from the content cache.

Cinzel Decorative Bold also changes the native proportional UI face; inspect
menu readability alongside the inventory. Go Mono remains the fallback.
