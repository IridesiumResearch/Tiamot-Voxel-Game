// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Fonts a server's mods pushed, and the client's own underneath them.
//!
//! # Charter rule 14 lives in this file
//!
//! A font file is a parser running on bytes a server chose, and font parsers
//! are one of the most attacked surfaces in any client. What stands between one
//! and this process:
//!
//! - **Pure Rust.** egui parses through `ab_glyph`; there is no C codec here.
//! - **A size cap before anything parses**, applied where the bytes arrive
//!   ([`tiamot_core::font::MAX_FONT_BYTES`]).
//! - **A count cap**, so a server cannot push a hundred
//!   ([`tiamot_core::font::MAX_FONTS`]).
//! - **Panic isolation.** Installing runs inside `catch_unwind`; a font that
//!   kills the parser is dropped, the player is told, and every other font and
//!   the whole client carry on.
//! - **A fuzz target on the same entry point**, `fuzz/fuzz_targets/font_ingest.rs`.
//!
//! # Why installing is not free, and is therefore rare
//!
//! `egui::Context::set_fonts` throws away the glyph atlas and rebuilds it. Done
//! per frame that would be a permanent hitch; done per arrival it is one hitch
//! per font, once, at the start of a session. So this collects what has arrived
//! and rebuilds ONCE per batch, and never from the draw path.

use std::collections::BTreeMap;

/// The client's own font, which every mod that says nothing gets.
///
/// Named rather than anonymous because a mod's font is added BESIDE it and not
/// instead of it: a face with no glyph for something falls back here, which is
/// what stops a mod's display font turning half a dialog into empty boxes.
pub const FALLBACK: &str = "tiamot-fallback";

/// Every font this client can draw with.
pub struct Fonts {
    /// The bytes of every installed font, by id.
    ///
    /// **Kept rather than handed over.** `egui::Context::set_fonts` REPLACES
    /// the whole set, so every rebuild has to carry every font that came
    /// before — and egui owns what it was given, so the only way to offer one
    /// again is to have kept it. Bounded by `font::MAX_FONTS` files of
    /// `MAX_FONT_BYTES`, which is why keeping them is affordable.
    installed: BTreeMap<String, Vec<u8>>,
    /// Fonts that have arrived and are not installed yet.
    pending: Vec<(String, Vec<u8>)>,
    /// Ids that would not parse, so a second arrival is not retried for ever.
    refused: Vec<String>,
}

impl Fonts {
    /// An empty set: the client's own font and nothing else.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            installed: BTreeMap::new(),
            pending: Vec::new(),
            refused: Vec::new(),
        }
    }

    /// Takes a font's bytes, if they parse, to be installed on the next frame.
    ///
    /// Queued rather than installed here because installing rebuilds egui's
    /// glyph atlas, and this is called from the network pump.
    ///
    /// Returns whether it was accepted, so a caller can tell the player about
    /// a face that will not be used.
    pub fn offer(&mut self, id: String, bytes: Vec<u8>) -> bool {
        if self.refused.contains(&id) || self.installed.contains_key(&id) {
            return false;
        }
        if !parses(&bytes) {
            self.refused.push(id);
            return false;
        }
        self.pending.push((id, bytes));
        true
    }

    /// Whether anything is waiting to be installed.
    #[must_use]
    pub fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The family a style's font id draws in, or `None` for the client's own.
    ///
    /// **A name no font answers to is `None`, not an error.** A dialog whose
    /// lettering failed to arrive is still a dialog, and refusing to draw it
    /// would turn a missing file into a missing screen.
    #[must_use]
    pub fn family(&self, id: &str) -> Option<egui::FontFamily> {
        self.installed
            .contains_key(id)
            .then(|| egui::FontFamily::Name(family_name(id).into()))
    }

    /// How many mod fonts are installed, for the debug overlay.
    #[must_use]
    pub fn len(&self) -> usize {
        self.installed.len()
    }

    /// Whether only the client's own font is available.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.installed.is_empty()
    }

    /// Forgets every mod font, for a client leaving a world.
    ///
    /// A typeface belongs to the server that pushed it.
    pub fn clear(&mut self) {
        self.installed.clear();
        self.pending.clear();
        self.refused.clear();
    }

    /// Installs everything that has arrived, rebuilding egui's font set once.
    ///
    /// Returns the ids that would not parse, for whoever tells the player.
    ///
    /// # Where the isolation actually goes
    ///
    /// **Around `set_fonts`, not around the bytes.** `FontData::from_owned`
    /// only takes ownership; egui parses when it builds the glyph atlas, which
    /// happens inside `set_fonts`. `catch_unwind` around the wrapper would have
    /// isolated nothing and read as though it had — the worst kind of guard.
    ///
    /// When it does fire, every font in the batch is refused together: the
    /// panic says which atlas failed and not which face in it, and one bad font
    /// disabling a batch of at most eight is better than a client that dies on
    /// a screen a server chose to show it.
    pub fn install(&mut self, ctx: &egui::Context, bundled: &'static [u8]) -> Vec<String> {
        if self.pending.is_empty() {
            return Vec::new();
        }
        let arrived = std::mem::take(&mut self.pending);
        let names: Vec<String> = arrived.iter().map(|(id, _)| id.clone()).collect();
        for (id, bytes) in arrived {
            self.installed.insert(id, bytes);
        }

        if apply(ctx, bundled, &self.installed) {
            return Vec::new();
        }

        // The atlas would not build. Take the batch back out, refuse those ids
        // so they are not offered again, and rebuild from what worked before.
        for id in &names {
            self.installed.remove(id);
            self.refused.push(id.clone());
        }
        let _ = apply(ctx, bundled, &self.installed);
        names
    }
}

/// Builds egui's font set from the bundled font and every installed one.
///
/// Returns whether it survived. See [`Fonts::install`] for why the isolation is
/// here rather than around the parse.
fn apply(
    ctx: &egui::Context,
    bundled: &'static [u8],
    installed: &BTreeMap<String, Vec<u8>>,
) -> bool {
    let mut definitions = egui::FontDefinitions::empty();
    definitions.font_data.insert(
        FALLBACK.to_owned(),
        std::sync::Arc::new(egui::FontData::from_static(bundled)),
    );
    // Both families, because the client builds egui without `default_fonts`
    // and neither may be left empty — see `app::install_fonts`.
    for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
        definitions
            .families
            .entry(family)
            .or_default()
            .push(FALLBACK.to_owned());
    }
    for (id, bytes) in installed {
        let name = family_name(id);
        definitions.font_data.insert(
            name.clone(),
            std::sync::Arc::new(egui::FontData::from_owned(bytes.clone())),
        );
        // **Its own family, with the fallback behind it.** A display face with
        // no glyph for something falls through to the client's own rather than
        // drawing an empty box.
        definitions.families.insert(
            egui::FontFamily::Name(name.clone().into()),
            vec![name, FALLBACK.to_owned()],
        );
    }

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        ctx.set_fonts(definitions);
    }))
    .is_ok()
}

impl Default for Fonts {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether these bytes are a font a shaper can actually use.
///
/// # Why this exists, and why it is not a guard around `set_fonts`
///
/// **egui does not parse when it is given a font.** `FontData::from_owned`
/// takes ownership and `set_fonts` rebuilds definitions; the parse happens at
/// the first layout that names the family, which is inside a draw, per widget,
/// per frame. So isolation around installing protects nothing — a probe fed it
/// four kilobytes of zeroes and egui accepted them without complaint.
///
/// Charter rule 14 asks for the check BEFORE the allocation, so this is that
/// check: the bytes are parsed here, by `skrifa` — the same parser `epaint`
/// uses underneath and the one `fuzz/fuzz_targets/font_ingest.rs` hammers —
/// inside `catch_unwind`, and only a face that answers real questions is
/// handed on.
///
/// Asking is the point. A table directory that merely parses is not a font a
/// shaper can use, and the first lookup is where a malformed table falls over
/// rather than the accept.
fn parses(bytes: &[u8]) -> bool {
    use skrifa::MetadataProvider as _;

    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Ok(font) = skrifa::FontRef::new(bytes) else {
            return false;
        };
        let metrics = font.metrics(
            skrifa::instance::Size::unscaled(),
            skrifa::instance::LocationRef::default(),
        );
        if metrics.units_per_em == 0 {
            return false;
        }
        // A face with no character map cannot draw a single letter of anybody's
        // dialog, whatever else it contains.
        let charmap = font.charmap();
        let outlines = font.outline_glyphs();
        ['A', 'a', '0', ' ']
            .into_iter()
            .filter_map(|c| charmap.map(c))
            .any(|id| outlines.get(id).is_some())
    }))
    .unwrap_or(false)
}

/// The egui family name for a mod's font id.
///
/// Prefixed so a mod cannot name a family the engine uses — `mod:fallback`
/// would otherwise replace the client's own font in every widget that did not
/// ask for one.
fn family_name(id: &str) -> String {
    format!("mod-font-{id}")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The client's own font, as a stand-in for a mod's.
    ///
    /// A real TrueType file, because these tests exercise a parser: a buffer of
    /// zeroes would prove the plumbing and nothing about the face.
    pub(super) const REAL: &[u8] = include_bytes!("../assets/third-party/go-font/Go-Mono.ttf");

    #[test]
    fn a_font_nobody_registered_draws_in_the_clients_own() {
        // A name no font answers to is `None` rather than an error: a dialog
        // whose lettering failed to arrive is still a dialog, and refusing it
        // would turn a missing file into a missing screen.
        let fonts = Fonts::new();
        assert!(fonts.family("nobody:nothing").is_none());
        assert!(fonts.is_empty());
    }

    #[test]
    fn a_mod_cannot_name_the_engines_own_family() {
        // Without the prefix a mod registering `fallback` would replace the
        // client's own font everywhere, including in widgets that asked for no
        // font at all.
        assert_ne!(family_name("anything:fallback"), FALLBACK);
        assert!(family_name("a:b").starts_with("mod-font-"));
    }

    #[test]
    fn a_real_font_installs_and_a_style_can_name_it() {
        let ctx = egui::Context::default();
        let mut fonts = Fonts::new();
        assert!(fonts.offer("lettering:display".to_owned(), REAL.to_vec()));
        assert!(fonts.has_pending());

        let refused = fonts.install(&ctx, REAL);
        assert!(refused.is_empty(), "a real font was refused: {refused:?}");
        assert!(
            fonts.family("lettering:display").is_some(),
            "the font installed but no style can name it"
        );
        assert!(!fonts.has_pending(), "the batch was not taken");
    }

    #[test]
    fn a_font_that_will_not_parse_is_refused_and_the_client_survives() {
        // **Charter rule 14, as the assertion that matters.** A font file is a
        // parser running on bytes a server pushed. What must not happen is the
        // client dying on a screen a server chose to show it — so a face that
        // cannot be read is dropped, named to the caller, and everything else
        // carries on.
        let mut fonts = Fonts::new();
        assert!(
            !fonts.offer("hostile:face".to_owned(), vec![0x00; 4096]),
            "four kilobytes of zeroes were accepted as a typeface"
        );
        assert!(!fonts.has_pending(), "garbage was queued for installation");
        assert!(fonts.family("hostile:face").is_none());

        // **And it is refused where egui never sees it.** This is the whole
        // correction: `set_fonts` takes a font's bytes WITHOUT reading them and
        // parses at the first layout that names the family, so a guard around
        // installing isolates nothing — a probe fed egui these same zeroes and
        // it accepted them without complaint. `parses` is the guard, and it
        // runs before anything is handed over.
        assert!(!parses(&[0x00; 4096]), "zeroes parsed as a font");
        assert!(!parses(&REAL[..512]), "half a font parsed as a font");
        assert!(parses(REAL), "a real font did not parse");

        // Nor is it offered again on a reconnect, or by a second mod sharing
        // the hash.
        assert!(!fonts.offer("hostile:face".to_owned(), vec![0x00; 4096]));
    }

    #[test]
    fn a_second_font_does_not_displace_the_first() {
        // `set_fonts` REPLACES egui's whole set, so a rebuild that carried only
        // the new font would silently drop every earlier one — and the symptom
        // would be a dialog that lost its lettering when an unrelated mod's
        // font arrived.
        let ctx = egui::Context::default();
        let mut fonts = Fonts::new();
        assert!(fonts.offer("first:face".to_owned(), REAL.to_vec()));
        let _ = fonts.install(&ctx, REAL);
        assert!(fonts.offer("second:face".to_owned(), REAL.to_vec()));
        let _ = fonts.install(&ctx, REAL);

        assert!(
            fonts.family("first:face").is_some(),
            "the first font was dropped when the second arrived"
        );
        assert!(fonts.family("second:face").is_some());
        assert_eq!(fonts.len(), 2);
    }

    #[test]
    fn a_refused_font_is_not_retried_for_ever() {
        // Content arrives more than once — a reconnect, a second mod sharing
        // the hash — and a font that killed the parser must not be handed to it
        // again on every one.
        let mut fonts = Fonts::new();
        fonts.refused.push("bad:face".to_owned());
        assert!(!fonts.offer("bad:face".to_owned(), vec![0; 16]));
        assert!(
            !fonts.has_pending(),
            "a font that was already refused was queued again"
        );
    }
}

#[cfg(test)]
mod probe {
    use super::*;

    /// What egui does with bytes that are not a font, so the guard above is
    /// known to be exercised rather than assumed.
    #[test]
    #[ignore = "a probe, not a gate; run with --ignored --nocapture"]
    fn what_happens_to_garbage() {
        let ctx = egui::Context::default();
        for (name, bytes) in [
            ("zeroes", vec![0x00; 4096]),
            ("truncated real", super::tests::REAL[..512].to_vec()),
            ("random-ish", (0..4096u32).map(|i| (i * 7) as u8).collect()),
        ] {
            let mut fonts = Fonts::new();
            fonts.offer("probe:face".to_owned(), bytes);
            let refused = fonts.install(&ctx, super::tests::REAL);
            println!(
                "{name}: refused = {:?}, nameable = {}",
                refused,
                fonts.family("probe:face").is_some()
            );
        }
    }
}
