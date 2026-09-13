// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The scratch buffer worldgen writes into.
//!
//! # The whole point: block-level is the cheap default
//!
//! Sub-Node Contract §5 says generators write at **block resolution** by
//! default, and sub-node detail is opt-in per generator. This type is what makes
//! that real rather than aspirational.
//!
//! A buffer starts backed by 16³ = 4,096 block slots. The moment a sub-node
//! operation touches it, and not before, it expands to 48³ = 110,592 cells. A
//! generator that only ever places blocks — which is nearly all of them — never
//! allocates or touches 27× the memory, and never pays 27× the fill cost.
//!
//! Expansion is one-way. There is no attempt to detect that a buffer has become
//! uniform again and collapse back: it would cost a scan on every write to catch
//! a case that arises rarely, and [`ChunkBuffer::to_chunk`] already compresses
//! the result properly on the way out.
//!
//! This is the object handed to Lua generator callbacks in Task 05.

use crate::block::{BlockValue, Cells, EMPTY_CELLS, SUBNODES_PER_BLOCK, subnode_index};
use crate::chunk::Chunk;
use crate::coords::{ChunkPos, LocalBlock};
use crate::material::MaterialId;
use crate::{BLOCKS_PER_CHUNK, CHUNK_BLOCKS, CHUNK_SUBNODES, SUBNODES_PER_AXIS};

/// Sub-node cells in a fully expanded buffer.
pub const CELLS_PER_CHUNK: usize = (CHUNK_SUBNODES * CHUNK_SUBNODES * CHUNK_SUBNODES) as usize;

/// How a buffer is currently backed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Storage {
    /// One material per block. The default and the cheap path.
    Blocks(Vec<MaterialId>),
    /// One material per sub-node cell. Entered only on demand.
    SubNodes(Vec<MaterialId>),
}

/// A scratch chunk that worldgen fills and then converts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChunkBuffer {
    pos: ChunkPos,
    storage: Storage,
    /// Fluid placed during generation, if the generator placed any.
    ///
    /// **Beside the terrain rather than in it.** A block holds a material and,
    /// separately, a volume of one fluid — see `fluid::Fluid` — so an ocean is
    /// not a material a generator fills with, it is a layer over the same
    /// blocks. Kept empty until something asks for it, because the great
    /// majority of chunks have no fluid at all and an empty layer costs
    /// nothing.
    fluid: crate::fluid::FluidLayer,
}

/// One layer of a surface, for [`ChunkBuffer::fill_layers`]: the material a
/// cell takes when its block's code is `code` and its depth is in
/// `from..to`, in the depth field's own units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Layer {
    /// The code field's value, rounded, that selects this layer.
    pub code: i32,
    /// Depth at which the band begins, inclusive.
    pub from: f32,
    /// Depth at which it ends, exclusive.
    pub to: f32,
    /// What the band is made of.
    pub material: MaterialId,
}

/// How much resolution a density fill gives the surface.
///
/// Block resolution is the default and costs what it always did. The other two
/// are the opt-in of Sub-Node Contract §5, and they differ in what they can
/// SHOW rather than only in what they cost — see
/// [`ChunkBuffer::fill_density_detail`].

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Detail {
    /// Ask the field about all 27 cells of a surface block.
    ///
    /// The finest answer available, and the only one that can show a term the
    /// mod added at sub-node scale.
    Sampled,
    /// Interpolate the block-resolution samples down to the 27 cells.
    ///
    /// Nearly free, and it cannot show anything the block-resolution field did
    /// not already contain: it removes staircases rather than adding detail.
    Smooth,
}

/// Trilinearly interpolates a block's eight corner samples to its 27 cells.
///
/// The samples are at block ORIGINS, so a block's eight corners are the samples
/// at its own position and the seven one step along each axis — which is why
/// the caller's field is padded on the positive side as well as the negative.
///
/// Cell centres rather than cell corners: cell `c` spans `c/3 .. (c+1)/3` of
/// the block, so its middle is `(c + 0.5) / 3`. Sampling the corner instead
/// would bias every surface half a cell towards the block's origin, which over
/// a whole world reads as terrain sitting slightly too low./// What a bound settles about a palette fill before any evaluation.
enum Decided {
    /// No value in the chunk clears the lowest band: nothing to write.
    Nothing,
    /// Every value lands in one band: the whole chunk is this.
    All(MaterialId),
    /// The chunk crosses a boundary and has to be evaluated.
    Mixed,
}

/// The most bands a [`Palette`] may hold.
///
/// Sixteen materials by depth is more than any real strata want, and a table
/// of a thousand would turn the per-block lookup below from a handful of
/// comparisons into something that costs more than the field it maps.
pub const MAX_PALETTE_BANDS: usize = 16;

/// A table from a field's value to a material: the generalisation of "solid
/// where the field is positive" to "this material where it is above this,
/// that one where it is above that".
///
/// # Why a fill wants a table and not a material
///
/// A field of the form `noise - y` has, at every point, the value *surface
/// height minus y* — which is to say the DEPTH below the surface. So a
/// generator that wants grass on the top block, dirt for the next three and
/// stone below has three bands of that one field: above 0, above 1, above 4.
///
/// Before this it wrote them as three fills of three shifted fields, and each
/// fill evaluated the whole field over the chunk again. Ten materials was ten
/// evaluations of the same six-octave noise for one answer per block; the
/// write loop that follows an evaluation is a rounding error beside it, so the
/// generator's cost was very nearly the number of materials it layered. A
/// palette evaluates once and looks each value up.
///
/// # The rule
///
/// Bands are sorted by threshold. A value takes the material of the highest
/// band it is strictly above — the same `> 0.0` a plain fill uses, so
/// `fill_density(field, m)` and a one-band palette `{ 0 → m }` are the same
/// fill. A value not above the lowest band writes nothing: a fill ADDS, and
/// what the block held before is what it holds after.
#[derive(Debug, Clone, PartialEq)]
pub struct Palette {
    /// Ascending by threshold, no two equal.
    bands: Vec<(f32, MaterialId)>,
}

/// A palette that cannot be used.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum PaletteError {
    /// No bands at all: a fill that could never write anything.
    #[error("a palette needs at least one band")]
    Empty,
    /// More than [`MAX_PALETTE_BANDS`].
    #[error("a palette may hold at most {MAX_PALETTE_BANDS} bands, not {found}")]
    TooMany {
        /// How many were given.
        found: usize,
    },
    /// A threshold that is not a number.
    ///
    /// Refused rather than sorted somewhere arbitrary: every comparison with a
    /// NaN is false, so a NaN band would silently never be chosen and never be
    /// passed over either, and the bands around it would misbehave in ways
    /// that depend on where the sort happened to leave it.
    #[error("a palette threshold is not a number")]
    NotANumber,
    /// Two bands with the same threshold, which could never both be chosen.
    #[error("two palette bands share the threshold {threshold}")]
    Duplicate {
        /// The shared threshold.
        threshold: f32,
    },
}

impl Palette {
    /// Builds a palette from `(threshold, material)` pairs in any order.
    ///
    /// # Errors
    ///
    /// [`PaletteError`] for an empty table, too many bands, a NaN threshold or
    /// two equal ones.
    pub fn new(mut bands: Vec<(f32, MaterialId)>) -> Result<Self, PaletteError> {
        if bands.is_empty() {
            return Err(PaletteError::Empty);
        }
        if bands.len() > MAX_PALETTE_BANDS {
            return Err(PaletteError::TooMany { found: bands.len() });
        }
        if bands.iter().any(|(threshold, _)| threshold.is_nan()) {
            return Err(PaletteError::NotANumber);
        }
        // Total order is fine now that NaN is excluded; `partial_cmp` cannot
        // fail on what is left.
        bands.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        // Exact equality is the question — two thresholds a hair apart are two
        // bands, however useless — and with NaN excluded above `partial_cmp`
        // always answers.
        if let Some(pair) = bands
            .windows(2)
            .find(|pair| pair[0].0.partial_cmp(&pair[1].0) == Some(std::cmp::Ordering::Equal))
        {
            return Err(PaletteError::Duplicate {
                threshold: pair[0].0,
            });
        }
        Ok(Self { bands })
    }

    /// The material for one value, or `None` below the lowest band.
    ///
    /// A linear walk from the top, because the table is at most sixteen long
    /// and a deep block — most blocks — is answered by the first comparison.
    #[must_use]
    pub fn pick(&self, value: f32) -> Option<MaterialId> {
        self.bands
            .iter()
            .rev()
            .find(|(threshold, _)| value > *threshold)
            .map(|(_, material)| *material)
    }

    /// The material for one cell, decided the way a sequence of separate detail
    /// fills would have decided it.
    ///
    /// For each band, from the top: the cell's own value is what counts if the
    /// block is on that band's shell (`on_shell[k]`), and the block's centre
    /// value otherwise — because the fill for that band would have refined
    /// this block in the first case and written it whole in the second. The
    /// highest band that passes wins. See `fill_palette_detail` for why this
    /// and not simply [`Self::pick`] on the cell.
    #[must_use]
    pub fn pick_as_fills(&self, centre: f32, cell: f32, on_shell: &[bool]) -> Option<MaterialId> {
        self.bands
            .iter()
            .enumerate()
            .rev()
            .find(|(k, (threshold, _))| {
                let value = if on_shell.get(*k).copied().unwrap_or(false) {
                    cell
                } else {
                    centre
                };
                value > *threshold
            })
            .map(|(_, (_, material))| *material)
    }

    /// The lowest threshold: below it a fill writes nothing.
    fn floor(&self) -> f32 {
        self.bands[0].0
    }

    /// The bands, ascending.
    #[must_use]
    pub fn bands(&self) -> &[(f32, MaterialId)] {
        &self.bands
    }
}

fn trilinear_cells(field: &[f32], pitch: usize, at: [usize; 3], out: &mut [f32]) {
    let [px, py, pz] = at;
    let corner = |dx: usize, dy: usize, dz: usize| {
        field[(px + dx) + pitch * ((py + dy) + pitch * (pz + dz))]
    };
    let c000 = corner(0, 0, 0);
    let c100 = corner(1, 0, 0);
    let c010 = corner(0, 1, 0);
    let c110 = corner(1, 1, 0);
    let c001 = corner(0, 0, 1);
    let c101 = corner(1, 0, 1);
    let c011 = corner(0, 1, 1);
    let c111 = corner(1, 1, 1);

    let axis = SUBNODES_PER_AXIS as usize;
    for z in 0..axis {
        let tz = (z as f32 + 0.5) / axis as f32;
        for y in 0..axis {
            let ty = (y as f32 + 0.5) / axis as f32;
            for x in 0..axis {
                let tx = (x as f32 + 0.5) / axis as f32;
                // Written as explicit multiply-adds rather than `mul_add`:
                // charter rule 4 bans the latter, because it uses a hardware
                // FMA where there is one and a software fallback where there
                // is not, and the two round differently.
                let x00 = c000 + (c100 - c000) * tx;
                let x10 = c010 + (c110 - c010) * tx;
                let x01 = c001 + (c101 - c001) * tx;
                let x11 = c011 + (c111 - c011) * tx;
                let y0 = x00 + (x10 - x00) * ty;
                let y1 = x01 + (x11 - x01) * ty;
                out[subnode_index(x as u32, y as u32, z as u32)] = y0 + (y1 - y0) * tz;
            }
        }
    }
}

/// One block of a [`Schematic`]: where it sits from the root, what it is, and
/// which of the block's 27 cells it fills.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StampBlock {
    /// Offset from the root along x, in blocks.
    pub dx: i32,
    /// Offset from the root along y, in blocks.
    pub dy: i32,
    /// Offset from the root along z, in blocks.
    pub dz: i32,
    /// The material written into the cells in `mask`.
    pub material: MaterialId,
    /// The cells written, one bit each, indexed `x + 3*y + 9*z`. Every other
    /// cell of the block keeps what it held: a stamp is a merge write
    /// (Sub-Node Contract §7.4), so a trunk's base sits IN the surface block
    /// rather than replacing it with a block of trunk and air.
    pub mask: u32,
}

/// A structure built once and stamped many times: a tree, a boulder, a snag.
///
/// **Built once, in Lua, from a list of blocks; stamped natively.** A tree is
/// a hundred and fifty blocks of masks, and a forest is a dozen trees a chunk.
/// Writing those through `set_subnode_world` is two thousand crossings into the
/// VM per tree per chunk it overlaps, which is the per-cell loop charter rule
/// 4 forbids and the cost the opaque handles exist to prevent. So the mod
/// describes each tree once — a table of `{dx, dy, dz, material, mask}` — and
/// [`ChunkBuffer::scatter`] decides where the trees go and writes them.
#[derive(Debug, Clone, Default)]
pub struct Schematic {
    blocks: Vec<StampBlock>,
    reach_x: i32,
    reach_z: i32,
    lowest: i32,
    highest: i32,
}

impl Schematic {
    /// A schematic from its blocks, in any order.
    #[must_use]
    pub fn new(blocks: Vec<StampBlock>) -> Self {
        let mut schematic = Self {
            blocks,
            ..Self::default()
        };
        for block in &schematic.blocks {
            schematic.reach_x = schematic.reach_x.max(block.dx.abs());
            schematic.reach_z = schematic.reach_z.max(block.dz.abs());
            schematic.lowest = schematic.lowest.min(block.dy);
            schematic.highest = schematic.highest.max(block.dy);
        }
        schematic
    }

    /// The blocks, as given.
    #[must_use]
    pub fn blocks(&self) -> &[StampBlock] {
        &self.blocks
    }

    /// How many blocks it writes.
    #[must_use]
    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    /// Whether it writes nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }
}

/// What [`ChunkBuffer::scatter`] places, and where it may stand.
#[derive(Clone, Copy)]
pub struct Scatter<'a> {
    /// The terrain: positive inside the ground. A structure stands on the
    /// topmost block of a column that is solid with air over it.
    pub depth: &'a super::density::Density,
    /// Where a structure may stand, or `None` for anywhere: sampled at the
    /// centre of the surface block, positive to allow. A tree line, a biome
    /// mask, "not on a lake" — the same terms a surface fill is built from.
    pub stand: Option<&'a super::density::Density>,
    /// The structures, chosen among uniformly. Repeat one to weight it.
    pub schematics: &'a [&'a Schematic],
    /// The side of the square each candidate is drawn in, in blocks: one
    /// candidate a square, jittered within it, so two structures are never
    /// closer than a block and are `cell` apart on average.
    pub cell: u32,
    /// The share of squares that get a structure, 0 to 1.
    pub chance: f32,
    /// Mixed into the seed, so two scatters of one generator draw different
    /// squares.
    pub salt: u64,
    /// How many blocks the root sits BELOW the block over the surface: 1 puts
    /// it in the surface block itself, where a trunk's base merges into the
    /// partial block the smooth detail leaves; 0 stands it on top.
    pub sink: i32,
}

/// Every cell of a block, as a mask.
const FULL_MASK: u32 = (1_u32 << SUBNODES_PER_BLOCK as u32) - 1;

impl ChunkBuffer {
    /// A buffer full of one material, at block resolution.
    #[must_use]
    pub fn new(pos: ChunkPos, fill: MaterialId) -> Self {
        Self {
            pos,
            storage: Storage::Blocks(vec![fill; BLOCKS_PER_CHUNK]),
            fluid: crate::fluid::FluidLayer::default(),
        }
    }

    /// An empty buffer.
    #[must_use]
    pub fn air(pos: ChunkPos) -> Self {
        Self::new(pos, MaterialId::AIR)
    }

    /// The chunk this buffer is for.
    #[must_use]
    pub const fn pos(&self) -> ChunkPos {
        self.pos
    }

    /// Whether the buffer has expanded to sub-node resolution.
    ///
    /// Exposed so a generator, or a test, can assert it stayed on the cheap
    /// path — which is the kind of thing that regresses silently.
    #[must_use]
    pub const fn is_expanded(&self) -> bool {
        matches!(self.storage, Storage::SubNodes(_))
    }

    /// Heap bytes currently used.
    #[must_use]
    pub fn memory_usage(&self) -> usize {
        match &self.storage {
            Storage::Blocks(cells) | Storage::SubNodes(cells) => {
                cells.capacity() * size_of::<MaterialId>()
            }
        }
    }

    // -- block-level operations (the cheap, default path) ------------------

    /// Fills every block.
    ///
    /// Collapses an expanded buffer back to block storage, because after this
    /// there is by definition no sub-node detail left to preserve. The one case
    /// where collapsing is free.
    pub fn fill_all(&mut self, material: MaterialId) {
        self.storage = Storage::Blocks(vec![material; BLOCKS_PER_CHUNK]);
    }

    /// The material of a block. For an expanded buffer, the material of its
    /// first sub-node — enough for a generator deciding what to place on top.
    #[must_use]
    pub fn get_block(&self, local: LocalBlock) -> MaterialId {
        match &self.storage {
            Storage::Blocks(cells) => cells[local.index()],
            Storage::SubNodes(cells) => cells[Self::cell_index(local, 0, 0, 0)],
        }
    }

    /// Sets a whole block.
    ///
    /// Stays on the cheap path if the buffer has not expanded; writes all 27
    /// cells if it has.
    pub fn set_block(&mut self, local: LocalBlock, material: MaterialId) {
        match &mut self.storage {
            Storage::Blocks(cells) => cells[local.index()] = material,
            Storage::SubNodes(cells) => {
                for z in 0..SUBNODES_PER_AXIS {
                    for y in 0..SUBNODES_PER_AXIS {
                        for x in 0..SUBNODES_PER_AXIS {
                            cells[Self::cell_index(local, x, y, z)] = material;
                        }
                    }
                }
            }
        }
    }

    /// Fills every block strictly below a per-column height, in world blocks.
    ///
    /// The bread-and-butter terrain operation, and deliberately a single call:
    /// a Lua generator producing a heightmap and handing it over once costs one
    /// FFI crossing, where a per-block loop would cost 4,096.
    ///
    /// `heights` is indexed `x + 16 * z`, in world block coordinates. Columns
    /// whose height falls below this chunk leave it untouched; columns above it
    /// fill it completely.
    ///
    /// # Errors
    ///
    /// [`BufferError::WrongHeightmapSize`] if `heights` is not 256 long.
    pub fn fill_below_heightmap(
        &mut self,
        heights: &[i32],
        material: MaterialId,
    ) -> Result<(), BufferError> {
        const COLUMNS: usize = (CHUNK_BLOCKS * CHUNK_BLOCKS) as usize;
        if heights.len() != COLUMNS {
            return Err(BufferError::WrongHeightmapSize {
                expected: COLUMNS,
                found: heights.len(),
            });
        }

        let base_y = self.pos.y * CHUNK_BLOCKS as i32;
        for z in 0..CHUNK_BLOCKS {
            for x in 0..CHUNK_BLOCKS {
                let height = heights[(x + CHUNK_BLOCKS * z) as usize];
                // How many of this chunk's 16 layers are below the surface.
                let filled = (height - base_y).clamp(0, CHUNK_BLOCKS as i32);
                for y in 0..filled {
                    self.set_block(LocalBlock::new(x, y as u32, z), material);
                }
            }
        }
        Ok(())
    }

    /// Fills every block where a density field is POSITIVE.
    ///
    /// The one rule, and it is deliberately the only one: **greater than zero
    /// is solid**. A threshold parameter would be a second way to say what
    /// subtracting a constant already says, and a mod that wants a different
    /// cut-off subtracts it inside the field where the rest of the arithmetic
    /// lives.
    ///
    /// Evaluated at BLOCK resolution — one sample per block, at the block's
    /// own corner. Sub-node resolution is 27 times the samples and, measured,
    /// 2.66 ms a chunk for a single octave against a 50 ms tick shared by all
    /// simulation for all players; see `docs/worldgen-density.md`. A mod
    /// wanting sub-node detail carves it with `set_subnode` afterwards, where
    /// the cost is proportional to what it actually changes.
    ///
    /// # Errors
    ///
    /// [`BufferError::Density`] if the program will not evaluate over a chunk.
    pub fn fill_density(
        &mut self,
        density: &super::density::Density,
        seed: u64,
        material: MaterialId,
    ) -> Result<(), BufferError> {
        let side = CHUNK_BLOCKS as usize;
        let region = super::noise::Region3d {
            origin_x: (self.pos.x * CHUNK_BLOCKS as i32) as f32,
            origin_y: (self.pos.y * CHUNK_BLOCKS as i32) as f32,
            origin_z: (self.pos.z * CHUNK_BLOCKS as i32) as f32,
            step: 1.0,
            width: side,
            height: side,
            depth: side,
        };
        // **What the field cannot possibly do here, before doing it.** Most of
        // a world is not near its surface: a chunk a hundred blocks up is all
        // sky and one a hundred down is all rock, and both used to cost a full
        // evaluation to discover. `Density::bounds` answers from the program's
        // shape rather than from samples, so it cannot miss surface the way a
        // handful of probe points can — see its own docs for why that
        // distinction is the whole design.
        let bounds = density.bounds(seed, &region);
        if bounds.is_all_empty() {
            return Ok(());
        }
        if bounds.is_all_solid() {
            self.fill_all(material);
            return Ok(());
        }

        let mut field = vec![0.0f32; region.len()];
        density.evaluate(seed, &region, &mut field)?;

        // x-fastest, matching `fill_3d`'s layout and `LocalBlock::index`, so
        // no transpose and no chance of a transposed one.
        let mut index = 0;
        for z in 0..CHUNK_BLOCKS {
            for y in 0..CHUNK_BLOCKS {
                for x in 0..CHUNK_BLOCKS {
                    if field[index] > 0.0 {
                        self.set_block(LocalBlock::new(x, y, z), material);
                    }
                    index += 1;
                }
            }
        }
        Ok(())
    }

    /// Fills from a density field at SUB-NODE resolution, near the surface.
    ///
    /// # Why this is not twenty-seven times the cost
    ///
    /// A chunk is mostly not surface. Every block deep inside the ground is
    /// solid in all twenty-seven of its cells and every block in open air is
    /// empty in all of them, and sampling either at sub-node resolution asks a
    /// question whose answer was already known. Only the blocks the isosurface
    /// actually CROSSES need the finer look, and they are a shell through the
    /// chunk rather than a volume of it.
    ///
    /// So: one block-resolution pass over the chunk and one block of padding
    /// around it, then the fine pass on the blocks whose own sample disagrees
    /// in sign with one of their six neighbours. A generator pays for the shell
    /// rather than for the volume.
    ///
    /// # The two kinds of detail
    ///
    /// [`Detail::Sampled`] asks the field itself about all twenty-seven cells,
    /// so a mod that adds a high-frequency term to its density sees that term
    /// in the terrain. [`Detail::Smooth`] interpolates the block-resolution
    /// samples it already has, which costs almost nothing and cannot show
    /// anything finer than the block-scale field — it removes staircases rather
    /// than adding detail. Both produce a surface that follows the isosurface
    /// at sub-node resolution, which is the thing block resolution cannot do.
    ///
    /// # Errors
    ///
    /// [`BufferError`] if the density program cannot be evaluated.
    pub fn fill_density_detail(
        &mut self,
        density: &super::density::Density,
        seed: u64,
        material: MaterialId,
        detail: Detail,
    ) -> Result<(), BufferError> {
        // Padded by one block on every side, because whether a block is on the
        // surface is a question about its NEIGHBOURS, and the ones at the chunk
        // edge have neighbours in the next chunk. Cheaper than it looks: 18³ is
        // 1.4x the samples of 16³, and it is the only extra field work the
        // smooth path does at all.
        const PAD: usize = 1;
        let side = CHUNK_BLOCKS as usize;
        let padded = side + PAD * 2;
        let origin = [
            self.pos.x * CHUNK_BLOCKS as i32,
            self.pos.y * CHUNK_BLOCKS as i32,
            self.pos.z * CHUNK_BLOCKS as i32,
        ];
        let region = super::noise::Region3d {
            origin_x: (origin[0] - PAD as i32) as f32,
            origin_y: (origin[1] - PAD as i32) as f32,
            origin_z: (origin[2] - PAD as i32) as f32,
            step: 1.0,
            width: padded,
            height: padded,
            depth: padded,
        };
        // The same skip as the block-resolution fill, and it is worth more
        // here: this path pays 18³ samples and then a 27-sample pass on every
        // block of the surface shell. **Over the PADDED region**, so a chunk
        // called solid is solid including the ring its neighbours look at, and
        // no block can be found on the surface after the fact.
        let bounds = density.bounds(seed, &region);
        if bounds.is_all_empty() {
            return Ok(());
        }
        if bounds.is_all_solid() {
            // Every cell of every block, with no surface anywhere in it: the
            // sub-node pass would write exactly this and take 4,096 samples to
            // decide it.
            self.fill_all(material);
            return Ok(());
        }

        let mut field = vec![0.0f32; region.len()];
        let mut scratch = super::density::Scratch::default();
        density.evaluate_with(seed, &region, &mut field, &mut scratch)?;

        // x-fastest, matching `Region3d`'s own layout.
        let at = |x: usize, y: usize, z: usize| field[x + padded * (y + padded * z)];

        let mut cells = [MaterialId::AIR; SUBNODES_PER_BLOCK];
        let mut fine = vec![0.0f32; SUBNODES_PER_BLOCK];
        for z in 0..side {
            for y in 0..side {
                for x in 0..side {
                    let (px, py, pz) = (x + PAD, y + PAD, z + PAD);
                    let here = at(px, py, pz) > 0.0;
                    let surface = [
                        at(px - 1, py, pz),
                        at(px + 1, py, pz),
                        at(px, py - 1, pz),
                        at(px, py + 1, pz),
                        at(px, py, pz - 1),
                        at(px, py, pz + 1),
                    ]
                    .iter()
                    .any(|value| (*value > 0.0) != here);

                    let local = LocalBlock::new(x as u32, y as u32, z as u32);
                    if !surface {
                        // Wholly inside or wholly outside: the answer at block
                        // resolution is the answer, and writing it as a block
                        // keeps the buffer unexpanded where nothing is carved.
                        if here {
                            self.set_block(local, material);
                        }
                        continue;
                    }

                    match detail {
                        Detail::Sampled => {
                            let cell_region = super::noise::Region3d {
                                origin_x: (origin[0] + x as i32) as f32,
                                origin_y: (origin[1] + y as i32) as f32,
                                origin_z: (origin[2] + z as i32) as f32,
                                step: 1.0 / SUBNODES_PER_AXIS as f32,
                                width: SUBNODES_PER_AXIS as usize,
                                height: SUBNODES_PER_AXIS as usize,
                                depth: SUBNODES_PER_AXIS as usize,
                            };
                            density.evaluate_with(seed, &cell_region, &mut fine, &mut scratch)?;
                        }
                        Detail::Smooth => trilinear_cells(&field, padded, [px, py, pz], &mut fine),
                    }

                    // **A fill ADDS.** At block resolution a block where the
                    // field is not positive is left alone, and the cells are
                    // no different: start from what the block holds now and
                    // write the material only where the field is positive.
                    // Writing the other cells as air — which this did at
                    // first — meant a second sub-node fill wiped the first
                    // wherever its own surface crossed, so a generator could
                    // not put grass on dirt, or dirt on stone, without
                    // carving air between them.
                    for cz in 0..SUBNODES_PER_AXIS {
                        for cy in 0..SUBNODES_PER_AXIS {
                            for cx in 0..SUBNODES_PER_AXIS {
                                cells[subnode_index(cx, cy, cz)] =
                                    self.get_subnode(local, cx, cy, cz);
                            }
                        }
                    }
                    for (index, value) in fine.iter().enumerate() {
                        if *value > 0.0 {
                            cells[index] = material;
                        }
                    }
                    self.set_block_cells(local, &cells);
                }
            }
        }
        Ok(())
    }

    /// Fills from a density field through a [`Palette`], at block resolution.
    ///
    /// One evaluation of the field, however many materials the palette holds.
    /// Otherwise exactly what the same bands written as separate
    /// [`fill_density`](Self::fill_density) calls would have written, and
    /// `a_palette_writes_what_the_fills_it_replaces_would_have` holds it to
    /// that block for block.
    ///
    /// # Errors
    ///
    /// [`BufferError`] if the density program cannot be evaluated.
    pub fn fill_palette(
        &mut self,
        density: &super::density::Density,
        seed: u64,
        palette: &Palette,
    ) -> Result<(), BufferError> {
        let side = CHUNK_BLOCKS as usize;
        let region = super::noise::Region3d {
            origin_x: (self.pos.x * CHUNK_BLOCKS as i32) as f32,
            origin_y: (self.pos.y * CHUNK_BLOCKS as i32) as f32,
            origin_z: (self.pos.z * CHUNK_BLOCKS as i32) as f32,
            step: 1.0,
            width: side,
            height: side,
            depth: side,
        };
        // The same skip as the plain fill, with the same guarantee behind it,
        // generalised: a chunk whose whole range of possible values lands in
        // ONE band — including the band below the lowest, which writes nothing
        // — is decided without being evaluated.
        let bounds = density.bounds(seed, &region);
        match Self::decided_by(palette, bounds) {
            Decided::Nothing => return Ok(()),
            Decided::All(material) => {
                self.fill_all(material);
                return Ok(());
            }
            Decided::Mixed => {}
        }

        let mut field = vec![0.0f32; region.len()];
        density.evaluate(seed, &region, &mut field)?;

        let mut index = 0;
        for z in 0..CHUNK_BLOCKS {
            for y in 0..CHUNK_BLOCKS {
                for x in 0..CHUNK_BLOCKS {
                    if let Some(material) = palette.pick(field[index]) {
                        self.set_block(LocalBlock::new(x, y, z), material);
                    }
                    index += 1;
                }
            }
        }
        Ok(())
    }

    /// Fills from a density field through a [`Palette`], at sub-node resolution
    /// near every band boundary.
    ///
    /// # Every band gets a shell, not only the surface
    ///
    /// [`fill_density_detail`](Self::fill_density_detail) refines the blocks
    /// where its field changes sign. A palette has several boundaries — the
    /// surface, and the grass-to-dirt and dirt-to-stone under it — and this
    /// refines a block wherever it is on ANY of them. Inside a refined block
    /// each band is then decided the way its own fill would have decided it:
    /// by the cell's value where the block is on that band's shell, by the
    /// block's centre where it is not ([`Palette::pick_as_fills`]). That is
    /// what makes the output identical, cell for cell, to the separate detail
    /// fills it replaces — and it is not the obvious way to write it; see the
    /// comment at the write.
    ///
    /// It costs more shell blocks than a single fill, and less than the
    /// separate fills did. With [`Detail::Smooth`] a shell block is a trilinear
    /// interpolation of values already in hand, nearly free; with
    /// [`Detail::Sampled`] it is one 27-cell evaluation of the field, where the
    /// separate fills paid one PER FILL in every block that was on any of their
    /// shells.
    ///
    /// # Errors
    ///
    /// [`BufferError`] if the density program cannot be evaluated.
    pub fn fill_palette_detail(
        &mut self,
        density: &super::density::Density,
        seed: u64,
        palette: &Palette,
        detail: Detail,
    ) -> Result<(), BufferError> {
        const PAD: usize = 1;
        let side = CHUNK_BLOCKS as usize;
        let padded = side + PAD * 2;
        let origin = [
            self.pos.x * CHUNK_BLOCKS as i32,
            self.pos.y * CHUNK_BLOCKS as i32,
            self.pos.z * CHUNK_BLOCKS as i32,
        ];
        let region = super::noise::Region3d {
            origin_x: (origin[0] - PAD as i32) as f32,
            origin_y: (origin[1] - PAD as i32) as f32,
            origin_z: (origin[2] - PAD as i32) as f32,
            step: 1.0,
            width: padded,
            height: padded,
            depth: padded,
        };
        let bounds = density.bounds(seed, &region);
        match Self::decided_by(palette, bounds) {
            Decided::Nothing => return Ok(()),
            Decided::All(material) => {
                self.fill_all(material);
                return Ok(());
            }
            Decided::Mixed => {}
        }

        let mut field = vec![0.0f32; region.len()];
        let mut scratch = super::density::Scratch::default();
        density.evaluate_with(seed, &region, &mut field, &mut scratch)?;

        let at = |x: usize, y: usize, z: usize| field[x + padded * (y + padded * z)];

        let mut cells = [MaterialId::AIR; SUBNODES_PER_BLOCK];
        let mut fine = vec![0.0f32; SUBNODES_PER_BLOCK];
        for z in 0..side {
            for y in 0..side {
                for x in 0..side {
                    let (px, py, pz) = (x + PAD, y + PAD, z + PAD);
                    let centre = at(px, py, pz);
                    let neighbours = [
                        at(px - 1, py, pz),
                        at(px + 1, py, pz),
                        at(px, py - 1, pz),
                        at(px, py + 1, pz),
                        at(px, py, pz - 1),
                        at(px, py, pz + 1),
                    ];
                    // **Which of the band boundaries this block is on**, one
                    // answer per band, because the separate fills this
                    // replaces each asked only about their own. A block is on
                    // band k's shell if a neighbour is on the other side of
                    // band k's threshold from the centre; it is refined at all
                    // if it is on any.
                    let mut on_shell = [false; MAX_PALETTE_BANDS];
                    for (k, (threshold, _)) in palette.bands().iter().enumerate() {
                        let here = centre > *threshold;
                        on_shell[k] = neighbours.iter().any(|value| (*value > *threshold) != here);
                    }
                    let refined = on_shell[..palette.bands().len()].iter().any(|on| *on);

                    let local = LocalBlock::new(x as u32, y as u32, z as u32);
                    if !refined {
                        if let Some(material) = palette.pick(centre) {
                            self.set_block(local, material);
                        }
                        continue;
                    }

                    match detail {
                        Detail::Sampled => {
                            let cell_region = super::noise::Region3d {
                                origin_x: (origin[0] + x as i32) as f32,
                                origin_y: (origin[1] + y as i32) as f32,
                                origin_z: (origin[2] + z as i32) as f32,
                                step: 1.0 / SUBNODES_PER_AXIS as f32,
                                width: SUBNODES_PER_AXIS as usize,
                                height: SUBNODES_PER_AXIS as usize,
                                depth: SUBNODES_PER_AXIS as usize,
                            };
                            density.evaluate_with(seed, &cell_region, &mut fine, &mut scratch)?;
                        }
                        Detail::Smooth => trilinear_cells(&field, padded, [px, py, pz], &mut fine),
                    }

                    // A fill ADDS, cell by cell, for the reason the plain
                    // detail fill gives at the same spot.
                    for cz in 0..SUBNODES_PER_AXIS {
                        for cy in 0..SUBNODES_PER_AXIS {
                            for cx in 0..SUBNODES_PER_AXIS {
                                cells[subnode_index(cx, cy, cz)] =
                                    self.get_subnode(local, cx, cy, cz);
                            }
                        }
                    }
                    // **Per band, the cell's value where this block is on that
                    // band's shell and the block's centre where it is not.**
                    // That is exactly what the sequence of fills did: each
                    // fill refined the blocks on ITS shell and wrote the rest
                    // as blocks, so a block on the dirt-to-stone shell but not
                    // the surface's was given the surface material whole and
                    // then had stone carved into it. Asking the cell about
                    // every band instead — the obvious way to write this, and
                    // the first way it was written — refined the surface in
                    // exactly the blocks where a deeper boundary happened to
                    // pass, and left it block-shaped in their neighbours. A
                    // surface whose smoothness depends on where the dirt runs
                    // out is not a smooth surface.
                    for (index, value) in fine.iter().enumerate() {
                        if let Some(material) = palette.pick_as_fills(centre, *value, &on_shell) {
                            cells[index] = material;
                        }
                    }
                    self.set_block_cells(local, &cells);
                }
            }
        }
        Ok(())
    }

    /// What a chunk's bound decides on its own, if anything.
    ///
    /// The plain fill's `is_all_empty` / `is_all_solid` pair, for a table: a
    /// bound whose two ends land in the same band means every value between
    /// them does too, because bands are contiguous in value.
    fn decided_by(palette: &Palette, bounds: super::density::Interval) -> Decided {
        if bounds.high <= palette.floor() {
            return Decided::Nothing;
        }
        match (palette.pick(bounds.low), palette.pick(bounds.high)) {
            (Some(low), Some(high)) if low == high => Decided::All(low),
            _ => Decided::Mixed,
        }
    }

    /// Paints the surface's layers from ONE depth field and ONE code field.
    ///
    /// # Why this is not several `fill_density_detail` calls
    ///
    /// A surface is made of layers — turf over dirt, snow over that, ice on
    /// the floors — and each is a band of the terrain field under a
    /// condition of its own. As separate fills, every one re-evaluates the
    /// terrain: a biome with eight surface materials paid for its terrain
    /// eight times a chunk, six octaves of noise a sample, and a chunk took
    /// longer to generate than a tick. Here the terrain (`depth`, positive
    /// underground, like any fill's field) is evaluated once, and which
    /// layer a column gets is a second, cheap field (`code`) evaluated once
    /// at block resolution: its value rounded to an integer names a code,
    /// and the layers say which material each code's depth bands take.
    ///
    /// # The rule
    ///
    /// For every cell whose depth is positive, the first layer whose `code`
    /// is the block's code and whose `from..to` holds the cell's depth gives
    /// the material; a cell no layer claims is left as it was, so the
    /// generator's own base fill shows through. Depth is smooth at the cells
    /// (the trilinear interpolation of the block samples, as `Detail::Smooth`
    /// does) so a band's edge follows the surface; the code is the BLOCK's,
    /// because a code is a category and interpolating categories means
    /// nothing. A block is on the shell — worth its 27 cells — where its
    /// depth changes sign against a neighbour or its code differs from one.
    ///
    /// # Errors
    ///
    /// [`BufferError`] if either field cannot be evaluated.
    pub fn fill_layers(
        &mut self,
        depth: &super::density::Density,
        code: &super::density::Density,
        seed: u64,
        layers: &[Layer],
    ) -> Result<(), BufferError> {
        const PAD: usize = 1;
        let side = CHUNK_BLOCKS as usize;
        let padded = side + PAD * 2;
        let origin = [
            self.pos.x * CHUNK_BLOCKS as i32,
            self.pos.y * CHUNK_BLOCKS as i32,
            self.pos.z * CHUNK_BLOCKS as i32,
        ];
        let region = super::noise::Region3d {
            origin_x: (origin[0] - PAD as i32) as f32,
            origin_y: (origin[1] - PAD as i32) as f32,
            origin_z: (origin[2] - PAD as i32) as f32,
            step: 1.0,
            width: padded,
            height: padded,
            depth: padded,
        };
        // Nothing to paint where no layer reaches: above the surface, or
        // deeper than the deepest band.
        let deepest = layers.iter().map(|layer| layer.to).fold(0.0_f32, f32::max);
        let bounds = depth.bounds(seed, &region);
        if bounds.is_all_empty() || bounds.low >= deepest {
            return Ok(());
        }

        let mut field = vec![0.0f32; region.len()];
        let mut codes = vec![0.0f32; region.len()];
        let mut scratch = super::density::Scratch::default();
        depth.evaluate_with(seed, &region, &mut field, &mut scratch)?;
        code.evaluate_with(seed, &region, &mut codes, &mut scratch)?;

        let at = |x: usize, y: usize, z: usize| field[x + padded * (y + padded * z)];
        let code_at = |x: usize, y: usize, z: usize| {
            // Rounded the deterministic way (charter rule 4 bans `round`):
            // the floor of the value plus a half, in integer arithmetic.
            super::floor_to_i32(codes[x + padded * (y + padded * z)] + 0.5)
        };
        let pick = |code: i32, depth: f32| {
            layers
                .iter()
                .find(|layer| layer.code == code && layer.from <= depth && depth < layer.to)
                .map(|layer| layer.material)
        };

        let mut cells = [MaterialId::AIR; SUBNODES_PER_BLOCK];
        let mut fine = vec![0.0f32; SUBNODES_PER_BLOCK];
        for z in 0..side {
            for y in 0..side {
                for x in 0..side {
                    let (px, py, pz) = (x + PAD, y + PAD, z + PAD);
                    let here = at(px, py, pz) > 0.0;
                    let here_code = code_at(px, py, pz);
                    let neighbours = [
                        (px - 1, py, pz),
                        (px + 1, py, pz),
                        (px, py - 1, pz),
                        (px, py + 1, pz),
                        (px, py, pz - 1),
                        (px, py, pz + 1),
                    ];
                    let shell = neighbours.iter().any(|&(nx, ny, nz)| {
                        (at(nx, ny, nz) > 0.0) != here || code_at(nx, ny, nz) != here_code
                    });
                    let local = LocalBlock::new(x as u32, y as u32, z as u32);
                    if !shell {
                        if here && let Some(material) = pick(here_code, at(px, py, pz)) {
                            self.set_block(local, material);
                        }
                        continue;
                    }
                    trilinear_cells(&field, padded, [px, py, pz], &mut fine);
                    for cz in 0..SUBNODES_PER_AXIS {
                        for cy in 0..SUBNODES_PER_AXIS {
                            for cx in 0..SUBNODES_PER_AXIS {
                                cells[subnode_index(cx, cy, cz)] =
                                    self.get_subnode(local, cx, cy, cz);
                            }
                        }
                    }
                    let mut written = false;
                    for (index, value) in fine.iter().enumerate() {
                        if *value > 0.0
                            && let Some(material) = pick(here_code, *value)
                        {
                            cells[index] = material;
                            written = true;
                        }
                    }
                    if written {
                        self.set_block_cells(local, &cells);
                    }
                }
            }
        }
        Ok(())
    }

    /// Stands a run of cells of `material` on every surface the buffer holds.
    ///
    /// # Why this is a fill of its own and not a field
    ///
    /// Ground cover — grass, ferns — wants three things at once: to stand ON
    /// the surface the ground fill made, neither floating over a smooth slope
    /// nor sunk into it; to be a cell or two tall; and to stay inside ONE
    /// block, so a tuft is never two stacked blocks that highlight and dig
    /// apart. A density field can say none of that. It has no idea which block
    /// a sample is in, and a run two cells tall in a block whose surface sits
    /// at an arbitrary cell contains the block's one sample point in a third
    /// of columns — so [`fill_density_detail`](Self::fill_density_detail)'s
    /// surface-shell test, built from those samples, misses the other two
    /// thirds in stripes that follow the contours. The buffer, on the other
    /// hand, knows exactly where every surface is: it is wherever an occupied
    /// cell has an empty one above it.
    ///
    /// # The rule
    ///
    /// For every cell column, at the LOWEST cell in each block that is empty
    /// with an occupied cell below it, and where `take` is positive at that
    /// cell (everywhere, with no `take`), that cell and up to `cells - 1`
    /// empty cells above it become `material` — never crossing into the block
    /// above, and never overwriting a cell that holds something. Where the
    /// surface is a block's top cell the run is that one cell. Cover written
    /// by this call is not a surface for it: a run never stands on another
    /// run.
    ///
    /// **The lowest, not every one**, which matters only for a block holding
    /// two surfaces at once — a one-cell shelf, air over stone over air over
    /// stone inside three cells. The shelf's upper face gets nothing. It is a
    /// rare shape and the lower face is the one a player stands on, so this is
    /// a deliberate limit rather than an oversight; the alternative is runs
    /// interleaved with the ground they grow from, inside one block, which is
    /// the thing this call exists to avoid.
    ///
    /// `cells` is clamped to 1..=3. `take` is evaluated at cell resolution and
    /// only in the blocks that hold a surface, the economy the detail fill
    /// makes.
    ///
    /// The chunk's bottom cell row has nothing below it to look at, so a
    /// surface exactly on the chunk floor gets no cover from this chunk: the
    /// neighbouring chunk is not available at generation. One row in
    /// forty-eight.
    ///
    /// # Errors
    ///
    /// [`BufferError`] if `take` cannot be evaluated.
    pub fn fill_cover(
        &mut self,
        material: MaterialId,
        cells: u32,
        take: Option<&super::density::Density>,
        seed: u64,
    ) -> Result<(), BufferError> {
        let cells = cells.clamp(1, SUBNODES_PER_AXIS);
        // **Nothing to stand on, and nothing to look at.** Cover grows where an
        // empty cell sits on an occupied one, and a buffer holding one material
        // everywhere has no such cell anywhere in it — the chunk floor is the
        // only boundary left, and this call already cannot reach below itself.
        //
        // Worth its own check because the scan below reads all 27 cells of all
        // 4,096 blocks whatever they hold: measured at 0.047 ms on a chunk of
        // plain air, against 0.011 ms to generate one. A generator calling this
        // over a streamed column pays that on every chunk of empty sky above
        // its terrain and every chunk of solid rock below it, which is most of
        // them.
        if let Storage::Blocks(blocks) = &self.storage
            && let Some(first) = blocks.first()
            && blocks.iter().all(|block| block == first)
        {
            return Ok(());
        }
        let origin = [
            self.pos.x * CHUNK_BLOCKS as i32,
            self.pos.y * CHUNK_BLOCKS as i32,
            self.pos.z * CHUNK_BLOCKS as i32,
        ];
        let mut fine = vec![0.0f32; SUBNODES_PER_BLOCK];
        let mut scratch = super::density::Scratch::default();
        let mut block_cells: Cells = EMPTY_CELLS;

        for z in 0..CHUNK_BLOCKS {
            for x in 0..CHUNK_BLOCKS {
                // Whether the cell under each of this column's nine cell
                // columns is occupied, carried up block by block. Starts
                // false: the chunk floor has no cell below it.
                let mut below = [false; (SUBNODES_PER_AXIS * SUBNODES_PER_AXIS) as usize];
                for y in 0..CHUNK_BLOCKS {
                    let local = LocalBlock::new(x, y, z);
                    for cz in 0..SUBNODES_PER_AXIS {
                        for cy in 0..SUBNODES_PER_AXIS {
                            for cx in 0..SUBNODES_PER_AXIS {
                                block_cells[subnode_index(cx, cy, cz)] =
                                    self.get_subnode(local, cx, cy, cz);
                            }
                        }
                    }
                    // The bases: empty cells standing on an occupied one.
                    // Read BEFORE anything is written, so a run this call
                    // writes is never the ground for another.
                    let mut bases: [Option<u32>; (SUBNODES_PER_AXIS * SUBNODES_PER_AXIS) as usize] =
                        [None; (SUBNODES_PER_AXIS * SUBNODES_PER_AXIS) as usize];
                    let mut any = false;
                    for cz in 0..SUBNODES_PER_AXIS {
                        for cx in 0..SUBNODES_PER_AXIS {
                            let column = (cz * SUBNODES_PER_AXIS + cx) as usize;
                            let mut under = below[column];
                            for cy in 0..SUBNODES_PER_AXIS {
                                let here =
                                    block_cells[subnode_index(cx, cy, cz)] != MaterialId::AIR;
                                if !here && under && bases[column].is_none() {
                                    bases[column] = Some(cy);
                                    any = true;
                                }
                                under = here;
                            }
                            below[column] = under;
                        }
                    }
                    if !any {
                        continue;
                    }
                    if let Some(take) = take {
                        let cell_region = super::noise::Region3d {
                            origin_x: (origin[0] + x as i32) as f32,
                            origin_y: (origin[1] + y as i32) as f32,
                            origin_z: (origin[2] + z as i32) as f32,
                            step: 1.0 / SUBNODES_PER_AXIS as f32,
                            width: SUBNODES_PER_AXIS as usize,
                            height: SUBNODES_PER_AXIS as usize,
                            depth: SUBNODES_PER_AXIS as usize,
                        };
                        take.evaluate_with(seed, &cell_region, &mut fine, &mut scratch)?;
                    }
                    let mut written = false;
                    for cz in 0..SUBNODES_PER_AXIS {
                        for cx in 0..SUBNODES_PER_AXIS {
                            let column = (cz * SUBNODES_PER_AXIS + cx) as usize;
                            let Some(base) = bases[column] else {
                                continue;
                            };
                            if take.is_some() && fine[subnode_index(cx, base, cz)] <= 0.0 {
                                continue;
                            }
                            // Up from the base, inside this block, through
                            // empty cells only.
                            for cy in base..(base + cells).min(SUBNODES_PER_AXIS) {
                                let index = subnode_index(cx, cy, cz);
                                if block_cells[index] != MaterialId::AIR {
                                    break;
                                }
                                block_cells[index] = material;
                                written = true;
                            }
                        }
                    }
                    if written {
                        self.set_block_cells(local, &block_cells);
                    }
                }
            }
        }
        Ok(())
    }

    /// Writes one block by WORLD position, ignoring anything outside this chunk.
    ///
    /// # Why a generator needs to aim outside its own chunk
    ///
    /// A structure — a tree, a hut, a ruin — is rooted at one place and reaches
    /// out from it, and nothing makes that reach stop at a chunk boundary. The
    /// obvious fix is to let a generator write into its neighbours and have the
    /// engine hold those writes until those chunks are made, and that fix is
    /// **wrong**: chunks are generated in whatever order players walk towards
    /// them, so a chunk made before its neighbour would be missing the half of
    /// a tree that neighbour was going to contribute, and made after it would
    /// have it. Same seed, different world. Charter rule 4 is not only about
    /// floats.
    ///
    /// The order-independent shape is the other way round: when generating a
    /// chunk, run the structure pass for **every chunk within reach** — each
    /// one's structures are a pure function of its own position and the seed,
    /// via `rng_stream` — and write all of them in world coordinates. The ones
    /// that land here are kept and the rest are dropped, and the same chunk
    /// comes out whatever was generated before it. Every neighbour does the
    /// same work and keeps a different slice of it.
    ///
    /// That is what this call is for: **dropping is the feature**. A generator
    /// placing a structure whose root is two chunks away should not have to
    /// know which of its blocks fall inside, and asking it to check would be
    /// asking every mod to reimplement this and get the edges right.
    ///
    /// Returns whether the write landed, which a caller may ignore.
    pub fn set_block_world(&mut self, at: crate::BlockPos, material: MaterialId) -> bool {
        if at.chunk() != self.pos {
            return false;
        }
        self.set_block(at.local(), material);
        true
    }

    /// Writes one sub-node cell by WORLD cell position, ignoring anything
    /// outside this chunk.
    ///
    /// The sub-node half of [`Self::set_block_world`], for the same reason and
    /// with the same rule. Expands the buffer only when the write lands, so a
    /// generator running the structure pass for a neighbourhood does not pay
    /// 27x for the chunks it contributes nothing to.
    pub fn set_subnode_world(&mut self, at: crate::SubNodePos, material: MaterialId) -> bool {
        let block = crate::BlockPos::new(
            at.x.div_euclid(SUBNODES_PER_AXIS as i32),
            at.y.div_euclid(SUBNODES_PER_AXIS as i32),
            at.z.div_euclid(SUBNODES_PER_AXIS as i32),
        );
        if block.chunk() != self.pos {
            return false;
        }
        self.set_subnode(
            block.local(),
            at.x.rem_euclid(SUBNODES_PER_AXIS as i32) as u32,
            at.y.rem_euclid(SUBNODES_PER_AXIS as i32) as u32,
            at.z.rem_euclid(SUBNODES_PER_AXIS as i32) as u32,
            material,
        );
        true
    }

    /// Stamps schematics on the surface, across chunk edges, at generation.
    ///
    /// **The structure pass §5.2 describes, done natively.** Every chunk within
    /// a structure's reach runs the same pass and keeps its own slice: the
    /// candidates are drawn per `cell`-sized square of the ground from a hash
    /// of the square and the seed, so a square answers the same way from every
    /// chunk that asks, and nothing is held between chunks. Each candidate's
    /// surface is found by evaluating `depth` down its column — only over the
    /// window of heights whose structure could touch this chunk, a few dozen
    /// samples — and `stand` is sampled once at that surface. The schematic is
    /// then written by cell, clipped to this chunk: a merge write, as
    /// [`Self::set_subnode_world`] is.
    ///
    /// Asked for by the alpine forest: trees grown by random tick after a chunk
    /// loads stood in patches where the player had waited, and a chunk of
    /// forest is a hundred trees, which is minutes of ticks. A forest belongs
    /// to the terrain, so it is made with the terrain.
    ///
    /// Returns how many structures wrote at least one block into this chunk.
    ///
    /// # Errors
    ///
    /// [`BufferError`] if a density program cannot be evaluated.
    #[allow(clippy::too_many_lines)]
    pub fn scatter(&mut self, seed: u64, scatter: &Scatter<'_>) -> Result<usize, BufferError> {
        if scatter.schematics.is_empty() || scatter.cell == 0 {
            return Ok(0);
        }
        let (mut reach_x, mut reach_z, mut lowest, mut highest) = (0, 0, 0, 0);
        for schematic in scatter.schematics {
            reach_x = reach_x.max(schematic.reach_x);
            reach_z = reach_z.max(schematic.reach_z);
            lowest = lowest.min(schematic.lowest);
            highest = highest.max(schematic.highest);
        }
        let side = CHUNK_BLOCKS as i32;
        let cell = scatter.cell as i32;
        let (x0, y0, z0) = (self.pos.x * side, self.pos.y * side, self.pos.z * side);
        let (x1, y1, z1) = (x0 + side, y0 + side, z0 + side);
        // The surface blocks whose structure can reach into this chunk: the
        // root is `sink` below the block over the surface, and the structure
        // spans `lowest..=highest` from the root.
        let surface_lo = y0 - highest - 1 + scatter.sink;
        let surface_hi = y1 - 1 - lowest - 1 + scatter.sink;
        if surface_hi < surface_lo {
            return Ok(0);
        }
        // One sample more than the window, for the air over its top block.
        let samples = (surface_hi - surface_lo + 2) as usize;
        let mut field = vec![0.0_f32; samples];
        let mut scratch = super::density::Scratch::default();
        let mut placed = 0;
        for cz in (z0 - reach_z).div_euclid(cell)..=(z1 - 1 + reach_z).div_euclid(cell) {
            for cx in (x0 - reach_x).div_euclid(cell)..=(x1 - 1 + reach_x).div_euclid(cell) {
                // **Every draw happens whether or not the candidate is kept**,
                // in a fixed order, so a square's answer is the same from every
                // chunk that asks about it.
                let mut rng =
                    super::rng::Xoshiro256PlusPlus::seed_from_u64(super::rng::StreamRng::seed_for(
                        seed ^ scatter.salt,
                        ChunkPos::new(cx, 0, cz),
                        "scatter",
                    ));
                let keep = rng.next_f32() < scatter.chance;
                let x = cx * cell + rng.below(u64::from(scatter.cell)) as i32;
                let z = cz * cell + rng.below(u64::from(scatter.cell)) as i32;
                let which = rng.below(scatter.schematics.len() as u64) as usize;
                if !keep {
                    continue;
                }
                let schematic = scatter.schematics[which];
                if x + schematic.reach_x < x0
                    || x - schematic.reach_x >= x1
                    || z + schematic.reach_z < z0
                    || z - schematic.reach_z >= z1
                {
                    continue;
                }
                let region = super::noise::Region3d {
                    origin_x: x as f32 + 0.5,
                    origin_y: surface_lo as f32 + 0.5,
                    origin_z: z as f32 + 0.5,
                    step: 1.0,
                    width: 1,
                    height: samples,
                    depth: 1,
                };
                scatter
                    .depth
                    .evaluate_with(seed, &region, &mut field, &mut scratch)?;
                // The topmost surface in the window: solid, with air over it.
                let surface = (0..samples - 1)
                    .rev()
                    .find(|&i| field[i] > 0.0 && field[i + 1] <= 0.0)
                    .map(|i| surface_lo + i as i32);
                let Some(surface) = surface else {
                    continue;
                };
                if let Some(stand) = scatter.stand
                    && stand.sample(seed, x as f32 + 0.5, surface as f32 + 0.5, z as f32 + 0.5)?
                        <= 0.0
                {
                    continue;
                }
                let base = surface + 1 - scatter.sink;
                let mut landed = false;
                for block in schematic.blocks() {
                    let at = crate::BlockPos::new(x + block.dx, base + block.dy, z + block.dz);
                    if at.chunk() != self.pos {
                        continue;
                    }
                    let local = at.local();
                    if block.mask & FULL_MASK == FULL_MASK {
                        self.set_block(local, block.material);
                    } else {
                        for bit in 0..SUBNODES_PER_BLOCK as u32 {
                            if block.mask & (1 << bit) != 0 {
                                self.set_subnode(
                                    local,
                                    bit % 3,
                                    (bit / 3) % 3,
                                    bit / 9,
                                    block.material,
                                );
                            }
                        }
                    }
                    landed = true;
                }
                if landed {
                    placed += 1;
                }
            }
        }
        Ok(placed)
    }

    // -- sub-node operations (the opt-in path) -----------------------------

    /// The material of one sub-node cell.
    ///
    /// Reading does **not** expand the buffer: an unexpanded block answers for
    /// all 27 of its cells.
    #[must_use]
    pub fn get_subnode(&self, local: LocalBlock, x: u32, y: u32, z: u32) -> MaterialId {
        match &self.storage {
            Storage::Blocks(cells) => cells[local.index()],
            Storage::SubNodes(cells) => cells[Self::cell_index(local, x, y, z)],
        }
    }

    /// Sets one sub-node cell.
    ///
    /// **This is the call that expands the buffer**, and the only kind that
    /// does. A generator that never calls it never pays for sub-nodes.
    pub fn set_subnode(&mut self, local: LocalBlock, x: u32, y: u32, z: u32, material: MaterialId) {
        self.expand();
        let Storage::SubNodes(cells) = &mut self.storage else {
            unreachable!("expand() guarantees sub-node storage");
        };
        cells[Self::cell_index(local, x, y, z)] = material;
    }

    /// Sets all 27 cells of a block at once.
    ///
    /// Expands, since a per-cell array is sub-node detail by definition.
    pub fn set_block_cells(&mut self, local: LocalBlock, block_cells: &Cells) {
        self.expand();
        let Storage::SubNodes(cells) = &mut self.storage else {
            unreachable!("expand() guarantees sub-node storage");
        };
        for z in 0..SUBNODES_PER_AXIS {
            for y in 0..SUBNODES_PER_AXIS {
                for x in 0..SUBNODES_PER_AXIS {
                    cells[Self::cell_index(local, x, y, z)] = block_cells[subnode_index(x, y, z)];
                }
            }
        }
    }

    /// Copies a block region from another buffer.
    ///
    /// Expands only if the source has: blitting block-resolution content into a
    /// block-resolution buffer should stay cheap, which is what makes structure
    /// placement affordable.
    ///
    /// The region is clipped to both buffers, so a structure overhanging a chunk
    /// boundary is handled by passing the same call to each chunk.
    pub fn blit(&mut self, source: &Self, from: LocalBlock, to: LocalBlock, size: [u32; 3]) {
        if source.is_expanded() {
            self.expand();
        }

        for dz in 0..size[2] {
            for dy in 0..size[1] {
                for dx in 0..size[0] {
                    let (Some(src), Some(dst)) =
                        (Self::offset(from, dx, dy, dz), Self::offset(to, dx, dy, dz))
                    else {
                        continue;
                    };

                    if source.is_expanded() || self.is_expanded() {
                        for z in 0..SUBNODES_PER_AXIS {
                            for y in 0..SUBNODES_PER_AXIS {
                                for x in 0..SUBNODES_PER_AXIS {
                                    let material = source.get_subnode(src, x, y, z);
                                    self.set_subnode(dst, x, y, z, material);
                                }
                            }
                        }
                    } else {
                        self.set_block(dst, source.get_block(src));
                    }
                }
            }
        }
    }

    /// Fills every block below `level` with `fluid`, around the terrain.
    ///
    /// **An ocean is a layer, not a material.** A block holds a material and,
    /// separately, a volume of one fluid, so filling a sea is not a matter of
    /// setting blocks to water — it is putting a volume into the space the
    /// ground leaves. Sub-Node Contract §4.
    ///
    /// How much goes in a block is what `Fluid::room_in` says the terrain
    /// leaves it: a solid block takes nothing, an empty one takes all 27 cells,
    /// and a half-carved one takes what is left. So a shoreline comes out of
    /// the terrain rather than having to be described.
    ///
    /// `level` is a WORLD height, like a heightmap's, so a sea level is one
    /// number for the whole world and does not restart at every chunk.
    ///
    /// **Placed, not simulated.** The conserved solver moves what exists and
    /// creates nothing (see `docs/subnode-contract.md` §4), so worldgen is the
    /// only place an ocean can come from. It settles from here like any other
    /// volume.
    pub fn fill_fluid_below(&mut self, level: i32, fluid: crate::fluid::FluidId) {
        let base_y = self.pos.y * CHUNK_BLOCKS as i32;
        for y in 0..CHUNK_BLOCKS {
            if base_y + y as i32 >= level {
                break;
            }
            for z in 0..CHUNK_BLOCKS {
                for x in 0..CHUNK_BLOCKS {
                    let local = LocalBlock::new(x, y, z);
                    let room = crate::fluid::MAX_VOLUME.saturating_sub(self.filled_cells(local));
                    if room > 0 {
                        self.fluid.set(local, crate::fluid::Fluid::new(fluid, room));
                    }
                }
            }
        }
    }

    /// How many of a block's 27 cells hold terrain.
    ///
    /// **Whole-block first.** A buffer that has never been chiselled stores one
    /// material a block, so the answer is 0 or 27 without touching a cell —
    /// which is every block of a heightmap or density world. The per-cell count
    /// is for the expanded case and costs 27 reads only there.
    fn filled_cells(&self, local: LocalBlock) -> u32 {
        if !self.is_expanded() {
            return if self.get_block(local).is_air() {
                0
            } else {
                crate::fluid::MAX_VOLUME
            };
        }
        let mut filled = 0;
        for z in 0..SUBNODES_PER_AXIS {
            for y in 0..SUBNODES_PER_AXIS {
                for x in 0..SUBNODES_PER_AXIS {
                    if !self.get_subnode(local, x, y, z).is_air() {
                        filled += 1;
                    }
                }
            }
        }
        filled
    }

    /// The fluid this buffer holds, for the caller that stores it.
    #[must_use]
    pub const fn fluid(&self) -> &crate::fluid::FluidLayer {
        &self.fluid
    }

    // -- conversion --------------------------------------------------------

    /// Builds the palette-compressed [`Chunk`].
    ///
    /// Canonicalisation happens inside [`Chunk::set_block_local`], so a buffer
    /// full of blocks that happen to be uniform comes out as `Uniform` entries
    /// and a chiselled one comes out as `Partial` or `Mixed` — whichever is
    /// correct — without the generator having to know the difference.
    #[must_use]
    pub fn to_chunk(&self) -> Chunk {
        match &self.storage {
            Storage::Blocks(materials) => {
                // Start from the first block's material so a uniform buffer
                // produces a one-entry palette with no index storage at all,
                // rather than building 4,096 identical writes over air.
                let mut chunk = Chunk::new(self.pos, materials[0]);
                for (index, &material) in materials.iter().enumerate() {
                    if material != materials[0] {
                        chunk.set_block_local(
                            LocalBlock::from_index(index),
                            BlockValue::Uniform(material),
                        );
                    }
                }
                chunk
            }
            Storage::SubNodes(_) => {
                let mut chunk = Chunk::air(self.pos);
                for index in 0..BLOCKS_PER_CHUNK {
                    let local = LocalBlock::from_index(index);
                    let mut block_cells = EMPTY_CELLS;
                    for z in 0..SUBNODES_PER_AXIS {
                        for y in 0..SUBNODES_PER_AXIS {
                            for x in 0..SUBNODES_PER_AXIS {
                                block_cells[subnode_index(x, y, z)] =
                                    self.get_subnode(local, x, y, z);
                            }
                        }
                    }
                    chunk.set_block_local(local, BlockValue::Cells(block_cells));
                }
                chunk
            }
        }
    }

    // -- internals ---------------------------------------------------------

    /// Expands to sub-node storage, if not already expanded.
    fn expand(&mut self) {
        let Storage::Blocks(blocks) = &self.storage else {
            return;
        };

        let mut cells = vec![MaterialId::AIR; CELLS_PER_CHUNK];
        for (index, &material) in blocks.iter().enumerate() {
            let local = LocalBlock::from_index(index);
            for z in 0..SUBNODES_PER_AXIS {
                for y in 0..SUBNODES_PER_AXIS {
                    for x in 0..SUBNODES_PER_AXIS {
                        cells[Self::cell_index(local, x, y, z)] = material;
                    }
                }
            }
        }
        self.storage = Storage::SubNodes(cells);
    }

    /// Flat index of a sub-node cell. x-fastest, matching every other layout in
    /// the engine.
    const fn cell_index(local: LocalBlock, x: u32, y: u32, z: u32) -> usize {
        let cell_x = local.x * SUBNODES_PER_AXIS + x;
        let cell_y = local.y * SUBNODES_PER_AXIS + y;
        let cell_z = local.z * SUBNODES_PER_AXIS + z;
        (cell_x + CHUNK_SUBNODES * cell_y + CHUNK_SUBNODES * CHUNK_SUBNODES * cell_z) as usize
    }

    /// A block offset from `base`, or `None` if it leaves the chunk.
    const fn offset(base: LocalBlock, dx: u32, dy: u32, dz: u32) -> Option<LocalBlock> {
        let x = base.x + dx;
        let y = base.y + dy;
        let z = base.z + dz;
        if x >= CHUNK_BLOCKS || y >= CHUNK_BLOCKS || z >= CHUNK_BLOCKS {
            return None;
        }
        Some(LocalBlock { x, y, z })
    }
}

/// A buffer operation was given something it could not use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum BufferError {
    /// A heightmap was the wrong length.
    #[error("heightmap holds {found} columns but a chunk has {expected}")]
    WrongHeightmapSize {
        /// Columns a chunk has.
        expected: usize,
        /// Columns supplied.
        found: usize,
    },
    /// A density program could not be evaluated over this chunk.
    #[error(transparent)]
    Density(#[from] super::density::DensityError),
}

#[cfg(test)]
mod tests {
    use super::*;

    const STONE: MaterialId = MaterialId(2);
    const DIRT: MaterialId = MaterialId(3);

    fn origin() -> ChunkPos {
        ChunkPos::new(0, 0, 0)
    }

    /// A field that is positive below a sloping plane: y < x/4, so the surface
    /// crosses blocks at a shallow angle that block resolution has to stair-step
    /// and sub-node resolution does not.
    fn slope() -> super::super::density::Density {
        use super::super::density::{Axis, Op};
        super::super::density::Density::compile(vec![
            Op::Coordinate(Axis::X),
            Op::Constant(0.25),
            Op::Multiply,
            Op::Coordinate(Axis::Y),
            Op::Subtract,
        ])
        .expect("compile")
    }

    /// Asserts two buffers hold the same cells, and says WHICH when they do not.
    ///
    /// `assert_eq!` on two buffers prints both — 110,592 cells each — and a
    /// failure nobody can read is a failure nobody investigates. This reports a
    /// count and the first few differing cells with what each side put there.
    fn assert_same_cells(a: &ChunkBuffer, b: &ChunkBuffer, context: &str) {
        if a == b {
            return;
        }
        let (ca, cb) = (a.to_chunk(), b.to_chunk());
        let origin = crate::BlockPos::from_chunk_corner(a.pos());
        let mut differing = 0;
        let mut examples = Vec::new();
        for x in 0..CHUNK_BLOCKS as i32 {
            for y in 0..CHUNK_BLOCKS as i32 {
                for z in 0..CHUNK_BLOCKS as i32 {
                    let pos = crate::BlockPos::new(origin.x + x, origin.y + y, origin.z + z);
                    let (va, vb) = (ca.get_block(pos), cb.get_block(pos));
                    for cell in 0..SUBNODES_PER_BLOCK {
                        let (ma, mb) = (
                            va.as_ref().map(|v| v.subnode(cell)),
                            vb.as_ref().map(|v| v.subnode(cell)),
                        );
                        if ma != mb {
                            differing += 1;
                            if examples.len() < 6 {
                                examples.push(((x, y, z), cell, ma, mb));
                            }
                        }
                    }
                }
            }
        }
        panic!("{context}: {differing} cells differ (block, cell, left, right), e.g. {examples:?}");
    }

    /// `noise - y`, the shape whose value is depth below the surface.
    fn strata_field() -> super::super::density::Density {
        use super::super::density::{Axis, Op};
        use super::super::noise::{Fractal, FractalParams};
        super::super::density::Density::compile(vec![
            Op::Noise {
                params: FractalParams {
                    fractal: Fractal::Fbm,
                    octaves: 3,
                    frequency: 0.02,
                    lacunarity: 2.0,
                    gain: 0.5,
                },
                amplitude: 10.0,
                stream: 5,
            },
            Op::Coordinate(Axis::Y),
            Op::Subtract,
        ])
        .expect("compile")
    }

    /// Six octaves and 24 blocks of relief, lowered by `depth`: the shape of a
    /// real terrain mod's field, and steep enough that the surface crosses
    /// several blocks per block. `depth` of zero is the surface itself.
    fn steep_field(depth: f32) -> super::super::density::Density {
        use super::super::density::{Axis, Op};
        use super::super::noise::{Fractal, FractalParams};
        let mut ops = vec![
            Op::Noise {
                params: FractalParams {
                    fractal: Fractal::Fbm,
                    octaves: 6,
                    frequency: 0.01,
                    lacunarity: 2.0,
                    gain: 0.5,
                },
                amplitude: 24.0,
                stream: 1,
            },
            Op::Coordinate(Axis::Y),
            Op::Subtract,
        ];
        if depth != 0.0 {
            ops.push(Op::Constant(depth));
            ops.push(Op::Subtract);
        }
        super::super::density::Density::compile(ops).expect("compile")
    }

    /// The same field, lowered by `depth`: what a generator wrote to put a
    /// second material `depth` blocks under the first.
    #[allow(dead_code)]
    fn strata_field_below(depth: f32) -> super::super::density::Density {
        use super::super::density::{Axis, Op};
        use super::super::noise::{Fractal, FractalParams};
        super::super::density::Density::compile(vec![
            Op::Noise {
                params: FractalParams {
                    fractal: Fractal::Fbm,
                    octaves: 3,
                    frequency: 0.02,
                    lacunarity: 2.0,
                    gain: 0.5,
                },
                amplitude: 10.0,
                stream: 5,
            },
            Op::Coordinate(Axis::Y),
            Op::Subtract,
            Op::Constant(depth),
            Op::Subtract,
        ])
        .expect("compile")
    }

    #[test]
    fn a_palette_writes_what_the_fills_it_replaces_would_have() {
        // **The claim is "the same terrain, one evaluation", and this is the
        // first half of it.** A generator layering grass on dirt on stone
        // wrote three fills of three shifted fields; the palette is meant to be
        // a pure replacement, so it is held to the same output block for block
        // and cell for cell — at block resolution and at both kinds of detail.
        //
        // Over a column of chunks so all three bound outcomes occur, and with a
        // count of chunks holding BOTH materials, because a palette that only
        // ever wrote one band would agree with the fills on every chunk that
        // only ever needed one.
        // **On a steep field.** The first version of this test used gentle
        // terrain and passed for all three resolutions with a palette that was
        // wrong: it asked every cell about every band, which agrees with the
        // fills wherever no band boundary runs through a block the surface
        // test called interior. With 24 blocks of relief at six octaves, the
        // field drops several blocks per block and that happens constantly —
        // 272 cells in twelve chunks. A test of "the same as the fills" has to
        // run where the fills do something awkward.
        const GRASS: MaterialId = MaterialId(9);
        let field = steep_field(0.0);
        let deeper = steep_field(3.0);
        let palette = Palette::new(vec![(0.0, GRASS), (3.0, STONE)]).expect("palette");

        let mut both = 0;
        for detail in [None, Some(Detail::Smooth), Some(Detail::Sampled)] {
            for cx in -6..6 {
                let pos = ChunkPos::new(cx, 0, 0);

                let mut by_fills = ChunkBuffer::new(pos, MaterialId::AIR);
                let mut by_palette = ChunkBuffer::new(pos, MaterialId::AIR);
                match detail {
                    None => {
                        by_fills.fill_density(&field, 3, GRASS).expect("fill");
                        by_fills.fill_density(&deeper, 3, STONE).expect("fill");
                        by_palette
                            .fill_palette(&field, 3, &palette)
                            .expect("palette");
                    }
                    Some(detail) => {
                        by_fills
                            .fill_density_detail(&field, 3, GRASS, detail)
                            .expect("fill");
                        by_fills
                            .fill_density_detail(&deeper, 3, STONE, detail)
                            .expect("fill");
                        by_palette
                            .fill_palette_detail(&field, 3, &palette, detail)
                            .expect("palette");
                    }
                }

                assert_same_cells(
                    &by_fills,
                    &by_palette,
                    &format!(
                        "the palette and the two fills it replaces disagree at {pos:?} with \
                         detail {detail:?}"
                    ),
                );

                let chunk = by_palette.to_chunk();
                let holds = |material: MaterialId| {
                    (0..16).any(|x| {
                        (0..16).any(|y| {
                            (0..16).any(|z| {
                                chunk
                                    .get_block(crate::BlockPos::new(
                                        pos.x * 16 + x,
                                        pos.y * 16 + y,
                                        pos.z * 16 + z,
                                    ))
                                    .is_some_and(|block| {
                                        (0..SUBNODES_PER_BLOCK)
                                            .any(|cell| block.subnode(cell) == material)
                                    })
                            })
                        })
                    })
                };
                if holds(GRASS) && holds(STONE) {
                    both += 1;
                }
            }
        }
        assert!(
            both >= 3,
            "only {both} chunks held both bands, so the two-band case was barely tested"
        );
    }

    #[test]
    fn a_palette_decided_by_its_bounds_writes_what_evaluating_it_would_have() {
        // The pruning, generalised: a chunk whose whole range of values lands
        // in one band is filled with that band without being evaluated, and a
        // chunk under the lowest band is skipped. Sound only if the answer is
        // the same as doing the work, so: against a reference that evaluates
        // every block and knows nothing of bounds — and an assertion that all
        // three outcomes occurred, or the run tested nothing.
        const GRASS: MaterialId = MaterialId(9);
        let field = strata_field();
        let palette = Palette::new(vec![(0.0, GRASS), (3.0, DIRT), (8.0, STONE)]).expect("palette");

        let (mut nothing, mut whole, mut mixed) = (0, 0, 0);
        for cy in -6..6 {
            for cx in -1..2 {
                let pos = ChunkPos::new(cx, cy, 1);
                let mut buffer = ChunkBuffer::new(pos, MaterialId::AIR);
                buffer.fill_palette(&field, 3, &palette).expect("palette");

                let side = CHUNK_BLOCKS as usize;
                let region = super::super::noise::Region3d {
                    origin_x: (pos.x * CHUNK_BLOCKS as i32) as f32,
                    origin_y: (pos.y * CHUNK_BLOCKS as i32) as f32,
                    origin_z: (pos.z * CHUNK_BLOCKS as i32) as f32,
                    step: 1.0,
                    width: side,
                    height: side,
                    depth: side,
                };
                let mut values = vec![0.0f32; region.len()];
                field.evaluate(3, &region, &mut values).expect("evaluate");
                let mut reference = ChunkBuffer::new(pos, MaterialId::AIR);
                let mut index = 0;
                let mut seen = std::collections::BTreeSet::new();
                for z in 0..CHUNK_BLOCKS {
                    for y in 0..CHUNK_BLOCKS {
                        for x in 0..CHUNK_BLOCKS {
                            let picked = palette.pick(values[index]);
                            seen.insert(picked.map(|m| m.0));
                            if let Some(material) = picked {
                                reference.set_block(LocalBlock::new(x, y, z), material);
                            }
                            index += 1;
                        }
                    }
                }
                assert_same_cells(
                    &reference,
                    &buffer,
                    &format!("the palette fill at {pos:?} differs from evaluating every block"),
                );
                match seen.len() {
                    1 if seen.contains(&None) => nothing += 1,
                    1 => whole += 1,
                    _ => mixed += 1,
                }
            }
        }
        assert!(
            nothing > 0 && whole > 0 && mixed > 0,
            "the column should hold chunks under every band ({nothing}), inside one band \
             ({whole}) and across a boundary ({mixed})"
        );
    }

    #[test]
    fn a_palette_refuses_what_it_could_not_use_and_sorts_what_it_can() {
        assert_eq!(Palette::new(vec![]), Err(PaletteError::Empty));
        let many: Vec<_> = (0..=MAX_PALETTE_BANDS).map(|i| (i as f32, STONE)).collect();
        assert_eq!(
            Palette::new(many),
            Err(PaletteError::TooMany {
                found: MAX_PALETTE_BANDS + 1
            })
        );
        assert_eq!(
            Palette::new(vec![(0.0, STONE), (f32::NAN, DIRT)]),
            Err(PaletteError::NotANumber)
        );
        assert_eq!(
            Palette::new(vec![(2.0, STONE), (2.0, DIRT)]),
            Err(PaletteError::Duplicate { threshold: 2.0 })
        );

        // Given out of order, the table still answers by value.
        let palette =
            Palette::new(vec![(4.0, STONE), (0.0, MaterialId(9)), (1.0, DIRT)]).expect("palette");
        assert_eq!(
            palette.pick(-1.0),
            None,
            "below the lowest band writes nothing"
        );
        assert_eq!(
            palette.pick(0.0),
            None,
            "the threshold itself is not above it"
        );
        assert_eq!(palette.pick(0.5), Some(MaterialId(9)));
        assert_eq!(palette.pick(1.0), Some(MaterialId(9)));
        assert_eq!(palette.pick(2.5), Some(DIRT));
        assert_eq!(palette.pick(100.0), Some(STONE));
    }

    #[test]
    fn skipping_a_chunk_by_its_bounds_writes_what_evaluating_it_would_have() {
        // **The differential test the pruning has to pass.** `fill_density`
        // now asks `Density::bounds` whether a chunk can hold any surface and
        // returns early when it cannot, which is only sound if the answer is
        // the same as doing the work. So: the same fills, checked against a
        // reference that evaluates every block and knows nothing about bounds.
        //
        // Chunks are chosen to cover all three outcomes — wholly empty, wholly
        // solid, and crossed — and the assertion at the end is that all three
        // actually occurred, because a run where every chunk was crossed would
        // pass this without testing anything.
        use super::super::density::{Axis, Op};
        use super::super::noise::{Fractal, FractalParams};

        let density = super::super::density::Density::compile(vec![
            Op::Noise {
                params: FractalParams {
                    fractal: Fractal::Fbm,
                    octaves: 3,
                    frequency: 0.02,
                    lacunarity: 2.0,
                    gain: 0.5,
                },
                amplitude: 10.0,
                stream: 5,
            },
            Op::Coordinate(Axis::Y),
            Op::Subtract,
        ])
        .expect("compile");

        let (mut empty, mut solid, mut crossed) = (0, 0, 0);
        for cy in -6..6 {
            for cx in -2..3 {
                let pos = ChunkPos::new(cx, cy, 1);
                let mut buffer = ChunkBuffer::new(pos, MaterialId::AIR);
                buffer.fill_density(&density, 3, STONE).expect("fill");

                // The reference: evaluate the whole chunk, block by block.
                let side = CHUNK_BLOCKS as usize;
                let region = super::super::noise::Region3d {
                    origin_x: (pos.x * CHUNK_BLOCKS as i32) as f32,
                    origin_y: (pos.y * CHUNK_BLOCKS as i32) as f32,
                    origin_z: (pos.z * CHUNK_BLOCKS as i32) as f32,
                    step: 1.0,
                    width: side,
                    height: side,
                    depth: side,
                };
                let mut field = vec![0.0f32; region.len()];
                density.evaluate(3, &region, &mut field).expect("evaluate");

                let mut positive = 0;
                let mut index = 0;
                for z in 0..CHUNK_BLOCKS {
                    for y in 0..CHUNK_BLOCKS {
                        for x in 0..CHUNK_BLOCKS {
                            let local = LocalBlock::new(x, y, z);
                            let wanted = if field[index] > 0.0 {
                                positive += 1;
                                STONE
                            } else {
                                MaterialId::AIR
                            };
                            assert_eq!(
                                buffer.get_block(local),
                                wanted,
                                "chunk {pos:?} block ({x}, {y}, {z}): the pruned fill and the \
                                 reference disagree"
                            );
                            index += 1;
                        }
                    }
                }
                if positive == 0 {
                    empty += 1;
                } else if positive == region.len() {
                    solid += 1;
                } else {
                    crossed += 1;
                }
            }
        }

        assert!(
            empty > 0 && solid > 0 && crossed > 0,
            "the sweep has to reach all three outcomes to mean anything: \
             {empty} empty, {solid} solid, {crossed} crossed"
        );
    }

    #[test]
    fn a_second_detail_fill_adds_to_the_first_instead_of_clearing_it() {
        // **Fills layer.** A generator paints stone, then dirt over it with a
        // band of the same field, then grass over that. Each of those is a
        // sub-node fill whose surface crosses blocks the earlier fill already
        // shaped, and the rule at cell resolution is the block rule: where the
        // field is not positive, leave the cell alone. Written as air instead,
        // the second fill's surface wipes the first's cells under it, and a
        // hillside comes out as ribbons of grass floating over stepped dirt.
        for detail in [Detail::Sampled, Detail::Smooth] {
            let mut buffer = ChunkBuffer::new(origin(), MaterialId(5));
            buffer
                .fill_density_detail(&slope(), 7, MaterialId(2), detail)
                .expect("fill");
            // A block the slope crosses: x = 8 puts the surface at y = 2.
            let local = LocalBlock::new(8, 2, 8);
            let mut below = 0;
            let mut above = 0;
            for cell in 0..SUBNODES_PER_BLOCK {
                let (sx, sy, sz) = crate::block::subnode_offset(cell);
                match buffer.get_subnode(local, sx, sy, sz) {
                    MaterialId(2) => below += 1,
                    MaterialId(5) => above += 1,
                    other => panic!("a cell the fill did not claim became {other:?} ({detail:?})"),
                }
            }
            assert!(
                below > 0 && above > 0,
                "the block was not crossed ({detail:?})"
            );
        }
    }

    #[test]
    fn detail_carves_the_surface_and_leaves_the_depths_alone() {
        // **The mechanism in one assertion.** A block deep inside the ground is
        // solid in all 27 of its cells and a block in open air is empty in all
        // of them; only the blocks the surface crosses are worth 27 samples.
        // What must NOT happen is the depths being carved — that would be the
        // shell test misfiring, and it would look like holes in the ground.
        let mut buffer = ChunkBuffer::new(origin(), MaterialId::AIR);
        buffer
            .fill_density_detail(&slope(), 7, MaterialId(2), Detail::Sampled)
            .expect("fill");

        // Deep below the slope: solid, every cell.
        for cell in 0..SUBNODES_PER_BLOCK {
            let (sx, sy, sz) = crate::block::subnode_offset(cell);
            assert_eq!(
                buffer.get_subnode(LocalBlock::new(8, 0, 8), sx, sy, sz),
                MaterialId(2),
                "a block under the surface was carved"
            );
        }
        // Well above it: empty, every cell.
        for cell in 0..SUBNODES_PER_BLOCK {
            let (sx, sy, sz) = crate::block::subnode_offset(cell);
            assert_eq!(
                buffer.get_subnode(LocalBlock::new(0, 12, 0), sx, sy, sz),
                MaterialId::AIR,
                "a block in open air was filled"
            );
        }
    }

    #[test]
    fn detail_gives_a_surface_block_some_cells_and_not_others() {
        // The point of sub-node worldgen: a block the surface crosses comes out
        // PART full. At block resolution every block is all or nothing, which
        // is the staircase a mod author is trying to get rid of.
        for detail in [Detail::Sampled, Detail::Smooth] {
            let mut buffer = ChunkBuffer::new(origin(), MaterialId::AIR);
            buffer
                .fill_density_detail(&slope(), 7, MaterialId(2), detail)
                .expect("fill");
            assert!(
                buffer.is_expanded(),
                "{detail:?} produced no sub-node detail at all"
            );

            let mut partial = 0;
            for x in 0..CHUNK_BLOCKS {
                for y in 0..CHUNK_BLOCKS {
                    let filled = (0..SUBNODES_PER_BLOCK)
                        .filter(|cell| {
                            let (sx, sy, sz) = crate::block::subnode_offset(*cell);
                            buffer.get_subnode(LocalBlock::new(x, y, 0), sx, sy, sz)
                                != MaterialId::AIR
                        })
                        .count();
                    if filled > 0 && filled < SUBNODES_PER_BLOCK {
                        partial += 1;
                    }
                }
            }
            assert!(
                partial > 0,
                "{detail:?} left every block all-or-nothing, which is block resolution"
            );
        }
    }

    #[test]
    fn detail_agrees_with_the_block_fill_about_what_is_solid() {
        // A sub-node fill must not MOVE the terrain. Every block the block-
        // resolution pass fills is at least partly filled here, and every block
        // it leaves empty is at most partly filled — so a mod switching detail
        // on gets the same landscape at a finer resolution rather than a
        // different one.
        let density = slope();
        let mut blocks = ChunkBuffer::new(origin(), MaterialId::AIR);
        blocks
            .fill_density(&density, 7, MaterialId(2))
            .expect("fill");
        let mut fine = ChunkBuffer::new(origin(), MaterialId::AIR);
        fine.fill_density_detail(&density, 7, MaterialId(2), Detail::Smooth)
            .expect("fill");

        for x in 0..CHUNK_BLOCKS {
            for y in 0..CHUNK_BLOCKS {
                let local = LocalBlock::new(x, y, 4);
                let coarse = blocks.get_subnode(local, 0, 0, 0) != MaterialId::AIR;
                let filled = (0..SUBNODES_PER_BLOCK)
                    .filter(|cell| {
                        let (sx, sy, sz) = crate::block::subnode_offset(*cell);
                        fine.get_subnode(local, sx, sy, sz) != MaterialId::AIR
                    })
                    .count();
                if coarse {
                    assert!(filled > 0, "({x},{y}) was solid and came out empty");
                } else {
                    assert!(
                        filled < SUBNODES_PER_BLOCK,
                        "({x},{y}) was empty and came out solid"
                    );
                }
            }
        }
    }

    #[test]
    fn a_new_buffer_is_block_backed() {
        let buffer = ChunkBuffer::new(origin(), STONE);
        assert!(!buffer.is_expanded());
        assert_eq!(buffer.memory_usage(), BLOCKS_PER_CHUNK * 2);
    }

    #[test]
    fn block_operations_never_expand() {
        // The property Sub-Node Contract §5 depends on. If this regresses,
        // every generator silently starts paying 27x.
        let mut buffer = ChunkBuffer::air(origin());
        buffer.fill_all(STONE);
        buffer.set_block(LocalBlock::new(1, 2, 3), DIRT);
        let heights = [8; 256];
        buffer
            .fill_below_heightmap(&heights, STONE)
            .expect("heightmap");
        assert_eq!(buffer.get_block(LocalBlock::new(1, 2, 3)), STONE);
        assert!(
            !buffer.is_expanded(),
            "block-level work must stay on the cheap path"
        );
    }

    #[test]
    fn reading_a_subnode_does_not_expand() {
        let buffer = ChunkBuffer::new(origin(), STONE);
        assert_eq!(buffer.get_subnode(LocalBlock::new(0, 0, 0), 1, 1, 1), STONE);
        assert!(!buffer.is_expanded(), "a read must not cost an expansion");
    }

    #[test]
    fn writing_a_subnode_expands_once() {
        let mut buffer = ChunkBuffer::new(origin(), STONE);
        buffer.set_subnode(LocalBlock::new(0, 0, 0), 1, 1, 1, DIRT);
        assert!(buffer.is_expanded());
        assert_eq!(buffer.memory_usage(), CELLS_PER_CHUNK * 2);

        // The expansion must have preserved everything.
        assert_eq!(buffer.get_subnode(LocalBlock::new(0, 0, 0), 1, 1, 1), DIRT);
        assert_eq!(buffer.get_subnode(LocalBlock::new(0, 0, 0), 0, 0, 0), STONE);
        assert_eq!(buffer.get_subnode(LocalBlock::new(9, 9, 9), 2, 2, 2), STONE);
    }

    #[test]
    fn expanding_costs_exactly_twenty_seven_times_the_memory() {
        let mut buffer = ChunkBuffer::new(origin(), STONE);
        let before = buffer.memory_usage();
        buffer.set_subnode(LocalBlock::new(0, 0, 0), 0, 0, 0, DIRT);
        assert_eq!(buffer.memory_usage(), before * 27);
    }

    #[test]
    fn fill_all_collapses_an_expanded_buffer() {
        let mut buffer = ChunkBuffer::air(origin());
        buffer.set_subnode(LocalBlock::new(0, 0, 0), 0, 0, 0, DIRT);
        assert!(buffer.is_expanded());
        buffer.fill_all(STONE);
        assert!(
            !buffer.is_expanded(),
            "after filling everything there is no sub-node detail left to keep"
        );
        assert_eq!(buffer.to_chunk().is_uniform(), Some(STONE));
    }

    #[test]
    fn setting_a_block_on_an_expanded_buffer_writes_all_cells() {
        let mut buffer = ChunkBuffer::air(origin());
        let local = LocalBlock::new(2, 2, 2);
        buffer.set_subnode(local, 0, 0, 0, DIRT);
        buffer.set_block(local, STONE);
        for z in 0..3 {
            for y in 0..3 {
                for x in 0..3 {
                    assert_eq!(buffer.get_subnode(local, x, y, z), STONE);
                }
            }
        }
    }

    #[test]
    fn heightmap_fills_the_right_layers() {
        let mut buffer = ChunkBuffer::air(origin());
        let mut heights = [0i32; 256];
        heights[0] = 5;
        heights[1] = 0;
        heights[2] = 100; // above the chunk: fills it entirely
        heights[3] = -10; // below the chunk: leaves it alone
        buffer.fill_below_heightmap(&heights, STONE).expect("fill");

        for y in 0..16 {
            assert_eq!(
                buffer.get_block(LocalBlock::new(0, y, 0)),
                if y < 5 { STONE } else { MaterialId::AIR },
                "column 0 at y={y}"
            );
        }
        assert_eq!(buffer.get_block(LocalBlock::new(1, 0, 0)), MaterialId::AIR);
        assert_eq!(buffer.get_block(LocalBlock::new(2, 15, 0)), STONE);
        assert_eq!(buffer.get_block(LocalBlock::new(3, 0, 0)), MaterialId::AIR);
    }

    #[test]
    fn heightmap_respects_the_chunks_vertical_offset() {
        // A chunk at y=1 covers world blocks 16..32, so a height of 20 fills
        // only its bottom four layers.
        let mut buffer = ChunkBuffer::air(ChunkPos::new(0, 1, 0));
        buffer
            .fill_below_heightmap(&[20; 256], STONE)
            .expect("fill");
        assert_eq!(buffer.get_block(LocalBlock::new(0, 3, 0)), STONE);
        assert_eq!(buffer.get_block(LocalBlock::new(0, 4, 0)), MaterialId::AIR);
    }

    #[test]
    fn a_wrong_sized_heightmap_is_an_error_not_a_panic() {
        let mut buffer = ChunkBuffer::air(origin());
        assert!(matches!(
            buffer.fill_below_heightmap(&[0; 10], STONE),
            Err(BufferError::WrongHeightmapSize { .. })
        ));
    }

    #[test]
    fn to_chunk_canonicalises() {
        let mut buffer = ChunkBuffer::air(origin());
        let local = LocalBlock::new(0, 0, 0);
        // Every cell of a block set to the same material must come out Uniform,
        // not as a Mixed slot holding 27 copies.
        for z in 0..3 {
            for y in 0..3 {
                for x in 0..3 {
                    buffer.set_subnode(local, x, y, z, STONE);
                }
            }
        }
        let chunk = buffer.to_chunk();
        assert_eq!(
            chunk.get_block_local(local),
            crate::BlockView::Uniform(STONE)
        );
        assert_eq!(chunk.mixed_len(), 0, "no mixed slot should be needed");
    }

    #[test]
    fn a_uniform_buffer_produces_a_uniform_chunk() {
        let buffer = ChunkBuffer::new(origin(), STONE);
        let chunk = buffer.to_chunk();
        assert_eq!(chunk.is_uniform(), Some(STONE));
        assert_eq!(chunk.palette_len(), 1);
        assert_eq!(chunk.bits_per_index(), 0);
    }

    #[test]
    fn a_partially_chiselled_block_survives_conversion() {
        let mut buffer = ChunkBuffer::new(origin(), STONE);
        let local = LocalBlock::new(5, 5, 5);
        buffer.set_subnode(local, 0, 0, 0, MaterialId::AIR);
        let chunk = buffer.to_chunk();
        let view = chunk.get_block_local(local);
        assert_eq!(view.occupied_units(), 26);
        assert_eq!(view.subnode(subnode_index(0, 0, 0)), MaterialId::AIR);
    }

    #[test]
    fn blit_copies_block_content_without_expanding() {
        let mut source = ChunkBuffer::air(origin());
        source.set_block(LocalBlock::new(0, 0, 0), STONE);
        source.set_block(LocalBlock::new(1, 0, 0), DIRT);

        let mut target = ChunkBuffer::air(origin());
        target.blit(
            &source,
            LocalBlock::new(0, 0, 0),
            LocalBlock::new(4, 4, 4),
            [2, 1, 1],
        );

        assert_eq!(target.get_block(LocalBlock::new(4, 4, 4)), STONE);
        assert_eq!(target.get_block(LocalBlock::new(5, 4, 4)), DIRT);
        assert!(
            !target.is_expanded(),
            "a block-resolution blit must stay on the cheap path"
        );
    }

    #[test]
    fn blit_from_an_expanded_source_carries_subnode_detail() {
        let mut source = ChunkBuffer::new(origin(), STONE);
        source.set_subnode(LocalBlock::new(0, 0, 0), 1, 1, 1, DIRT);

        let mut target = ChunkBuffer::air(origin());
        target.blit(
            &source,
            LocalBlock::new(0, 0, 0),
            LocalBlock::new(8, 8, 8),
            [1, 1, 1],
        );

        assert!(target.is_expanded());
        assert_eq!(target.get_subnode(LocalBlock::new(8, 8, 8), 1, 1, 1), DIRT);
        assert_eq!(target.get_subnode(LocalBlock::new(8, 8, 8), 0, 0, 0), STONE);
    }

    #[test]
    fn blit_clips_at_the_chunk_edge() {
        let mut source = ChunkBuffer::new(origin(), STONE);
        source.set_block(LocalBlock::new(0, 0, 0), DIRT);

        let mut target = ChunkBuffer::air(origin());
        // Deliberately overhanging: a structure at the chunk edge.
        target.blit(
            &source,
            LocalBlock::new(0, 0, 0),
            LocalBlock::new(15, 15, 15),
            [4, 4, 4],
        );
        assert_eq!(target.get_block(LocalBlock::new(15, 15, 15)), DIRT);
    }

    /// Ground under the cover tests: the slope, at sub-node resolution, so
    /// its surface sits at a different cell in different columns.
    fn sloped_ground() -> ChunkBuffer {
        let mut buffer = ChunkBuffer::new(origin(), MaterialId::AIR);
        buffer
            .fill_density_detail(&slope(), 7, STONE, Detail::Sampled)
            .expect("fill");
        buffer
    }

    /// The material of the cell directly under `(local, cx, cy, cz)`, or
    /// `None` at the chunk floor.
    fn under(
        buffer: &ChunkBuffer,
        local: LocalBlock,
        cx: u32,
        cy: u32,
        cz: u32,
    ) -> Option<MaterialId> {
        if cy > 0 {
            Some(buffer.get_subnode(local, cx, cy - 1, cz))
        } else if local.y > 0 {
            Some(buffer.get_subnode(LocalBlock::new(local.x, local.y - 1, local.z), cx, 2, cz))
        } else {
            None
        }
    }

    #[test]
    fn cover_stands_on_the_ground_inside_one_block_and_is_never_stacked() {
        // Every cover cell either stands on ground, or stands on a cover cell
        // that stands on ground — and a cover cell at the bottom of a block
        // ALWAYS stands on ground, which is what "never two stacked blocks"
        // means in cells. The slope puts the surface at every cell height, so
        // the one-cell case (surface in a block's top cell) and the two-cell
        // case both occur.
        const GRASS: MaterialId = MaterialId(9);
        let mut buffer = sloped_ground();
        buffer.fill_cover(GRASS, 2, None, 7).expect("cover");

        let (mut singles, mut doubles, mut total) = (0, 0, 0);
        for z in 0..CHUNK_BLOCKS {
            for y in 0..CHUNK_BLOCKS {
                for x in 0..CHUNK_BLOCKS {
                    let local = LocalBlock::new(x, y, z);
                    for cell in 0..SUBNODES_PER_BLOCK {
                        let (cx, cy, cz) = crate::block::subnode_offset(cell);
                        if buffer.get_subnode(local, cx, cy, cz) != GRASS {
                            continue;
                        }
                        total += 1;
                        let below = under(&buffer, local, cx, cy, cz)
                            .expect("cover never sits on the chunk floor");
                        if below == GRASS {
                            assert!(
                                cy > 0,
                                "a run crossed a block boundary at {local:?} ({cx}, {cy}, {cz})"
                            );
                            let ground = under(&buffer, local, cx, cy - 1, cz).expect("in chunk");
                            assert_eq!(
                                ground, STONE,
                                "a run is more than two cells tall at {local:?}"
                            );
                            doubles += 1;
                        } else {
                            assert_eq!(
                                below, STONE,
                                "cover floating at {local:?} ({cx}, {cy}, {cz})"
                            );
                            let above_is_grass =
                                cy < 2 && buffer.get_subnode(local, cx, cy + 1, cz) == GRASS;
                            if !above_is_grass {
                                singles += 1;
                            }
                        }
                    }
                }
            }
        }
        assert!(total > 0, "the slope should have grown cover");
        assert!(
            doubles > 0,
            "no run reached two cells, so `cells` did nothing"
        );
        assert!(
            singles > 0,
            "no run was clipped to one cell at a block's top, which the slope must produce"
        );
    }

    #[test]
    fn cover_on_a_chunk_of_one_material_writes_nothing_and_looks_at_nothing() {
        // The skip that makes this call affordable over a streamed column: a
        // chunk of plain sky or plain rock has no cell standing on another, so
        // there is nothing to find and the 27-cells-of-4,096-blocks scan is
        // pure cost. The assertion is that skipping it is not a behaviour
        // change — the same chunk, cell for cell.
        const GRASS: MaterialId = MaterialId(9);
        for held in [MaterialId::AIR, STONE] {
            let mut buffer = ChunkBuffer::new(origin(), held);
            let before = buffer.clone();
            buffer.fill_cover(GRASS, 2, None, 7).expect("cover");
            assert_eq!(
                buffer, before,
                "cover changed a chunk made entirely of one material"
            );
        }

        // And the skip is not reached once anything breaks the uniformity, or
        // it would be skipping real work. One block of stone in the sky is a
        // surface, and its top cell grows a run.
        let mut buffer = ChunkBuffer::new(origin(), MaterialId::AIR);
        buffer.set_block(LocalBlock::new(5, 5, 5), STONE);
        buffer.fill_cover(GRASS, 2, None, 7).expect("cover");
        assert_eq!(
            buffer.get_subnode(LocalBlock::new(5, 6, 5), 1, 0, 1),
            GRASS,
            "the block over a lone stone block should have grown cover"
        );
    }

    #[test]
    fn cover_takes_only_where_the_field_is_positive_and_never_overwrites() {
        // `take` positive for x < 8 only; and a slab of stone laid one cell
        // over part of the surface, which a run must not write into.
        use super::super::density::{Axis, Op};
        const GRASS: MaterialId = MaterialId(9);
        let take = super::super::density::Density::compile(vec![
            Op::Constant(8.0),
            Op::Coordinate(Axis::X),
            Op::Subtract,
        ])
        .expect("compile");
        let mut buffer = sloped_ground();
        buffer.fill_cover(GRASS, 2, Some(&take), 7).expect("cover");

        let mut west = 0;
        for z in 0..CHUNK_BLOCKS {
            for y in 0..CHUNK_BLOCKS {
                for x in 0..CHUNK_BLOCKS {
                    let local = LocalBlock::new(x, y, z);
                    for cell in 0..SUBNODES_PER_BLOCK {
                        let (cx, cy, cz) = crate::block::subnode_offset(cell);
                        if buffer.get_subnode(local, cx, cy, cz) == GRASS {
                            assert!(x < 8, "cover where `take` is not positive, at {local:?}");
                            west += 1;
                        }
                    }
                }
            }
        }
        assert!(west > 0, "no cover where `take` is positive");

        // A block of stone set whole is untouched, and the cell under it in
        // the column gets a run of one.
        let mut buffer = ChunkBuffer::new(origin(), MaterialId::AIR);
        for x in 0..CHUNK_BLOCKS {
            for z in 0..CHUNK_BLOCKS {
                buffer.set_block(LocalBlock::new(x, 3, z), STONE);
                buffer.set_block(LocalBlock::new(x, 4, z), STONE);
            }
        }
        // Carve the bottom cell layer of the upper block: a one-cell gap.
        buffer.set_subnode(LocalBlock::new(5, 4, 5), 1, 0, 1, MaterialId::AIR);
        buffer.fill_cover(GRASS, 2, None, 7).expect("cover");
        assert_eq!(
            buffer.get_subnode(LocalBlock::new(5, 4, 5), 1, 0, 1),
            GRASS,
            "the gap gets a run of one"
        );
        assert_eq!(
            buffer.get_subnode(LocalBlock::new(5, 4, 5), 1, 1, 1),
            STONE,
            "the stone over the gap is kept"
        );
    }

    #[test]
    fn layers_paint_each_code_its_own_bands_and_leave_the_rest() {
        // A slope for the depth, and a code field that is 1 west of x = 8
        // and 2 east of it. West: turf a block deep over dirt to three;
        // east: one band of snow two deep. Below the bands, and above the
        // surface, the buffer is untouched.
        use super::super::density::{Axis, Op};
        const TURF: MaterialId = MaterialId(11);
        const DIRT: MaterialId = MaterialId(12);
        const SNOW: MaterialId = MaterialId(13);
        let code = super::super::density::Density::compile(vec![
            Op::Coordinate(Axis::X),
            Op::Constant(8.0),
            Op::Subtract,
            Op::Constant(100.0),
            Op::Multiply,
            Op::Clamp {
                low: 0.0,
                high: 1.0,
            },
            Op::Constant(1.0),
            Op::Add,
        ])
        .expect("compile");
        let layers = [
            Layer {
                code: 1,
                from: 0.0,
                to: 1.0,
                material: TURF,
            },
            Layer {
                code: 1,
                from: 1.0,
                to: 3.0,
                material: DIRT,
            },
            Layer {
                code: 2,
                from: 0.0,
                to: 2.0,
                material: SNOW,
            },
        ];
        let mut buffer = ChunkBuffer::new(origin(), MaterialId::AIR);
        buffer
            .fill_density_detail(&slope(), 7, STONE, Detail::Smooth)
            .expect("ground");
        buffer
            .fill_layers(&slope(), &code, 7, &layers)
            .expect("layers");

        // The slope is `0.25 x - y`: depth at (x, y) is 0.25x - y. Column
        // x = 4 (code 1): the surface at y = 1; y = 0 is a block into the
        // ground — turf in its top cells, dirt below them; y = -... is not
        // in this chunk. Column x = 12 (code 2): surface at y = 3; y = 2 is
        // snow, y = 0 is three deep, past the band: stone.
        let west_top = buffer.get_subnode(LocalBlock::new(4, 0, 0), 1, 2, 1);
        assert_eq!(west_top, TURF, "the top cells of a code-1 column are turf");
        let west_low = buffer.get_subnode(LocalBlock::new(4, 0, 0), 1, 0, 1);
        assert!(
            west_low == DIRT || west_low == TURF,
            "under the turf is dirt (or turf at a fine edge), got {west_low:?}"
        );
        assert_eq!(
            buffer.get_subnode(LocalBlock::new(12, 2, 0), 1, 1, 1),
            SNOW,
            "a code-2 column's top is snow"
        );
        assert_eq!(
            buffer.get_block(LocalBlock::new(12, 0, 0)),
            STONE,
            "past the band the ground is left as it was"
        );
        assert_eq!(
            buffer.get_block(LocalBlock::new(12, 6, 0)),
            MaterialId::AIR,
            "the air above is left alone"
        );
    }

    #[test]
    fn set_block_cells_expands_and_round_trips() {
        let mut buffer = ChunkBuffer::air(origin());
        let mut cells = EMPTY_CELLS;
        cells[0] = STONE;
        cells[26] = DIRT;
        let local = LocalBlock::new(3, 3, 3);
        buffer.set_block_cells(local, &cells);

        assert!(buffer.is_expanded());
        for (index, expected) in cells.iter().enumerate() {
            let (x, y, z) = crate::block::subnode_offset(index);
            assert_eq!(buffer.get_subnode(local, x, y, z), *expected);
        }
    }
    /// A column of stone as a schematic: three whole blocks up from the root
    /// and a half block (its lower cells) on top.
    fn column() -> Schematic {
        let mut blocks = vec![];
        for dy in 0..3 {
            blocks.push(StampBlock {
                dx: 0,
                dy,
                dz: 0,
                material: STONE,
                mask: FULL_MASK,
            });
        }
        blocks.push(StampBlock {
            dx: 0,
            dy: 3,
            dz: 0,
            material: STONE,
            mask: 0b111 | 0b111 << 9 | 0b111 << 18,
        });
        Schematic::new(blocks)
    }

    /// Ground below y = 8 everywhere: 8 - y.
    fn flat_ground() -> super::super::density::Density {
        use super::super::density::{Axis, Density, Op};
        Density::compile(vec![
            Op::Constant(8.0),
            Op::Coordinate(Axis::Y),
            Op::Subtract,
        ])
        .expect("compiles")
    }

    #[test]
    fn scatter_stands_a_column_on_the_surface_it_finds() {
        let mut buffer = ChunkBuffer::new(origin(), MaterialId::AIR);
        buffer
            .fill_density(&flat_ground(), 7, STONE)
            .expect("fills");
        let column = column();
        let placed = buffer
            .scatter(
                7,
                &Scatter {
                    depth: &flat_ground(),
                    stand: None,
                    schematics: &[&column],
                    cell: 16,
                    chance: 1.0,
                    salt: 1,
                    sink: 0,
                },
            )
            .expect("scatters");
        assert_eq!(placed, 1, "one square, one candidate, kept");
        // Somewhere in the chunk a column of stone stands at y = 8, 9, 10 on
        // ground whose top block is y = 7, with the half block at 11.
        let mut found = 0;
        for x in 0..CHUNK_BLOCKS {
            for z in 0..CHUNK_BLOCKS {
                if buffer.get_block(LocalBlock::new(x, 8, z)) == STONE {
                    found += 1;
                    for y in 8..11 {
                        assert_eq!(buffer.get_block(LocalBlock::new(x, y, z)), STONE);
                    }
                    assert_eq!(
                        buffer.get_subnode(LocalBlock::new(x, 11, z), 1, 0, 1),
                        STONE
                    );
                    assert_eq!(
                        buffer.get_subnode(LocalBlock::new(x, 11, z), 1, 2, 1),
                        MaterialId::AIR
                    );
                    assert_eq!(
                        buffer.get_block(LocalBlock::new(x, 7, z)),
                        STONE,
                        "the ground"
                    );
                }
            }
        }
        assert_eq!(found, 1);
    }

    #[test]
    fn scatter_writes_the_same_structure_from_both_sides_of_an_edge() {
        // A wide slab: five blocks along x from the root, so a root near a
        // chunk edge reaches into the neighbour.
        let slab = Schematic::new(
            (-5..=5)
                .map(|dx| StampBlock {
                    dx,
                    dy: 1,
                    dz: 0,
                    material: DIRT,
                    mask: FULL_MASK,
                })
                .collect(),
        );
        let scatter = |pos: ChunkPos| {
            let mut buffer = ChunkBuffer::new(pos, MaterialId::AIR);
            buffer
                .fill_density(&flat_ground(), 3, STONE)
                .expect("fills");
            buffer
                .scatter(
                    3,
                    &Scatter {
                        depth: &flat_ground(),
                        stand: None,
                        schematics: &[&slab],
                        cell: 4,
                        chance: 0.5,
                        salt: 9,
                        sink: 0,
                    },
                )
                .expect("scatters");
            buffer
        };
        let west = scatter(ChunkPos::new(0, 0, 0));
        let east = scatter(ChunkPos::new(1, 0, 0));
        // Every slab crossing x = 16 is in both: the east chunk's first column
        // and the west chunk's last agree block for block along z, and there
        // is at least one dirt block on the seam to make the test mean it.
        let mut dirt = 0;
        for z in 0..CHUNK_BLOCKS {
            let w = west.get_block(LocalBlock::new(15, 9, z));
            let e = east.get_block(LocalBlock::new(0, 9, z));
            let west_has_slab_here = w == DIRT;
            // A slab of eleven blocks centred within four blocks of the seam
            // reaches across it, so dirt at x = 15 means dirt at x = 16 unless
            // the root sits at the slab's east end — which at cell 4 with a
            // reach of 5 it never does.
            if west_has_slab_here {
                assert_eq!(e, DIRT, "z = {z}: the east chunk lacks the west's slab");
                dirt += 1;
            }
        }
        assert!(
            dirt > 0,
            "no slab crossed the seam; the test proves nothing"
        );
    }
}
