// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Fuzzes the font parser — the lettering half of charter rule 14.
//!
//! A client parses font files pushed by servers it does not trust, and font
//! parsers are among the most attacked surfaces in any program that has one.
//! This target lands in the same task the loading path does rather than being
//! deferred to hardening, which is what the charter asks for in as many words.
//!
//! # What is being tested, and what is not
//!
//! The property is not "the font loads" — almost every input here is nonsense
//! and should be refused. It is that **a refusal is orderly**: the parser is
//! reached only through `ab_glyph`, which is pure Rust with no `unsafe` in the
//! path, so what this hunts is a panic. A client that dies because a server
//! sent a malformed typeface is still a client that dies, and a mod's dialog is
//! a thing a server chooses to show you.
//!
//! The size cap is applied where the bytes arrive (`net::offer_font`) and is
//! asserted here too: a target that fed the parser more than the client ever
//! would be fuzzing something nobody runs.
//!
//! # Why this parses rather than calling the client
//!
//! `client::fonts::Fonts::install` needs an `egui::Context`, which needs a
//! frame, which a fuzz target has no business building. `skrifa` is what egui
//! hands the bytes to — epaint 0.35 is built on the fontations stack — so this
//! hands them to the same place: one layer under the client and the same
//! parser. **If egui's font backend changes, this target must follow it**, or
//! it will be fuzzing a parser nobody runs.

#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    // The client refuses anything larger before it parses; so does this, or the
    // fuzzer spends its budget on inputs no client would ever see.
    if data.len() as u64 > tiamot_core::font::MAX_FONT_BYTES {
        return;
    }

    // The parse itself: reading the table directory and the tables a shaper
    // will then ask for.
    let Ok(font) = skrifa::FontRef::new(data) else {
        return;
    };

    // **A font that parsed is then ASKED things**, which is where a parser that
    // accepted a malformed table falls over: accepting is not the dangerous
    // moment, the first lookup is. These are the questions egui asks of every
    // face it loads — how many glyphs, what does this character map to, and
    // what is its outline.
    use skrifa::MetadataProvider as _;
    let charmap = font.charmap();
    let metrics = font.metrics(skrifa::instance::Size::unscaled(), skrifa::instance::LocationRef::default());
    let _ = metrics.units_per_em;
    let glyphs = font.outline_glyphs();
    for codepoint in ['A', ' ', '\u{0}', '\u{FFFD}', '\u{10FFFF}'] {
        let id = charmap.map(codepoint);
        let _ = id.and_then(|id| glyphs.get(id));
    }
});
