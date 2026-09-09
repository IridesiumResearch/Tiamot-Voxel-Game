// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Fonts a mod ships, for its own interface.
//!
//! # Why the engine has fonts at all
//!
//! Charter rule 1: the engine holds no opinion about what a game looks like,
//! and a typeface is most of what an interface looks like. A mod that can
//! choose its blocks, its sounds and its dialogs and not its lettering is a mod
//! whose screens all look like the engine's.
//!
//! # These are hostile input, and the caps here are why
//!
//! Charter rule 14: a font file is a PARSER running on bytes a server pushed,
//! and font parsers are one of the most attacked surfaces there is. What
//! protects this client is not care in the mod that sent it:
//!
//! - the parser is pure Rust (`ab_glyph`, through `egui`), so there is no C
//!   codec in the path;
//! - [`MAX_FONT_BYTES`] is checked before anything parses, and is far below
//!   `content::MAX_FILE_BYTES` because a font is not a texture pack;
//! - [`MAX_FONTS`] bounds how many a server may push at all;
//! - parsing happens on a worker with panic isolation, so a font that kills
//!   the parser disables that font and nothing else;
//! - `fuzz/fuzz_targets/font_ingest.rs` fuzzes the same entry point.
//!
//! # And the cost nobody expects: the glyph atlas
//!
//! A font FILE is small. What it rasterises into is not: coverage is what
//! costs, and a face with a full CJK range is orders of magnitude more atlas
//! than a Latin one. [`MAX_FONTS`] is as much about that as about parsing.

/// A font a mod registered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Font {
    /// The qualified id, e.g. `"my_mod:cinzel"`.
    pub id: String,
    /// The mod that registered it, and whose directory `file` is relative to.
    pub mod_id: String,
    /// The file inside that mod's directory, e.g. `"fonts/cinzel.ttf"`.
    pub file: String,
}

/// The largest font file a client will parse, in bytes.
///
/// **Two megabytes, against `content::MAX_FILE_BYTES`' sixteen.** A Latin
/// TrueType face is 100–400 KiB and a large one with several weights is under
/// a megabyte; two is generous for anything an interface needs. The general
/// file cap is sized for texture packs, and letting a font have all of it would
/// be handing a parser fifteen megabytes of attacker-chosen bytes for no reason
/// anybody can name.
pub const MAX_FONT_BYTES: u64 = 2 * 1024 * 1024;

/// The most fonts one server may push.
///
/// **Bounded by the glyph atlas rather than by the files.** Each font that is
/// actually used rasterises into the client's atlas, and coverage is what
/// costs: a handful of faces is an interface, and a hundred is a client
/// spending its texture memory on lettering nobody asked to read.
pub const MAX_FONTS: usize = 8;

/// The font cap must stay well under the general file cap.
///
/// **A compile-time assertion rather than a test**, because it is a claim about
/// two constants and there is no run in which it could differ. The general cap
/// is sized for texture packs; letting a font parser have all of it would be
/// handing attacker-chosen bytes fifteen megabytes of room for no reason
/// anybody can name.
const _: () = assert!(MAX_FONT_BYTES < crate::content::MAX_FILE_BYTES);

/// And still enough for a real face with several weights.
const _: () = assert!(MAX_FONT_BYTES >= 1024 * 1024);
