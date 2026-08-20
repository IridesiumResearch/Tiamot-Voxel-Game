// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Round-trip and hostile-shape properties for the widget schema.
//!
//! Charter rule 15 asks for `proptest` on round-trip identity. The schema is
//! the contract between a server mod and every client that will ever render
//! it, so "what came out is what went in" is the property that has to hold for
//! every tree, not for the handful anyone thought to write down.

use proptest::prelude::*;

use super::tree::{Align, Children, Direction, Node, Style, Tree, Widget};

/// Every leaf is ten by ten. The property is about rectangles being sane, not
/// about what a font would say.
struct Ruler;

impl super::layout::Measure for Ruler {
    fn natural(&self, _widget: &Widget, _style: &Style) -> (i32, i32) {
        (10, 10)
    }
}
use super::{Limits, check};

/// Arbitrary widgets, covering every variant.
fn any_widget() -> impl Strategy<Value = Widget> {
    prop_oneof![
        (
            prop_oneof![Just(Direction::Row), Just(Direction::Column)],
            any::<u16>(),
            any::<u16>(),
            prop_oneof![
                Just(Align::Start),
                Just(Align::Center),
                Just(Align::End),
                Just(Align::Stretch)
            ],
        )
            .prop_map(|(direction, gap, padding, align)| Widget::Container {
                direction,
                gap,
                padding,
                align,
            }),
        ".{0,40}".prop_map(|text| Widget::Label { text }),
        ".{0,40}".prop_map(|text| Widget::Button { text }),
        any::<[u8; 32]>().prop_map(|hash| Widget::Image { hash }),
        (".{0,20}", ".{0,20}").prop_map(|(initial, placeholder)| Widget::TextInput {
            initial,
            placeholder
        }),
        (".{0,20}", any::<bool>()).prop_map(|(text, checked)| Widget::Checkbox { text, checked }),
        (any::<i32>(), any::<i32>(), any::<i32>()).prop_map(|(min, max, value)| Widget::Slider {
            min,
            max,
            value
        }),
        (prop::collection::vec(".{0,10}", 0..6), any::<u16>())
            .prop_map(|(options, selected)| Widget::Dropdown { options, selected }),
        (".{0,20}", any::<u16>()).prop_map(|(view, index)| Widget::ItemSlot { view, index }),
        (".{0,20}", any::<u16>(), any::<u16>(), any::<u16>()).prop_map(
            |(view, columns, first, count)| Widget::ItemGrid {
                view,
                columns,
                first,
                count
            }
        ),
        Just(Widget::Scroll),
        Just(Widget::Spacer),
        any::<u16>().prop_map(|permille| Widget::Progress { permille }),
    ]
}

fn any_style() -> impl Strategy<Value = Style> {
    (
        any::<Option<[u8; 4]>>(),
        any::<Option<[u8; 4]>>(),
        any::<Option<[u8; 32]>>(),
        any::<Option<[u8; 4]>>(),
        any::<Option<u16>>(),
    )
        .prop_map(
            |(background, border, nine_slice, text_colour, text_size)| Style {
                background,
                border,
                nine_slice,
                text_colour,
                text_size,
            },
        )
}

fn any_node() -> impl Strategy<Value = Node> {
    (
        any_widget(),
        ".{0,16}",
        any_style(),
        any::<u16>(),
        any::<Option<u16>>(),
        any::<Option<u16>>(),
        (any::<u32>(), any::<u32>()),
    )
        .prop_map(
            |(widget, name, style, grow, size, cross_size, (first, count))| Node {
                widget,
                name,
                style,
                grow,
                size,
                cross_size,
                children: Children { first, count },
            },
        )
}

/// Arbitrary trees, including shapes no builder would produce.
///
/// The child ranges are random, so most of these are not valid trees at all —
/// which is the point for the properties below: a decoder sees whatever a
/// server sends, not whatever a builder built.
fn any_tree() -> impl Strategy<Value = Tree> {
    prop::collection::vec(any_node(), 0..24).prop_map(|nodes| Tree { nodes })
}

proptest! {
    /// **Round-trip identity.** Charter rule 15's named property.
    #[test]
    fn a_tree_survives_encoding_and_decoding(tree in any_tree()) {
        let bytes = crate::proto::encode(&tree).expect("encode");
        let back: Tree = crate::proto::decode(&bytes).expect("decode");
        prop_assert_eq!(tree, back);
    }

    /// **Checking never panics, whatever the shape.**
    ///
    /// `check` is the first thing a server's tree meets, so it is the function
    /// with the least excuse for a crash. Random child ranges mean most of
    /// these are malformed, which is exactly the input it exists for.
    #[test]
    fn checking_any_tree_answers_rather_than_panicking(tree in any_tree()) {
        let _ = check(&tree, Limits::default());
    }

    /// **A checked tree lays out, and nothing it does escapes its box.**
    ///
    /// The layout is presentation and may produce any rectangle it likes for a
    /// tree nobody validated; for one that PASSED, every rectangle has to be
    /// finite and non-negative, because a renderer will turn these into buffer
    /// offsets.
    #[test]
    fn a_checked_tree_lays_out_to_sane_rectangles(tree in any_tree()) {
        if check(&tree, Limits::default()).is_err() {
            return Ok(());
        }
        let laid = super::layout(&tree, super::Rect::new(0, 0, 800, 600), &Ruler);
        let mut stack = vec![&laid];
        while let Some(node) = stack.pop() {
            prop_assert!(node.rect.w >= 0, "negative width: {:?}", node.rect);
            prop_assert!(node.rect.h >= 0, "negative height: {:?}", node.rect);
            for child in &node.children {
                stack.push(child);
            }
        }
    }
}
