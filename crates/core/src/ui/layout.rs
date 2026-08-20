// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Turning a widget tree into rectangles.
//!
//! # Why the layout is here and the measuring is not
//!
//! Flex is arithmetic: sum the children, share out what is left, place them in
//! order. None of that needs a font, a window, or a GPU, so all of it belongs
//! in `core` where it can be tested headlessly against a table of expected
//! rectangles.
//!
//! Measuring a LEAF does need a font — how wide is this label, in this size? —
//! and charter rule 3 forbids `core` from having one. So measurement is a
//! trait the caller supplies. The client implements it with real font metrics;
//! a test implements it with numbers it chose, which is what makes the flex
//! arithmetic assertable at all.
//!
//! # Integers, and where the leftover pixel goes
//!
//! Rectangles are `i32` virtual pixels. Sharing space between children with
//! `grow` weights divides, and division leaves a remainder — so **the leftover
//! is given one pixel at a time to the earliest growing children**. That rule
//! is arbitrary; having it written down and tested is not. Without it, two
//! renderers would disagree by a pixel and nobody would know which was right.
//!
//! Determinism (charter rule 4) does not reach here — this is presentation —
//! but integer arithmetic means it happens to be exact anyway.

use super::tree::{Align, Direction, Style, Tree, Widget};

/// A rectangle in virtual pixels, top-left origin.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    /// Left edge.
    pub x: i32,
    /// Top edge.
    pub y: i32,
    /// Width.
    pub w: i32,
    /// Height.
    pub h: i32,
}

impl Rect {
    /// A rectangle from its parts.
    #[must_use]
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        Self { x, y, w, h }
    }

    /// This rectangle shrunk by `by` on every side, never below zero size.
    #[must_use]
    pub const fn inset(self, by: i32) -> Self {
        Self {
            x: self.x + by,
            y: self.y + by,
            w: if self.w > by * 2 { self.w - by * 2 } else { 0 },
            h: if self.h > by * 2 { self.h - by * 2 } else { 0 },
        }
    }
}

/// How big a leaf widget wants to be.
///
/// Implemented by the client with real font metrics, and by tests with numbers
/// they chose. Sizes are in virtual pixels and are a REQUEST — the layout may
/// give a widget less, and a renderer must clip rather than overflow.
pub trait Measure {
    /// The natural width and height of a widget that has no children.
    fn natural(&self, widget: &Widget, style: &Style) -> (i32, i32);
}

/// A node's rectangle, and its children's.
///
/// Mirrors the tree exactly, so a renderer walks both together and cannot
/// misalign them the way a flat list indexed by a separate traversal could.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Laid {
    /// Where this node is.
    pub rect: Rect,
    /// Its children, in the tree's order.
    pub children: Vec<Laid>,
}

/// Lays a tree out inside `area`.
///
/// The root fills `area`; everything below it is placed by the flex rules in
/// the module docs. Walks by index rather than by reference, so nothing here
/// recurses on a shape a server chose — see [`Tree`] for why that matters.
///
/// A tree that has not passed [`super::check`] still terminates: `children_of`
/// returns an empty range for anything out of bounds, and the index-ordering
/// invariant means a walk cannot revisit a node. It may lay out nonsense, which
/// is the caller's fault for skipping the check.
#[must_use]
pub fn layout(tree: &Tree, area: Rect, measure: &dyn Measure) -> Laid {
    if tree.nodes.is_empty() {
        return Laid {
            rect: area,
            children: Vec::new(),
        };
    }
    place(tree, 0, area, measure, 0)
}

/// Places one node in the box it has been given, then its children inside it.
fn place(tree: &Tree, index: usize, rect: Rect, measure: &dyn Measure, depth: usize) -> Laid {
    if depth >= MAX_WALK_DEPTH {
        return Laid {
            rect,
            children: Vec::new(),
        };
    }
    let Some(node) = tree.nodes.get(index) else {
        return Laid {
            rect,
            children: Vec::new(),
        };
    };
    match &node.widget {
        Widget::Container {
            direction,
            gap,
            padding,
            align,
        } => {
            let inner = rect.inset(i32::from(*padding));
            let children = place_children(
                tree,
                index,
                inner,
                Flow {
                    direction: *direction,
                    gap: i32::from(*gap),
                    align: *align,
                },
                measure,
                depth,
            );
            Laid { rect, children }
        }
        // A scroll's children get the full width and as much height as they
        // want, because scrolling is precisely the case where content is
        // allowed to be bigger than its box. The renderer clips it.
        Widget::Scroll => {
            let children = tree
                .children_of(index)
                .map(|child| {
                    let (_, wanted) = natural_of(tree, child, measure);
                    let inner = Rect::new(rect.x, rect.y, rect.w, wanted.max(rect.h));
                    place(tree, child, inner, measure, depth + 1)
                })
                .collect();
            Laid { rect, children }
        }
        _ => Laid {
            rect,
            children: Vec::new(),
        },
    }
}

/// How a container wants its children arranged.
///
/// A struct rather than six parameters: they travel together, they are read
/// together, and passing them separately is how a `gap` ends up where an
/// `align` was meant to go.
#[derive(Debug, Clone, Copy)]
struct Flow {
    direction: Direction,
    gap: i32,
    align: Align,
}

/// The flex pass: share the main axis, then place along it.
fn place_children(
    tree: &Tree,
    parent: usize,
    inner: Rect,
    flow: Flow,
    measure: &dyn Measure,
    depth: usize,
) -> Vec<Laid> {
    let Flow {
        direction,
        gap,
        align,
    } = flow;
    let range = tree.children_of(parent);
    if range.is_empty() {
        return Vec::new();
    }
    let kids: Vec<usize> = range.collect();
    let horizontal = matches!(direction, Direction::Row);
    let main_total = if horizontal { inner.w } else { inner.h };
    let cross_total = if horizontal { inner.h } else { inner.w };

    // Every child's starting size along the main axis: what it asked for, or
    // what it naturally wants.
    let mut mains: Vec<i32> = kids
        .iter()
        .map(|&child| {
            let node = &tree.nodes[child];
            node.size.map_or_else(
                || {
                    let (w, h) = natural_of(tree, child, measure);
                    if horizontal { w } else { h }
                },
                i32::from,
            )
        })
        .collect();

    let gaps = gap * i32::try_from(kids.len().saturating_sub(1)).unwrap_or(0);
    let used: i32 = mains.iter().sum::<i32>() + gaps;
    let spare = main_total - used;
    let weights: u32 = kids
        .iter()
        .map(|&child| u32::from(tree.nodes[child].grow))
        .sum();

    if spare > 0 && weights > 0 {
        share(tree, &mut mains, &kids, spare, weights);
    } else if spare < 0 {
        // Over-full. Shrink proportionally rather than letting the last child
        // run off the edge — a dialog that overflows its own window is worse
        // than one that is a little cramped.
        shrink(&mut mains, used, main_total);
    }

    let mut laid = Vec::with_capacity(kids.len());
    let mut cursor = if horizontal { inner.x } else { inner.y };
    for (&child, main) in kids.iter().zip(mains) {
        let node = &tree.nodes[child];
        let cross = node.cross_size.map_or_else(
            || {
                if matches!(align, Align::Stretch) {
                    cross_total
                } else {
                    let (w, h) = natural_of(tree, child, measure);
                    let want = if horizontal { h } else { w };
                    want.min(cross_total)
                }
            },
            i32::from,
        );
        let offset = match align {
            Align::Start | Align::Stretch => 0,
            Align::Center => (cross_total - cross) / 2,
            Align::End => cross_total - cross,
        };
        let rect = if horizontal {
            Rect::new(cursor, inner.y + offset, main, cross)
        } else {
            Rect::new(inner.x + offset, cursor, cross, main)
        };
        laid.push(place(tree, child, rect, measure, depth + 1));
        cursor += main + gap;
    }
    laid
}

/// Shares `spare` between the growing children, by weight.
///
/// The remainder goes one pixel at a time to the earliest growers — see the
/// module docs for why that rule is written down rather than left to chance.
fn share(tree: &Tree, mains: &mut [i32], kids: &[usize], spare: i32, weights: u32) {
    let weights = i32::try_from(weights).unwrap_or(i32::MAX);
    let mut handed = 0;
    for (main, &child) in mains.iter_mut().zip(kids) {
        let grow = tree.nodes[child].grow;
        if grow == 0 {
            continue;
        }
        let portion = spare * i32::from(grow) / weights;
        *main += portion;
        handed += portion;
    }
    let mut remainder = spare - handed;
    for (main, &child) in mains.iter_mut().zip(kids) {
        if remainder <= 0 {
            break;
        }
        if tree.nodes[child].grow > 0 {
            *main += 1;
            remainder -= 1;
        }
    }
}

/// Scales every child down so the row fits, never below zero.
fn shrink(mains: &mut [i32], used: i32, available: i32) {
    if used <= 0 {
        return;
    }
    let available = available.max(0);
    for main in mains.iter_mut() {
        *main = (*main * available / used).max(0);
    }
}

/// A leaf's natural size, or a container's from its children.
///
/// Containers measure by summing, which is what lets a column inside a row take
/// only the width it needs.
///
/// **Recursive, and safe because the tree is not.** A child's index is always
/// greater than its parent's ([`Tree`]'s invariant), and [`super::check`] caps
/// depth, so this nests at most `Limits::depth` deep however large the tree is.
/// The `depth` guard is a second belt for a caller who skipped the check.
fn natural_of(tree: &Tree, index: usize, measure: &dyn Measure) -> (i32, i32) {
    natural_at(tree, index, measure, 0)
}

/// How deep any walk in this module will nest before giving up.
///
/// Matches `Limits::depth`'s default. A tree that passed [`super::check`] never
/// reaches it; one that did not is bounded here rather than on the stack.
///
/// **Both walks need this, which a test had to prove.** `place` recurses into
/// children, so a node claiming ITSELF as a child recurses for ever — the
/// index-ordering invariant makes that unrepresentable, but only for a tree
/// somebody checked, and layout must not be the thing that assumes it.
const MAX_WALK_DEPTH: usize = 32;

fn natural_at(tree: &Tree, index: usize, measure: &dyn Measure, depth: usize) -> (i32, i32) {
    if depth >= MAX_WALK_DEPTH {
        return (0, 0);
    }
    let Some(node) = tree.nodes.get(index) else {
        return (0, 0);
    };
    if let (Some(w), Some(h)) = (node.size, node.cross_size) {
        return (i32::from(w), i32::from(h));
    }
    match &node.widget {
        Widget::Container {
            direction,
            gap,
            padding,
            ..
        } => {
            let gap = i32::from(*gap);
            let pad = i32::from(*padding) * 2;
            let mut main = 0;
            let mut cross = 0;
            for (position, child) in tree.children_of(index).enumerate() {
                let (w, h) = natural_at(tree, child, measure, depth + 1);
                let (child_main, child_cross) = match direction {
                    Direction::Row => (w, h),
                    Direction::Column => (h, w),
                };
                main += child_main + if position > 0 { gap } else { 0 };
                cross = cross.max(child_cross);
            }
            match direction {
                Direction::Row => (main + pad, cross + pad),
                Direction::Column => (cross + pad, main + pad),
            }
        }
        Widget::Scroll => tree
            .children_of(index)
            .map(|child| natural_at(tree, child, measure, depth + 1))
            .fold((0, 0), |acc, (w, h)| (acc.0.max(w), acc.1 + h)),
        widget => measure.natural(widget, &node.style),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tree::{Align, Build, Direction, Node, Widget};

    /// A measurer with numbers a test chose, which is the point of the trait.
    ///
    /// Every leaf is 10x10 unless it is text, which is 8 virtual pixels per
    /// byte and 12 tall — near enough a font to make the arithmetic legible in
    /// an assertion, and deliberately nothing to do with a real one.
    struct Ruler;

    impl Measure for Ruler {
        fn natural(&self, widget: &Widget, _style: &Style) -> (i32, i32) {
            match widget {
                Widget::Label { text } | Widget::Button { text } => {
                    (i32::try_from(text.len()).unwrap_or(0) * 8, 12)
                }
                Widget::Spacer => (0, 0),
                _ => (10, 10),
            }
        }
    }

    fn container(direction: Direction, gap: u16, padding: u16, align: Align) -> Node {
        Node::new(Widget::Container {
            direction,
            gap,
            padding,
            align,
        })
    }

    fn row(children: Vec<Build>) -> Tree {
        Build::of(container(Direction::Row, 0, 0, Align::Start), children).flatten()
    }

    fn grown(grow: u16) -> Build {
        Build::of(
            Node {
                grow,
                ..Node::new(Widget::Spacer)
            },
            Vec::new(),
        )
    }

    fn sized(size: u16) -> Build {
        Build::of(
            Node {
                size: Some(size),
                ..Node::new(Widget::Spacer)
            },
            Vec::new(),
        )
    }

    fn widths(laid: &Laid) -> Vec<i32> {
        laid.children.iter().map(|c| c.rect.w).collect()
    }

    #[test]
    fn fixed_children_sit_end_to_end_with_the_gap_between_them() {
        let tree = Build::of(
            container(Direction::Row, 4, 2, Align::Start),
            vec![sized(10), sized(20), sized(30)],
        )
        .flatten();
        let laid = layout(&tree, Rect::new(0, 0, 100, 50), &Ruler);
        let xs: Vec<i32> = laid.children.iter().map(|c| c.rect.x).collect();
        // Padding moves the first child in by 2; each gap adds 4.
        assert_eq!(xs, vec![2, 16, 40], "{laid:#?}");
        assert_eq!(widths(&laid), vec![10, 20, 30]);
    }

    #[test]
    fn grow_shares_what_is_left_by_weight() {
        // 100 wide, 20 taken by the fixed child, 80 to share between weights
        // 1 and 3 — so 20 and 60.
        let tree = row(vec![sized(20), grown(1), grown(3)]);
        let laid = layout(&tree, Rect::new(0, 0, 100, 10), &Ruler);
        assert_eq!(widths(&laid), vec![20, 20, 60], "{laid:#?}");
    }

    #[test]
    fn the_leftover_pixel_goes_to_the_earliest_growers() {
        // 100 between three equal weights is 33 each, with 1 left over. The
        // rule is written down in the module docs precisely so this is not a
        // renderer's private business.
        let tree = row(vec![grown(1), grown(1), grown(1)]);
        let laid = layout(&tree, Rect::new(0, 0, 100, 10), &Ruler);
        assert_eq!(widths(&laid), vec![34, 33, 33], "{laid:#?}");
        assert_eq!(
            widths(&laid).iter().sum::<i32>(),
            100,
            "the row does not fill its box"
        );
    }

    #[test]
    fn a_row_that_does_not_fit_shrinks_instead_of_overflowing() {
        // Three 100-wide children in a 150-wide box. A dialog that ran off its
        // own window would be worse than a cramped one.
        let tree = row(vec![sized(100), sized(100), sized(100)]);
        let laid = layout(&tree, Rect::new(0, 0, 150, 10), &Ruler);
        let ws = widths(&laid);
        assert!(ws.iter().sum::<i32>() <= 150, "the row overflowed: {ws:?}");
        assert!(ws.iter().all(|w| *w > 0), "a child was shrunk away: {ws:?}");
    }

    #[test]
    fn alignment_puts_a_short_child_where_it_was_asked_to_go() {
        // A 10-tall child in a 50-tall row. `cross_size`, not `size`: `size` is
        // the MAIN axis, so setting it on a row's child makes it WIDE rather
        // than tall — which is what this test said the first time it was
        // written, and it duly centred a zero-height child.
        let short = || {
            Build::of(
                Node {
                    cross_size: Some(10),
                    ..Node::new(Widget::Spacer)
                },
                Vec::new(),
            )
        };
        for (align, expected) in [(Align::Start, 0), (Align::Center, 20), (Align::End, 40)] {
            let tree = Build::of(container(Direction::Row, 0, 0, align), vec![short()]).flatten();
            let laid = layout(&tree, Rect::new(0, 0, 100, 50), &Ruler);
            assert_eq!(
                laid.children[0].rect.y, expected,
                "{align:?} put it at {}",
                laid.children[0].rect.y
            );
        }

        // Stretch fills, rather than sitting somewhere.
        let tree = Build::of(
            container(Direction::Row, 0, 0, Align::Stretch),
            vec![Build::leaf(Widget::Spacer)],
        )
        .flatten();
        let laid = layout(&tree, Rect::new(0, 0, 100, 50), &Ruler);
        assert_eq!(laid.children[0].rect.h, 50, "stretch should fill");

        // And an explicit cross size BEATS stretch, because a mod that said how
        // tall it wanted something meant it.
        let tree = Build::of(
            container(Direction::Row, 0, 0, Align::Stretch),
            vec![short()],
        )
        .flatten();
        let laid = layout(&tree, Rect::new(0, 0, 100, 50), &Ruler);
        assert_eq!(laid.children[0].rect.h, 10, "an explicit size was ignored");
    }

    #[test]
    fn a_column_measures_itself_from_its_children() {
        // The natural size of a container is what its children need, which is
        // what lets a column inside a row take only the width it wants.
        let tree = Build::of(
            container(Direction::Column, 2, 3, Align::Start),
            vec![
                Build::leaf(Widget::Label {
                    text: "abcd".to_owned(),
                }),
                Build::leaf(Widget::Label {
                    text: "ab".to_owned(),
                }),
            ],
        )
        .flatten();
        let (w, h) = natural_of(&tree, 0, &Ruler);
        // Widest child is 4 bytes * 8 = 32, plus 3 padding either side.
        assert_eq!(w, 32 + 6, "width");
        // Two 12-tall labels, one 2 gap, plus 3 padding either side.
        assert_eq!(h, 12 + 2 + 12 + 6, "height");
    }

    #[test]
    fn the_degenerate_trees_lay_out_rather_than_panicking() {
        // A mod will ship every one of these.
        let empty = row(Vec::new());
        let laid = layout(&empty, Rect::new(0, 0, 40, 40), &Ruler);
        assert!(laid.children.is_empty());
        assert_eq!(laid.rect, Rect::new(0, 0, 40, 40));

        // A tree with no nodes at all — what an empty message decodes to.
        let nothing = Tree { nodes: Vec::new() };
        let laid = layout(&nothing, Rect::new(0, 0, 40, 40), &Ruler);
        assert!(laid.children.is_empty());

        // A box so small the padding eats it entirely.
        let padded = Build::of(
            container(Direction::Row, 0, 50, Align::Start),
            vec![sized(10)],
        )
        .flatten();
        let laid = layout(&padded, Rect::new(0, 0, 20, 20), &Ruler);
        assert_eq!(laid.rect.inset(50).w, 0, "inset should clamp at zero");
        assert_eq!(laid.children.len(), 1);
    }

    #[test]
    fn a_tree_that_never_passed_the_check_still_terminates() {
        // **Layout must not be the thing that enforces shape.** A child range
        // pointing off the end, and one pointing backwards, are both refused by
        // `check` — but layout is what a caller might reach first, and it has
        // to come back rather than loop or panic.
        let mut tree = row(vec![sized(10), sized(20)]);
        tree.nodes[0].children = crate::ui::Children {
            first: 900,
            count: 5,
        };
        let laid = layout(&tree, Rect::new(0, 0, 100, 50), &Ruler);
        assert!(laid.children.is_empty(), "an out-of-range range was walked");

        // **A node claiming itself as a child.** `children_of` is in range
        // here, so only the index-ordering invariant stops this being a cycle —
        // and `check` is what enforces that. Layout must not assume it has run.
        //
        // This test was written believing layout could not loop "because it
        // walks each range once". It recursed until the stack died. `place`
        // carries a depth for exactly this.
        let mut tree = row(vec![sized(10)]);
        tree.nodes[0].children = crate::ui::Children { first: 0, count: 1 };
        let laid = layout(&tree, Rect::new(0, 0, 100, 50), &Ruler);
        let mut deepest = 0;
        let mut at = &laid;
        while let Some(child) = at.children.first() {
            deepest += 1;
            at = child;
        }
        assert!(
            deepest < 64,
            "layout nested {deepest} deep on a self-parenting node"
        );
    }
}
