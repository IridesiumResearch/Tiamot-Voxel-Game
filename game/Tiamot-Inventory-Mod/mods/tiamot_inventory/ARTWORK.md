# Ornate artwork — v0.2

Generated with the built-in image-generation tool for this mod. The two PNGs are
retained unchanged at their returned 1254×1254 dimensions. Their engine-domain BLAKE3 hashes (`tiamot:content:v1` followed by file bytes)
in `init.lua` and `hud.lua` address the exact files served by Tiamot's existing
content pipeline. No images are registered as fake blocks or items.

- `textures/ornate-panel.png`: carved iron/brass panel frame with ivory corner scrolls.
- `textures/iron-slot.png`: simpler heavy iron frame for item slots, buttons and tabs.

Both images have opaque charcoal centers. The renderer splits each at 25% and
75% in both axes. UI corners remain square and at most 52 points wide; only the
rails and empty center stretch. HUD slots are square and use the second image
whole. This keeps the inventory's controls and text independent of the artwork.

## Typography

Cinzel Decorative Bold is bundled unchanged with its SIL OFL 1.1 license under
`crates/client/assets/third-party/cinzel-decorative/`. It replaces the client's
proportional face, so native menus use it too. Go Mono remains the monospace
and missing-glyph fallback. No font upload is needed. This is a close stylistic
interpretation, not an identification of the lettering in the references.

## Final generation prompts

### Panel

Create one production-ready seamless nine-slice UI PANEL FRAME TEXTURE for a dark fantasy voxel game, square 1024x1024 image. Straight-on flat orthographic view, perfectly square outer boundary, NO perspective. Art direction: ornate ancient dwarven architecture, thick weathered charcoal iron double rails with warm ivory-brass engraved edges, angular chiseled knotwork, tiny rivets, symmetrical ivory bone-like curled corner ornaments (stylized abstract, no skull faces), like a richly rendered classic fantasy RPG inventory. Bold weight and legible detail, smoothly shaded painterly metal rather than modern thin outlines. Outer frame thickness about 125 pixels on all sides; ALL major ornaments confined within the outer 170px corners so the central 684x684 region can stretch as a nine-slice image without deforming decoration. Along middle edges use subtle repetitive carved geometric marks, NO letters and NO text. The very large center is a completely plain flat near-black charcoal (#14171a) opaque panel with no symbols, no illustrations, no objects. The frame exactly fills the image to its edges, no surrounding scene, no margins, no shadow outside the canvas, no separate background, no UI controls, no inventory items. Square reusable UI texture, not a concept screenshot. Warm brass should be restrained, worn iron dominant. No watermark.

### Slots and buttons

Generate a single SQUARE reusable UI ITEM SLOT / BUTTON FRAME texture for a dark fantasy voxel RPG. Square image, frontal orthographic, precisely flush with all four edges, no perspective. Bold THICK weathered dark iron beveled rim, three stepped metallic rails, muted warm brass inner edge, robust angular armored corner caps with one small brass rivet each. Ancient dwarven/ruined temple styling, carved angular corner details, tactile hammered metal. Smoothly shaded detailed painterly art, not a flat modern outline, not pixel art. VERY SIMPLE compared with an ornate main window frame: no bone curls, no faces, no icons, no letters or text. The frame fills the whole canvas; center is an opaque empty near-black charcoal (#14171a) square. All border art confined to outer 18 percent on all sides, corner caps confined to 23 percent of each corner, so a nine-slice renderer can stretch the center and middle rails. Symmetrical square with clipped/chamfered corners but no margins. Should remain legible reduced to a 64 pixel inventory cell, and also stretch horizontally into a chunky tab or button. No scene, no showcase layout, no multiple objects, no watermark. Produce a 1024 by 1024 PNG texture.
