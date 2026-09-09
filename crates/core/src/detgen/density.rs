// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Density fields: 3D terrain and caves, described by a mod and evaluated here.
//!
//! Implements `docs/subnode-contract.md` §5: a density field writes at BLOCK
//! resolution. It never expands a buffer to sub-nodes, so a generator that uses
//! one pays nothing for sub-nodes existing — the opt-in the section describes
//! stays a separate, deliberate act.
//!
//! # Why this exists
//!
//! A mod may not compute simulation values itself. Charter rule 4 guarantees
//! that the same seed produces bit-identical worlds on every supported target,
//! and that guarantee rests on restricting which floating-point operations run
//! — which the engine cannot police inside a script VM. So worldgen has always
//! been "ask for a whole buffer, hand it to a whole-buffer operation", and the
//! only buffer on offer was a heightmap.
//!
//! A heightmap cannot describe an overhang, an arch or a cave. Those need a
//! value at a POINT IN SPACE rather than a height per column, and a mod cannot
//! produce one without doing exactly the per-sample arithmetic that is
//! forbidden.
//!
//! This closes that gap without opening the other: a mod describes an
//! expression as a Lua table, the engine compiles it once, and the engine
//! evaluates it. The mod says WHAT to compute and never computes it.
//!
//! # Whole arrays, not a tree walk per sample
//!
//! Evaluation is a stack machine over ARRAYS. Each operation consumes whole
//! buffers and produces one, so a noise node is a single [`super::fill_3d`]
//! call over the region and an add is one pass over two slices. The obvious
//! implementation — walk the tree once per sample — would call the noise
//! kernel from the innermost loop of an interpreter, which is the shape that
//! defeats every optimisation the fills were written to allow.
//!
//! The cost is memory: one buffer per stack slot, `region.len()` floats each.
//! At block resolution over a chunk that is 4,096 floats, 16 KiB a slot, and
//! [`MAX_DEPTH`] bounds how many can be live.
//!
//! # What it may contain, and why the list is short
//!
//! Every operation here is in the deterministic subset: `+ - * /`, `min`,
//! `max`, `abs`, comparison, and the gradient noise that
//! [`super::noise`] already provides. **No transcendentals**, so there is no
//! `pow`, `sin`, `exp` or `sqrt` node and there will not be one — see
//! `docs/float-determinism.md`. A mod wanting a curve builds it from
//! multiplication, or asks for a shape this module does not have and gets a
//! new node with a determinism argument attached.
//!
//! # What it deliberately is NOT
//!
//! Not a biome system, not an ore placer, not erosion. Those are content and
//! belong in a mod (charter rule 1) — this is the mechanism that makes them
//! expressible. A mod composes `noise`, `y` and arithmetic into whatever idea
//! of a landscape it has; the engine holds no opinion about which idea is
//! right.

use super::noise::{BufferSizeMismatch, Fractal, FractalParams, Region3d, fill_3d};

/// How many operations one density program may hold.
///
/// A bound rather than a limit anybody should meet: a hand-written field is a
/// dozen nodes and this is two orders above that. It exists because the program
/// arrives from a script and a runaway table should be refused with a message
/// rather than allocated.
pub const MAX_OPS: usize = 256;

/// How many array buffers may be live at once.
///
/// Each is `region.len()` floats, so this is the memory bound: eight buffers
/// over a chunk at block resolution is 128 KiB. An expression needing more is
/// almost certainly a mistake, and one that is not can be split.
pub const MAX_DEPTH: usize = 8;

/// The range a density program can take over a box, as `[low, high]`.
///
/// Always contains every value the program produces there, and usually more:
/// it is an interval extension, not a measurement.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Interval {
    /// No sample in the box is below this.
    pub low: f32,
    /// No sample in the box is above this.
    pub high: f32,
}

impl Interval {
    /// The interval holding exactly one value.
    #[must_use]
    pub const fn exactly(value: f32) -> Self {
        Self {
            low: value,
            high: value,
        }
    }

    /// Whether every value in the box is solid — the field is positive
    /// throughout, so a fill can write the material without asking.
    #[must_use]
    pub const fn is_all_solid(self) -> bool {
        self.low > 0.0
    }

    /// Whether no value in the box is solid, so a fill can do nothing at all.
    ///
    /// The comparison is `<=` because [`Density`] treats a sample as solid when
    /// it is strictly greater than zero, and the two rules have to agree
    /// exactly or a plane of cells appears or disappears at the boundary.
    #[must_use]
    pub const fn is_all_empty(self) -> bool {
        self.high <= 0.0
    }

    /// Whether the surface might cross this box, so it has to be evaluated.
    #[must_use]
    pub const fn is_undecided(self) -> bool {
        !self.is_all_solid() && !self.is_all_empty()
    }

    /// Widens by the arithmetic's own error, outwards.
    ///
    /// Interval arithmetic in `f32` rounds to nearest, which can move an end
    /// the WRONG way by half an ulp per operation, and a bound that is too
    /// narrow by one ulp is still a bound that can put a hole in a world.
    /// [`MAX_OPS`] caps the program at 256 operations, so `256 × 2⁻²³` relative
    /// covers the accumulation with room to spare; the absolute term covers an
    /// interval that straddles zero, where relative means nothing.
    fn widened(self) -> Self {
        let magnitude = self.low.abs().max(self.high.abs());
        let margin = magnitude * 3.1e-5 + 1e-6;
        Self {
            low: self.low - margin,
            high: self.high + margin,
        }
    }

    /// The interval containing both ends of a product's four corners.
    fn multiply(self, other: Self) -> Self {
        let corners = [
            self.low * other.low,
            self.low * other.high,
            self.high * other.low,
            self.high * other.high,
        ];
        let mut low = corners[0];
        let mut high = corners[0];
        for corner in &corners[1..] {
            low = low.min(*corner);
            high = high.max(*corner);
        }
        Self { low, high }
    }

    /// The same for a quotient, which has one case a product does not.
    fn divide(self, other: Self) -> Self {
        // **A divisor that straddles zero gives up.** The quotient runs to
        // both infinities and no finite interval contains it, so the honest
        // answer is the one that decides nothing and makes the caller
        // evaluate. `Op::Divide`'s own doc explains why an infinity is allowed
        // to exist at all.
        if other.low <= 0.0 && other.high >= 0.0 {
            return Self {
                low: f32::NEG_INFINITY,
                high: f32::INFINITY,
            };
        }
        self.multiply(Self {
            low: 1.0 / other.high,
            high: 1.0 / other.low,
        })
    }

    /// The interval of `|x|` over this one.
    fn absolute(self) -> Self {
        if self.low <= 0.0 && self.high >= 0.0 {
            // Straddles zero, so the smallest magnitude available is zero
            // itself — not `min(|low|, |high|)`, which is the mistake that
            // makes `abs` look like it preserves a gap it has closed.
            Self {
                low: 0.0,
                high: self.low.abs().max(self.high.abs()),
            }
        } else {
            let (a, b) = (self.low.abs(), self.high.abs());
            Self {
                low: a.min(b),
                high: a.max(b),
            }
        }
    }
}

/// One step of a compiled density program, in postfix order.
///
/// Values are pushed and consumed on a stack of whole arrays. A binary
/// operation pops two and pushes one.
#[derive(Debug, Clone, PartialEq)]
pub enum Op {
    /// Push the same number everywhere.
    Constant(f32),
    /// Push the sample's world x, y or z.
    Coordinate(Axis),
    /// Push fractal noise, sampled over the whole region in one call.
    Noise {
        /// The fractal's shape.
        params: FractalParams,
        /// Scales the result. Applied here rather than by a `mul` node so the
        /// common case is one op rather than two.
        amplitude: f32,
        /// Mixed into the world seed so two noise nodes in one program differ.
        /// **Not the chunk position**: a density field has to be continuous
        /// across a chunk boundary, so nothing about which chunk is being
        /// generated may reach the seed.
        stream: u64,
    },
    /// Pop two, push the sum.
    Add,
    /// Pop two, push `first - second`.
    Subtract,
    /// Pop two, push the product.
    Multiply,
    /// Pop two, push `first / second`.
    ///
    /// **A zero divisor gives an infinity, not a panic**, and an infinity that
    /// reaches a comparison is still a decision. What must never happen is a
    /// NaN in simulation state (charter rule 4), and `x / 0.0` is only NaN when
    /// `x` is zero as well — which [`Density::evaluate`] checks for in debug.
    Divide,
    /// Pop two, push the smaller.
    Minimum,
    /// Pop two, push the larger.
    Maximum,
    /// Pop one, push its absolute value.
    Absolute,
    /// Push a map's value at the sample's world x and z, ignoring its y.
    ///
    /// # Why a density needs this at all
    ///
    /// [`super::map`] exists because erosion and rivers cannot be a function of
    /// one position — where water goes depends on the land everywhere else — so
    /// the engine computes whole fields in passes. Without this node the only
    /// exit from a map is `Map::heightmap`, which feeds
    /// `ChunkBuffer::fill_below_heightmap` and nothing else: a world whose
    /// surface is a DENSITY (a dome, overhangs, caves) could not read an eroded
    /// field at all. The mechanism was half-built, and this is the other half.
    ///
    /// # It holds the map, rather than pointing at one
    ///
    /// A density program is a value, and a value that changed underneath a
    /// world would be the worst kind of bug: chunks generated before a mod
    /// blurred its map once more would disagree for ever with chunks generated
    /// after, along a seam nothing downstream can see. So compiling the node
    /// takes a COPY, shared with an `Arc` so that copy is made once however
    /// many programs read it.
    Map {
        /// The field, as it was when the program was compiled.
        map: std::sync::Arc<super::map::Map>,
        /// The smallest and largest value it holds, found once at compile time
        /// so [`Density::bounds`] can answer without walking it again.
        range: (f32, f32),
    },
    /// Pop one, push it bounded to a range.
    Clamp {
        /// Lower bound.
        low: f32,
        /// Upper bound.
        high: f32,
    },
}

/// Which coordinate an [`Op::Coordinate`] pushes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// World x.
    X,
    /// World y — the one a terrain field almost always wants, to make density
    /// fall off with height.
    Y,
    /// World z.
    Z,
}

/// What a density program can be wrong about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum DensityError {
    /// More operations than [`MAX_OPS`].
    #[error("a density program may hold at most {MAX_OPS} operations, not {found}")]
    TooManyOps {
        /// How many were given.
        found: usize,
    },
    /// The program needs more live buffers than [`MAX_DEPTH`].
    #[error("a density program may use at most {MAX_DEPTH} buffers at once, not {found}")]
    TooDeep {
        /// How many it would need.
        found: usize,
    },
    /// An operation wanted more values than the stack held.
    #[error("operation {index} wanted {wanted} values and the stack held {found}")]
    StackUnderflow {
        /// Which operation.
        index: usize,
        /// How many it needed.
        wanted: usize,
        /// How many there were.
        found: usize,
    },
    /// The program did not finish with exactly one value.
    #[error("a density program must leave exactly one value, not {found}")]
    NotOneResult {
        /// How many were left.
        found: usize,
    },
    /// A program with nothing in it.
    #[error("a density program must have at least one operation")]
    Empty,
    /// A number that cannot be part of a deterministic field.
    #[error("a density program may not contain {what}, which is not a finite number")]
    NotFinite {
        /// Which value.
        what: &'static str,
    },
    /// The output buffer was the wrong length.
    #[error(transparent)]
    Size(#[from] BufferSizeMismatch),
}

/// Reusable working buffers for [`Density::evaluate_with`].
///
/// A density program is a stack machine, and every slot it uses needs a buffer
/// the size of the region. Held by the caller so that a generator making a
/// thousand small calls — one per block the surface crosses, at sub-node
/// resolution — allocates once rather than a thousand times.
#[derive(Debug, Default)]
pub struct Scratch {
    slots: Vec<Vec<f32>>,
}

impl Scratch {
    /// Buffers for `depth` slots of `len` values, grown as needed.
    ///
    /// Existing buffers are resized rather than replaced, so the steady state
    /// after the first call is no allocation at all. The contents are not
    /// cleared: every op writes its whole slot before anything reads it, which
    /// is the same guarantee the freshly-allocated version relied on.
    fn slots(&mut self, depth: usize, len: usize) -> &mut [Vec<f32>] {
        while self.slots.len() < depth {
            self.slots.push(vec![0.0; len]);
        }
        for slot in &mut self.slots[..depth] {
            // **Exactly `len`, not at least it.** Every op writes and reads a
            // whole slot, and the region's own length is what says how long
            // that is — a longer buffer left over from a bigger region would
            // be a size mismatch at the first `fill_3d`. `resize` down keeps
            // the capacity, so growing back allocates nothing.
            if slot.len() != len {
                slot.resize(len, 0.0);
            }
        }
        &mut self.slots[..depth]
    }
}

/// A compiled density program.
///
/// Built once — a mod holds one and hands it to every chunk it generates —
/// because compiling walks a script's table and validating it costs more than
/// evaluating a small program does.
#[derive(Debug, Clone, PartialEq)]
pub struct Density {
    ops: Vec<Op>,
    depth: usize,
}

impl Density {
    /// Checks a program and records how many buffers it needs.
    ///
    /// # Errors
    ///
    /// Every variant of [`DensityError`] except [`DensityError::Size`], which
    /// belongs to [`Density::evaluate`].
    pub fn compile(ops: Vec<Op>) -> Result<Self, DensityError> {
        if ops.is_empty() {
            return Err(DensityError::Empty);
        }
        if ops.len() > MAX_OPS {
            return Err(DensityError::TooManyOps { found: ops.len() });
        }

        // **Validated by walking the stack at compile time**, so evaluation
        // needs no bounds checks of its own and a malformed program is refused
        // once rather than per chunk.
        let mut height = 0usize;
        let mut peak = 0usize;
        for (index, op) in ops.iter().enumerate() {
            for (value, what) in op.constants() {
                if !value.is_finite() {
                    return Err(DensityError::NotFinite { what });
                }
            }
            let wanted = op.arity();
            if height < wanted {
                return Err(DensityError::StackUnderflow {
                    index,
                    wanted,
                    found: height,
                });
            }
            height = height - wanted + 1;
            peak = peak.max(height);
        }
        if height != 1 {
            return Err(DensityError::NotOneResult { found: height });
        }
        if peak > MAX_DEPTH {
            return Err(DensityError::TooDeep { found: peak });
        }
        Ok(Self { ops, depth: peak })
    }

    /// How many buffers evaluating this needs at once.
    #[must_use]
    pub const fn depth(&self) -> usize {
        self.depth
    }

    /// How many operations it holds.
    #[must_use]
    pub fn len(&self) -> usize {
        self.ops.len()
    }

    /// Whether it holds no operations. Never true — `compile` refuses those.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }

    /// Evaluates the field over `region`, writing `region.len()` values.
    ///
    /// # Errors
    ///
    /// [`DensityError::Size`] if `out` is not exactly `region.len()` long.
    ///
    /// # Panics
    ///
    /// In debug builds, if the program produces a NaN. Charter rule 4 forbids
    /// one reaching simulation state, and a field is simulation state the
    /// moment a chunk is filled from it.
    pub fn evaluate(
        &self,
        seed: u64,
        region: &Region3d,
        out: &mut [f32],
    ) -> Result<(), DensityError> {
        self.evaluate_with(seed, region, out, &mut Scratch::default())
    }

    /// What the program can possibly produce over a box, without evaluating it.
    ///
    /// # Why a bound and not nine samples
    ///
    /// The obvious way to ask "could the surface be in this chunk?" is to
    /// evaluate the corners and the centre and look at the signs. **That is not
    /// a bound and it will put holes in a world**: gradient noise between two
    /// samples is not bounded by those samples, so a chunk whose nine samples
    /// all read solid can still contain surface. The holes appear only where
    /// the field happens to wiggle at the wrong scale, which makes them look
    /// like corruption rather than like a wrong constant.
    ///
    /// So this is an interval extension instead: every operation is replaced by
    /// its interval version, and the result is guaranteed to contain every
    /// value the program takes anywhere in the box. It costs one pass over the
    /// operations — for a 26-node program, twenty-six interval operations
    /// against 26 × 5,832 sample evaluations, so it is free beside the thing it
    /// avoids.
    ///
    /// The answer is conservative in ONE direction, always: an interval that is
    /// too wide costs terrain that is generated when it need not have been, and
    /// one that is too narrow is a hole. Where a bound is not known, the widest
    /// possible one is returned rather than a guess — see [`Interval::divide`].
    ///
    /// # What the caller may do with it
    ///
    /// [`Interval::is_all_solid`] and [`Interval::is_all_empty`] are the two
    /// decisions worth taking. Anything else is [`Interval::is_undecided`], and
    /// the only answer to that is to evaluate.
    ///
    /// The box is the one the region SPANS, corner to corner, so a caller that
    /// has a `Region3d` for a chunk already has the right argument. Values
    /// between the sample points are covered too, which matters: a sub-node
    /// pass samples inside the same box at a finer step and must not be able to
    /// find surface a coarser bound said was not there.
    #[must_use]
    pub fn bounds(&self, over: &Region3d) -> Interval {
        let span = |origin: f32, count: usize| -> Interval {
            let far = origin + (count.saturating_sub(1)) as f32 * over.step;
            Interval {
                low: origin.min(far),
                high: origin.max(far),
            }
        };
        let axes = [
            span(over.origin_x, over.width),
            span(over.origin_y, over.height),
            span(over.origin_z, over.depth),
        ];

        let mut stack: Vec<Interval> = Vec::with_capacity(self.depth);
        for op in &self.ops {
            match op {
                Op::Constant(value) => stack.push(Interval::exactly(*value)),
                Op::Coordinate(which) => stack.push(match which {
                    Axis::X => axes[0],
                    Axis::Y => axes[1],
                    Axis::Z => axes[2],
                }),
                Op::Noise {
                    params, amplitude, ..
                } => {
                    // The seed does not appear: a bound on the fractal holds
                    // for every seed, which is the property that makes this
                    // safe to compute once and reuse.
                    let (low, high) = params.range();
                    let scaled = Interval { low, high }.multiply(Interval::exactly(*amplitude));
                    stack.push(scaled);
                }
                Op::Map { range, .. } => stack.push(Interval {
                    low: range.0,
                    high: range.1,
                }),
                Op::Absolute => {
                    let Some(value) = stack.pop() else {
                        return Interval {
                            low: f32::NEG_INFINITY,
                            high: f32::INFINITY,
                        };
                    };
                    stack.push(value.absolute());
                }
                Op::Clamp { low, high } => {
                    let Some(value) = stack.pop() else {
                        return Interval {
                            low: f32::NEG_INFINITY,
                            high: f32::INFINITY,
                        };
                    };
                    stack.push(Interval {
                        low: value.low.clamp(*low, *high),
                        high: value.high.clamp(*low, *high),
                    });
                }
                binary => {
                    // `first` is the deeper of the two, matching `apply_binary`
                    // folding `second` into `first`. Getting this backwards is
                    // invisible for `add` and wrong for `sub` and `div`.
                    let (Some(second), Some(first)) = (stack.pop(), stack.pop()) else {
                        return Interval {
                            low: f32::NEG_INFINITY,
                            high: f32::INFINITY,
                        };
                    };
                    stack.push(match binary {
                        Op::Add => Interval {
                            low: first.low + second.low,
                            high: first.high + second.high,
                        },
                        Op::Subtract => Interval {
                            low: first.low - second.high,
                            high: first.high - second.low,
                        },
                        Op::Multiply => first.multiply(second),
                        Op::Divide => first.divide(second),
                        Op::Minimum => Interval {
                            low: first.low.min(second.low),
                            high: first.high.min(second.high),
                        },
                        Op::Maximum => Interval {
                            low: first.low.max(second.low),
                            high: first.high.max(second.high),
                        },
                        _ => unreachable!("the outer match limits this to the binary ops"),
                    });
                }
            }
        }

        stack.pop().map_or(
            Interval {
                low: f32::NEG_INFINITY,
                high: f32::INFINITY,
            },
            Interval::widened,
        )
    }

    /// The same, over a scratch buffer the caller keeps.
    ///
    /// # Why this exists
    ///
    /// [`Self::evaluate`] allocates one buffer per stack slot, which is the
    /// right trade for the one big call a chunk used to need. Sub-node terrain
    /// makes a THOUSAND small calls — one 3×3×3 region per block the surface
    /// crosses — and at that size the allocation is the work. The scratch grows
    /// to the largest region it has been asked for and is reused after that.
    ///
    /// # Errors
    ///
    /// [`DensityError::Size`] if `out` is not exactly `region.len()` long.
    ///
    /// # Panics
    ///
    /// In debug builds, if the program produces a NaN — see [`Self::evaluate`].
    pub fn evaluate_with(
        &self,
        seed: u64,
        region: &Region3d,
        out: &mut [f32],
        scratch: &mut Scratch,
    ) -> Result<(), DensityError> {
        let len = region.len();
        if out.len() != len {
            return Err(DensityError::Size(BufferSizeMismatch {
                expected: len,
                found: out.len(),
            }));
        }

        let stack = scratch.slots(self.depth, len);
        let mut height = 0usize;

        for op in &self.ops {
            match op {
                Op::Constant(value) => {
                    stack[height].fill(*value);
                    height += 1;
                }
                Op::Coordinate(axis) => {
                    fill_coordinate(*axis, region, &mut stack[height]);
                    height += 1;
                }
                Op::Noise {
                    params,
                    amplitude,
                    stream,
                } => {
                    let slot = &mut stack[height];
                    fill_3d(seed ^ stream, region, params, slot)?;
                    if (*amplitude - 1.0).abs() > f32::EPSILON {
                        for value in slot.iter_mut() {
                            *value *= amplitude;
                        }
                    }
                    height += 1;
                }
                Op::Map { map, .. } => {
                    let slot = &mut stack[height];
                    fill_from_map(map, region, slot);
                    height += 1;
                }
                Op::Absolute | Op::Clamp { .. } => {
                    let slot = &mut stack[height - 1];
                    match op {
                        Op::Absolute => {
                            for value in slot.iter_mut() {
                                *value = value.abs();
                            }
                        }
                        Op::Clamp { low, high } => {
                            for value in slot.iter_mut() {
                                *value = value.clamp(*low, *high);
                            }
                        }
                        _ => unreachable!("outer match limits this to the unary ops"),
                    }
                }
                binary => {
                    // **Split so the borrow checker can see two slots.** The
                    // second operand is folded into the first, which is then
                    // the result — one pass and no third buffer.
                    let (left, right) = stack.split_at_mut(height - 1);
                    let first = &mut left[height - 2];
                    let second = &right[0];
                    apply_binary(binary, first, second);
                    height -= 1;
                }
            }
        }

        out.copy_from_slice(&stack[0]);
        debug_assert!(
            out.iter().all(|value| !value.is_nan()),
            "a density program produced a NaN, which charter rule 4 forbids in \
             simulation state"
        );
        Ok(())
    }
}

impl Op {
    /// How many values this consumes.
    const fn arity(&self) -> usize {
        match self {
            Self::Constant(_) | Self::Coordinate(_) | Self::Noise { .. } | Self::Map { .. } => 0,
            Self::Absolute | Self::Clamp { .. } => 1,
            Self::Add
            | Self::Subtract
            | Self::Multiply
            | Self::Divide
            | Self::Minimum
            | Self::Maximum => 2,
        }
    }

    /// Every literal in this operation, for the finiteness check.
    fn constants(&self) -> Vec<(f32, &'static str)> {
        match self {
            Self::Constant(value) => vec![(*value, "a constant")],
            Self::Noise {
                params, amplitude, ..
            } => vec![
                (*amplitude, "a noise amplitude"),
                (params.frequency, "a noise frequency"),
                (params.lacunarity, "a noise lacunarity"),
                (params.gain, "a noise gain"),
            ],
            Self::Clamp { low, high } => vec![(*low, "a clamp bound"), (*high, "a clamp bound")],
            _ => Vec::new(),
        }
    }
}

/// The default fractal shape, for a mod that names only a frequency.
#[must_use]
pub fn default_params() -> FractalParams {
    FractalParams {
        fractal: Fractal::Fbm,
        octaves: 4,
        frequency: 0.02,
        lacunarity: 2.0,
        gain: 0.5,
    }
}

/// Writes a coordinate value into every sample of the region.
/// Fills a slot with a map's value under each sample.
///
/// A map is a surface, so y does nothing here: every sample in a column gets
/// the same value, and a program that wants a height comparison subtracts `y`
/// itself. Sampled per position rather than per column because the region's
/// layout is x-fastest and re-deriving the column would cost more than the
/// bilinear read does.
fn fill_from_map(map: &super::map::Map, region: &Region3d, out: &mut [f32]) {
    let mut index = 0;
    for layer in 0..region.depth {
        let z = region.origin_z + layer as f32 * region.step;
        for _ in 0..region.height {
            for column in 0..region.width {
                let x = region.origin_x + column as f32 * region.step;
                out[index] = map.sample(super::floor_to_i32(x), super::floor_to_i32(z));
                index += 1;
            }
        }
    }
}

fn fill_coordinate(axis: Axis, region: &Region3d, out: &mut [f32]) {
    let mut index = 0;
    for layer in 0..region.depth {
        let z = region.origin_z + layer as f32 * region.step;
        for row in 0..region.height {
            let y = region.origin_y + row as f32 * region.step;
            for column in 0..region.width {
                let x = region.origin_x + column as f32 * region.step;
                out[index] = match axis {
                    Axis::X => x,
                    Axis::Y => y,
                    Axis::Z => z,
                };
                index += 1;
            }
        }
    }
}

/// Folds `second` into `first` elementwise.
///
/// **A fixed order, always.** Charter rule 4 bans float accumulation over a
/// non-deterministic iteration order, and while a slice walk is already
/// ordered, saying so here is what stops somebody reaching for `rayon` on the
/// grounds that the loop is embarrassingly parallel. It is — and a parallel
/// reduction would still be a different answer.
fn apply_binary(op: &Op, first: &mut [f32], second: &[f32]) {
    match op {
        Op::Add => {
            for (a, b) in first.iter_mut().zip(second) {
                *a += *b;
            }
        }
        Op::Subtract => {
            for (a, b) in first.iter_mut().zip(second) {
                *a -= *b;
            }
        }
        Op::Multiply => {
            for (a, b) in first.iter_mut().zip(second) {
                *a *= *b;
            }
        }
        Op::Divide => {
            for (a, b) in first.iter_mut().zip(second) {
                *a /= *b;
            }
        }
        Op::Minimum => {
            for (a, b) in first.iter_mut().zip(second) {
                *a = a.min(*b);
            }
        }
        Op::Maximum => {
            for (a, b) in first.iter_mut().zip(second) {
                *a = a.max(*b);
            }
        }
        _ => unreachable!("only the binary operations reach here"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_region() -> Region3d {
        Region3d {
            origin_x: 0.0,
            origin_y: 0.0,
            origin_z: 0.0,
            step: 1.0,
            width: 16,
            height: 16,
            depth: 16,
        }
    }

    /// A field with hills in it, the shape a real generator uses: noise that
    /// falls off with height, so the surface is where the two balance.
    fn terrain(frequency: f32, amplitude: f32) -> Vec<Op> {
        vec![
            Op::Noise {
                params: FractalParams {
                    fractal: Fractal::Fbm,
                    octaves: 4,
                    frequency,
                    lacunarity: 2.0,
                    gain: 0.5,
                },
                amplitude,
                stream: 7,
            },
            Op::Coordinate(Axis::Y),
            Op::Subtract,
        ]
    }

    #[test]
    fn a_bound_contains_every_sample_inside_it() {
        // The whole contract, over boxes all over the world rather than at the
        // origin: a bound that only holds near zero is a bound that fails
        // wherever anybody actually builds.
        let density = Density::compile(terrain(0.03, 12.0)).expect("compile");
        for corner in [-8192.0, -256.0, 0.0, 1024.0, 60_000.0] {
            for height in [-320.0, -16.0, 0.0, 96.0] {
                let region = Region3d {
                    origin_x: corner,
                    origin_y: height,
                    origin_z: corner * 0.5,
                    step: 1.0,
                    width: 16,
                    height: 16,
                    depth: 16,
                };
                let bounds = density.bounds(&region);
                let mut field = vec![0.0; region.len()];
                density.evaluate(9, &region, &mut field).expect("evaluate");
                for value in &field {
                    assert!(
                        *value >= bounds.low && *value <= bounds.high,
                        "sample {value} at ({corner}, {height}) fell outside \
                         ({}, {})",
                        bounds.low,
                        bounds.high
                    );
                }
            }
        }
    }

    #[test]
    fn a_bound_still_holds_at_sub_node_resolution() {
        // The case the sub-node fill depends on. A coarse bound is taken over
        // the box, and then the fine pass samples INSIDE it at a third of the
        // step: if the bound only covered the coarse sample points, the fine
        // pass could find surface where the chunk was skipped.
        let density = Density::compile(terrain(0.08, 6.0)).expect("compile");
        let coarse = Region3d {
            origin_x: 32.0,
            origin_y: -4.0,
            origin_z: -64.0,
            step: 1.0,
            width: 16,
            height: 16,
            depth: 16,
        };
        let bounds = density.bounds(&coarse);
        let fine = Region3d {
            step: 1.0 / 3.0,
            width: 46,
            height: 46,
            depth: 46,
            ..coarse
        };
        let mut field = vec![0.0; fine.len()];
        density.evaluate(9, &fine, &mut field).expect("evaluate");
        for value in &field {
            assert!(
                *value >= bounds.low && *value <= bounds.high,
                "a sub-node sample {value} escaped the block-resolution bound ({}, {})",
                bounds.low,
                bounds.high
            );
        }
    }

    /// Noise about a threshold, with no height term: the shape of a cave field
    /// or an ore pocket, and the one where a coarse sample grid is blindest.
    fn blobs(frequency: f32, threshold: f32) -> Vec<Op> {
        vec![
            Op::Noise {
                params: FractalParams {
                    fractal: Fractal::Fbm,
                    octaves: 2,
                    frequency,
                    lacunarity: 2.0,
                    gain: 0.5,
                },
                amplitude: 1.0,
                stream: 3,
            },
            Op::Constant(threshold),
            Op::Subtract,
        ]
    }

    #[test]
    fn sampling_the_corners_and_the_centre_is_not_a_bound() {
        // **The counter-example this whole mechanism exists for.** The cheap
        // way to ask "could the surface be in this chunk" is to evaluate the
        // eight corners and the centre and look at the signs, and it is wrong:
        // noise between two samples is not bounded by those samples. A chunk
        // whose nine samples agree can still contain surface, and a generator
        // that skipped it would leave a hole.
        //
        // **What makes it fail is feature size against sample spacing.** Nine
        // samples over sixteen blocks see nothing smaller than sixteen blocks,
        // and a mod's caves and ore are deliberately smaller than that. A
        // gentle heightmap would survive the same test, which is exactly why
        // "it worked when I tried it" is not evidence here.
        //
        // Searched rather than hand-picked, so it keeps meaning something if
        // the noise is ever retuned.
        let mut found = None;
        'search: for (frequency, threshold) in [(0.45, 0.5), (0.8, 0.35), (1.3, 0.2)] {
            let density = Density::compile(blobs(frequency, threshold)).expect("compile");
            for x in 0..24 {
                for z in 0..24 {
                    let region = Region3d {
                        origin_x: (x * 16) as f32,
                        origin_y: 0.0,
                        origin_z: (z * 16) as f32,
                        step: 1.0,
                        width: 16,
                        height: 16,
                        depth: 16,
                    };
                    let far = 15.0;
                    let mut corners = vec![(far / 2.0, far / 2.0, far / 2.0)];
                    for dx in [0.0, far] {
                        for dy in [0.0, far] {
                            for dz in [0.0, far] {
                                corners.push((dx, dy, dz));
                            }
                        }
                    }
                    let missed = corners.iter().all(|(dx, dy, dz)| {
                        let point = Region3d {
                            origin_x: region.origin_x + dx,
                            origin_y: region.origin_y + dy,
                            origin_z: region.origin_z + dz,
                            step: 1.0,
                            width: 1,
                            height: 1,
                            depth: 1,
                        };
                        let mut one = [0.0];
                        density.evaluate(5, &point, &mut one).expect("evaluate");
                        one[0] <= 0.0
                    });
                    if !missed {
                        continue;
                    }
                    let mut field = vec![0.0; region.len()];
                    density.evaluate(5, &region, &mut field).expect("evaluate");
                    if let Some(solid) = field.iter().copied().find(|value| *value > 0.0) {
                        found = Some((density, region, solid));
                        break 'search;
                    }
                }
            }
        }

        let (density, region, solid) = found.expect(
            "no counter-example found — nine samples are not sound, so this search failing \
             means the search is wrong rather than that the shortcut is safe",
        );
        // The interval extension is not fooled by the chunk that fooled them.
        assert!(
            !density.bounds(&region).is_all_empty(),
            "a chunk holding a sample of {solid} was called empty by its own bound"
        );
    }

    fn evaluate(ops: Vec<Op>) -> Vec<f32> {
        let density = Density::compile(ops).expect("compile");
        let region = chunk_region();
        let mut out = vec![0.0; region.len()];
        density.evaluate(7, &region, &mut out).expect("evaluate");
        out
    }

    #[test]
    fn a_constant_is_the_same_everywhere() {
        let out = evaluate(vec![Op::Constant(2.5)]);
        assert!(out.iter().all(|value| (*value - 2.5).abs() < f32::EPSILON));
    }

    #[test]
    fn arithmetic_folds_in_the_order_it_was_written() {
        // 10 - 4 = 6, not 4 - 10. Postfix order is easy to get backwards and
        // the symptom is terrain inverted about a plane, which reads as a bug
        // in the noise rather than in the subtraction.
        let out = evaluate(vec![Op::Constant(10.0), Op::Constant(4.0), Op::Subtract]);
        assert!((out[0] - 6.0).abs() < f32::EPSILON, "got {}", out[0]);

        let out = evaluate(vec![Op::Constant(10.0), Op::Constant(4.0), Op::Divide]);
        assert!((out[0] - 2.5).abs() < f32::EPSILON, "got {}", out[0]);
    }

    #[test]
    fn the_y_coordinate_is_the_world_one_not_the_local_one() {
        // **The field has to be continuous across a chunk boundary.** A `y`
        // node that pushed the local index would restart at zero every 16
        // blocks, and terrain built on it would be sixteen-block terraces all
        // the way up — the shape a height bias exists to avoid.
        let density = Density::compile(vec![Op::Coordinate(Axis::Y)]).expect("compile");
        let region = Region3d {
            origin_y: 32.0,
            ..chunk_region()
        };
        let mut out = vec![0.0; region.len()];
        density.evaluate(7, &region, &mut out).expect("evaluate");
        assert!((out[0] - 32.0).abs() < f32::EPSILON, "got {}", out[0]);
    }

    #[test]
    fn a_field_is_identical_for_the_same_seed_and_different_for_another() {
        // Charter rule 4 in the small: this is the property the whole module
        // is shaped around, and it is worth asserting where a change to the
        // evaluator would break it rather than only in the CI hash gate.
        let ops = vec![
            Op::Noise {
                params: default_params(),
                amplitude: 1.0,
                stream: 11,
            },
            Op::Coordinate(Axis::Y),
            Op::Constant(0.05),
            Op::Multiply,
            Op::Subtract,
        ];
        let first = evaluate(ops.clone());
        let second = evaluate(ops.clone());
        assert_eq!(
            first, second,
            "the same program and seed must agree exactly"
        );

        let density = Density::compile(ops).expect("compile");
        let region = chunk_region();
        let mut other = vec![0.0; region.len()];
        density.evaluate(8, &region, &mut other).expect("evaluate");
        assert_ne!(
            first, other,
            "a different world seed must give a different field"
        );
    }

    #[test]
    fn two_noise_nodes_with_different_streams_do_not_agree() {
        // Otherwise a mod asking for two independent fields — terrain and
        // caves, say — gets the same one twice and the caves follow the hills
        // exactly. The failure looks like a worldgen idea that did not work.
        let one = evaluate(vec![Op::Noise {
            params: default_params(),
            amplitude: 1.0,
            stream: 1,
        }]);
        let two = evaluate(vec![Op::Noise {
            params: default_params(),
            amplitude: 1.0,
            stream: 2,
        }]);
        assert_ne!(one, two);
    }

    #[test]
    fn a_malformed_program_is_refused_rather_than_evaluated() {
        assert_eq!(Density::compile(Vec::new()), Err(DensityError::Empty));
        assert_eq!(
            Density::compile(vec![Op::Add]),
            Err(DensityError::StackUnderflow {
                index: 0,
                wanted: 2,
                found: 0
            })
        );
        assert_eq!(
            Density::compile(vec![Op::Constant(1.0), Op::Constant(2.0)]),
            Err(DensityError::NotOneResult { found: 2 })
        );
        assert_eq!(
            Density::compile(vec![Op::Constant(f32::NAN)]),
            Err(DensityError::NotFinite { what: "a constant" })
        );
        assert_eq!(
            Density::compile(vec![Op::Constant(f32::INFINITY)]),
            Err(DensityError::NotFinite { what: "a constant" })
        );
    }

    #[test]
    fn a_program_too_deep_or_too_long_is_refused() {
        // It arrives from a script, so the bound has to be a message rather
        // than an allocation. Deep first: MAX_DEPTH + 1 constants pushed
        // without ever being consumed.
        let deep: Vec<Op> = (0..=MAX_DEPTH).map(|_| Op::Constant(1.0)).collect();
        let mut program = deep.clone();
        for _ in 0..MAX_DEPTH {
            program.push(Op::Add);
        }
        assert_eq!(
            Density::compile(program),
            Err(DensityError::TooDeep {
                found: MAX_DEPTH + 1
            })
        );

        let mut long = vec![Op::Constant(1.0)];
        for _ in 0..MAX_OPS {
            long.push(Op::Absolute);
        }
        assert_eq!(
            Density::compile(long),
            Err(DensityError::TooManyOps { found: MAX_OPS + 1 })
        );
    }

    #[test]
    fn a_wrong_sized_output_is_refused() {
        let density = Density::compile(vec![Op::Constant(1.0)]).expect("compile");
        let region = chunk_region();
        let mut out = vec![0.0; region.len() - 1];
        assert!(matches!(
            density.evaluate(1, &region, &mut out),
            Err(DensityError::Size(_))
        ));
    }
}
