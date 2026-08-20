// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Fuzzes the dialog schema a server sends a client.
//!
//! **Charter rule 14: a parser ships with its fuzz target, in the same task.**
//! A dialog is the server-pushed asset with the most obvious reason to want to
//! be code, and a widget tree is the shape that goes wrong worst — it nests, so
//! a walk over an attacker's tree is a stack overflow, and it branches, so a
//! small message describes an enormous amount of layout work.
//!
//! The properties, in the order they matter:
//!
//! 1. **Decoding never panics and never overflows the stack.** This is the one
//!    the flat representation exists for. `Tree` is a `Vec` with child index
//!    ranges precisely so that serde has nothing to recurse on, and so that
//!    dropping a hostile tree is a `Vec` drop rather than a recursive one.
//! 2. **`check` answers, whatever it is handed.** It is the first thing a
//!    server's tree meets, so it has the least excuse for a crash — and it runs
//!    on trees with random child ranges, which is most of what arrives here.
//! 3. **A tree that PASSED the check lays out.** Layout is the consumer, and a
//!    tree the checker blessed must not then take the renderer down. Its
////   rectangles must be non-negative, because a renderer turns them into
//!    buffer offsets and a negative one is a very different kind of bug.
//! 4. **Round-trip identity**, for anything that decodes: what this crate
//!    accepted, it must re-encode to the same thing. A tree that survives the
//!    decoder but cannot be reproduced is a format the two halves disagree
//!    about.
//!
//! **Seeding**: random bytes reach real trees here without help, because
//! postcard is a compact format with no magic to guess — a short random string
//! is a plausible small tree. Named `seed-*.bin` files in the corpus are shapes
//! worth keeping around anyway, and `regression-*.bin` are inputs that have
//! crashed this.
//!
//! Run: `cargo +nightly fuzz run dialog_schema`
#![no_main]

use libfuzzer_sys::fuzz_target;
use tiamot_core::ui::{Limits, Measure, Rect, Style, Tree, Widget, check, layout};

/// Every leaf is ten by ten.
///
/// A fuzz target has no font and wants none: what is under test is the
/// arithmetic over a hostile tree, not what a glyph measures.
struct Ruler;

impl Measure for Ruler {
    fn natural(&self, _widget: &Widget, _style: &Style) -> (i32, i32) {
        (10, 10)
    }
}

fuzz_target!(|data: &[u8]| {
    // Property 1. Most inputs are not a tree, and should not be.
    let Ok(tree) = tiamot_core::proto::decode::<Tree>(data) else {
        return;
    };

    // Property 4, before anything else touches it.
    let bytes = tiamot_core::proto::encode(&tree).expect("what decoded must encode");
    let again: Tree =
        tiamot_core::proto::decode(&bytes).expect("what this crate encoded, it must decode");
    assert_eq!(
        tree, again,
        "a tree decoded, re-encoded, and decoded to something else"
    );

    // Property 2.
    if check(&tree, Limits::default()).is_err() {
        return;
    }

    // Property 3: only for a tree the checker passed.
    let laid = layout(&tree, Rect::new(0, 0, 1920, 1080), &Ruler);
    let mut stack = vec![&laid];
    let mut seen = 0usize;
    while let Some(node) = stack.pop() {
        assert!(
            node.rect.w >= 0 && node.rect.h >= 0,
            "a checked tree laid out to a negative rectangle: {:?}",
            node.rect
        );
        seen += 1;
        // A checked tree cannot produce more laid nodes than it has widgets.
        // If it does, a walk is visiting something twice — which is the cycle
        // the index-ordering invariant is supposed to have made impossible.
        assert!(
            seen <= tree.nodes.len().saturating_add(1),
            "layout produced more nodes than the tree has"
        );
        for child in &node.children {
            stack.push(child);
        }
    }
});
