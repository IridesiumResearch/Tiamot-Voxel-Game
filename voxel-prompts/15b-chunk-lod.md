# TASK 15b — Chunk LOD and far view distance

Depends on: 15a (LOD caches are per-domain), 10. Client rendering + a server-side summary
path. **No content in this task.**

## Objective
See far. A mip chain of downsampled chunk volumes lets view distance extend well past the
full-detail streaming radius, without the server shipping full chunk data for everything on
the horizon.

## Design
### Levels
- LOD0 = the Task 08 mesher at full resolution (sub-node, or block-resolution if Task 02b
  returned FALLBACK — LOD0 is defined as "whatever 08 ships", not as a fixed resolution).
- LOD1 = block-resolution meshing.
- LOD2+ = downsampled 2ⁿ block volumes, majority-material downsample.
- Downsampling is computed **server-side** on demand, cached in the DB per domain, and
  invalidated on edit. Cache key includes the domain (Task 15a) and the level.
- Ring-based level selection by distance from camera, with hysteresis so a camera hovering on
  a ring boundary does not thrash between levels.

### Seams — use skirts, and do not re-open this
Cracks at LOD boundaries have two standard fixes: skirts (extend each chunk's border geometry
downward/outward to hide the gap) and geometric stitching (generate transition cells).
Transvoxel and dual contouring — the algorithms usually cited for this problem — are
**isosurface** techniques for smooth terrain and do not apply to cubic voxels at all; do not
be drawn into them by search results.

For blocky voxels, skirts are the standard answer and what shipped LOD implementations for
Minecraft-like worlds use. They cost a small amount of overdraw and a few extra quads per
chunk border. Stitching costs a combinatorial transition-cell implementation and a permanent
maintenance burden for a marginal fill-rate gain. **Implement skirts.** Record the reasoning
in module docs so it is not relitigated.

### Server support
- `ChunkSummary` message: palette-majority volume at a requested level, far cheaper than
  `ChunkData`. Per-client budget as with chunk streaming.
- Summaries generate from the authoritative chunk, so they are deterministic; hash them in
  the CI gate.
- Invalidation on edit must propagate up the mip chain (an LOD0 edit dirties every level
  above it for that column).

### Async and budget
- LOD mesh building is async with a per-frame budget, same machinery as remeshing. A distant
  ring appearing a few frames late is fine; a hitch is not.

## Tests
- [A] Downsample determinism: majority-material downsample hashes identically across
  platforms, in the CI gate.
- [A] Edit invalidation: an LOD0 edit updates every affected summary level; assert cache rows
  invalidated and regenerated correctly.
- [A] Ring hysteresis: a camera oscillating across a boundary does not rebuild meshes every
  frame (assert rebuild count bounded).
- [A] Skirt coverage: for a synthetic scene with adjacent chunks at different levels, assert
  no gap in the depth buffer along the boundary (renderable headless via the screenshot
  harness — sample the boundary column, not the whole frame).
- [A] Budget: LOD build never exceeds its per-frame allocation under a scripted fly-through.
- [H] Screenshot scenes at several view distances look right — no visible seams, no popping
  that reads as broken.

## Acceptance criteria
- [A] Determinism, invalidation, hysteresis, and skirt-coverage tests green.
- [A] LOD caches are per-domain and a second domain's summaries do not collide with the
      overworld's.
- [H] Overworld view distance with LOD reaches ≥ 32 chunks on the dev machine at 60 fps in
      mode 2. Record actuals in the PR.
