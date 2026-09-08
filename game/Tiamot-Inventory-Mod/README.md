# Tiamot Inventory — ornate edition (0.2)

A default-inventory candidate for Tiamot, based on upstream commit
`dd1b22cfe6b59caeefe287dbdee7c248c4a231f5`.

The ornate revision follows the supplied references more closely: generated
iron-and-brass frames with ivory corner scrolls, heavy inset slot rims, larger
buttons, and Cinzel Decorative Bold lettering. The original Inventory / Shape
crafter tabs remain button-driven, with a thick active-tab underline.

## Install or update

1. Extract this archive into the repository root. Keep the `mods/` and `crates/`
   paths as supplied. They contain the mod and bundled font. The selected patch
   adds the new engine source and fuzz target.
2. Choose **one** engine patch:
   - Unmodified upstream: `git apply --check engine-style.patch`, then
     `git apply engine-style.patch`.
   - Our first inventory revision already applied: use
     `upgrade-from-v1.patch` in the same commands instead.
   - Our current working branch: the changes are already applied.
3. Copy `mods/tiamot_inventory` to the active server's `game/` directory.
   For an update, replace the prior `game/tiamot_inventory` copy with this one.
4. In the launcher's mod selection, enable **Tiamot Inventory** and disable
   **Core UI**, keeping your other mods enabled. Both UI mods register a HUD
   and inventory action, so enabling both produces overlapping interfaces.
5. Rebuild with the repository's pinned Rust toolchain: `cargo build -p client`.
   Press **E**, or rebind **Open inventory / shape crafter** in controls.

For a dedicated server, use the same mod in its `mods_path`, selecting it in
`enabled_mods` alongside the other content mods and excluding `core_ui`.
Rebuilt clients fetch the two PNGs and HUD script automatically from the server.
Existing bindings for `core_ui:inventory` do not transfer to the new mod action.

The engine patches do not change the network protocol or core widget schema.
They finish rendering the existing `image` / `nine_slice` fields, apply mod
colors to interactive surfaces, and let larger content scroll within the native
sheet. The proportional font is bundled in the client and affects native menus
as well; Go Mono remains the monospace and missing-glyph fallback. No font needs
to be supplied by the player, and fonts are not downloaded from servers.

## Inventory and crafting

- Slots 1–9 remain quick access; slots 10–27 are pack page one; slot 28 remains
  the off-hand. Later pages expose slots from 29 onward. Paging never moves items.
- Inventory cells are 58 points rather than the first revision's 44. HUD cells
  are 72 virtual pixels rather than 60. Tabs/buttons are 44 points tall; button
  text is 19 points and the main heading is 30.
- The shape editor retains left-click carving, right-click restore and native
  rotation buttons. Slab, stair and pillar presets contain 9, 18 and 3 cells.
- Craft one or up to 90, limited by available loose material. Each occupied cell
  costs one unit. Named/shaped stacks are excluded, incomplete transactions are
  refunded, and depletion never silently selects another material.
- Live carving does not echo an outdated widget tree over a newer client click.
  The fixed per-cell rule stays visible; a completed craft reports its actual cost.
- Large controls keep their size. The native sheet scrolls vertically, with
  horizontal scrolling available when a narrow window cannot fit the grid.

`init.lua` owns layout and crafting. `hud.lua` owns the HUD. Artwork details,
font provenance and generation prompts are in `ARTWORK.md`. `preview.png` is
an approximate layout render generated from the actual Lua trees/HUD commands
with representative stacks, not an in-game screenshot.

## Validation

See `VALIDATION.md` for the checks actually executed in this revision.

```sh
lua5.4 mods/tiamot_inventory/tests/test_inventory.lua
cargo fmt --all --check
cargo test -p client ui_images::tests --lib
cargo test -p client cached_ui_artwork --lib
cargo test -p client --test connection ornate_inventory_dialog
cargo run -p server -- --check-mods mods
```

For the preview, install Pillow, blake3 and lupa, then run
`python mods/tiamot_inventory/tests/render_preview.py`.

Before shipping as the default, verify pointer interaction and visual feel in
an actual game window: E/Escape, drag/split, sparse hotbar selection, off-hand,
carve/restore/rotate, material depletion, reconnect, and scrolling at your chosen
resolution/UI scale. Native gameplay and visual approval remain a human check.

Code and the reused `core_ui` click sound: GPL-3.0-only, copyright Iridesium.
Font: SIL OFL 1.1, bundled separately with its own copyright and license.
