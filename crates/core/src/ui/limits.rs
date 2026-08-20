// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! What a widget tree may not exceed, and the errors for when it does.
//!
//! # Why a tree needs bounds at all
//!
//! Charter rule 14. A dialog arrives from a server a client has no reason to
//! trust, and a tree is the shape that goes wrong worst: it nests, so a
//! recursive walk over an attacker's tree is a stack overflow, and it branches,
//! so a small message can describe an enormous amount of layout work.
//!
//! Every limit here is checked BEFORE the tree is walked for any other purpose
//! — before layout, before rendering, before a slot is resolved. The decoder's
//! own size cap ([`crate::proto::MAX_MESSAGE_BYTES`]) bounds the bytes; this
//! bounds what those bytes are allowed to mean.

use super::tree::{Node, Tree, Widget};

/// Bounds on one widget tree.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// Deepest nesting. A tree at this depth still walks on a normal stack.
    pub depth: usize,
    /// Most nodes in the whole tree.
    pub nodes: usize,
    /// Longest single string — a label, a placeholder, one dropdown option.
    pub text_bytes: usize,
    /// Most options one dropdown may offer.
    pub options: usize,
    /// Most slots one [`Widget::ItemGrid`] may show.
    pub grid_slots: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            // A real dialog nests perhaps six deep. Thirty-two is generous for
            // anything hand-written and shallow enough that the recursive walks
            // in this module cannot run a thread out of stack.
            depth: 32,
            // A big inventory screen is a few hundred nodes. Four thousand is
            // room for something elaborate and a hard stop against a tree whose
            // only purpose is to be large.
            nodes: 4096,
            // A label is a sentence, not a document. Anything longer is either
            // a mistake or an attempt to make the client lay out a novel.
            text_bytes: 1024,
            // A dropdown a player has to scroll for a minute is already a
            // design problem; this is the runaway guard.
            options: 256,
            // A double chest is 54. This allows a very large container and
            // refuses a grid claiming a million slots.
            grid_slots: 1024,
        }
    }
}

/// Why a widget tree was refused.
///
/// Every variant names the limit and what was found, because these reach a mod
/// author as a Lua error and "invalid dialog" tells them nothing.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum UiError {
    /// The tree nests deeper than [`Limits::depth`].
    #[error("dialog nests {found} deep, over the limit of {limit}")]
    TooDeep {
        /// How deep it went.
        found: usize,
        /// The limit.
        limit: usize,
    },
    /// The tree holds more than [`Limits::nodes`] widgets.
    #[error("dialog has {found} widgets, over the limit of {limit}")]
    TooManyNodes {
        /// How many there were.
        found: usize,
        /// The limit.
        limit: usize,
    },
    /// A string is longer than [`Limits::text_bytes`].
    #[error("a {field} of {found} bytes is over the limit of {limit}")]
    TextTooLong {
        /// Which string.
        field: &'static str,
        /// How long it was.
        found: usize,
        /// The limit.
        limit: usize,
    },
    /// A list — dropdown options, grid slots — is too long.
    #[error("{field} has {found} entries, over the limit of {limit}")]
    TooManyEntries {
        /// Which list.
        field: &'static str,
        /// How many there were.
        found: usize,
        /// The limit.
        limit: usize,
    },
    /// A widget's own fields do not make sense together.
    #[error("{what}")]
    Malformed {
        /// What is wrong, in a mod author's terms.
        what: String,
    },
}

/// Adds the shape errors a flat tree can have that a nested one cannot.
impl UiError {
    /// A tree with no root.
    fn empty() -> Self {
        Self::Malformed {
            what: "a dialog has no widgets at all".to_owned(),
        }
    }
}

/// Checks a tree against `limits` in two linear passes.
///
/// # What is being defended against
///
/// Charter rule 14. This runs on a tree a server sent, before layout, before
/// rendering, before a slot is resolved.
///
/// Pass one is per node and independent of shape: strings, list lengths, and
/// fields that disagree with each other. It also enforces [`Tree`]'s ordering
/// invariant — **every child index is greater than its parent's** — which is
/// what makes a cycle unrepresentable and every later walk guaranteed to
/// terminate.
///
/// Pass two computes depth. It needs pass one to have run, because it walks the
/// child ranges and would otherwise be walking an attacker's graph.
///
/// # Errors
///
/// [`UiError`] naming the first limit exceeded.
pub fn check(tree: &Tree, limits: Limits) -> Result<(), UiError> {
    if tree.nodes.is_empty() {
        return Err(UiError::empty());
    }
    if tree.nodes.len() > limits.nodes {
        return Err(UiError::TooManyNodes {
            found: tree.nodes.len(),
            limit: limits.nodes,
        });
    }

    // Pass one: every node's own fields, and the ordering invariant.
    let total = tree.nodes.len();
    for (index, node) in tree.nodes.iter().enumerate() {
        check_node(node, limits)?;
        let first = node.children.first as usize;
        let count = node.children.count as usize;
        if count == 0 {
            continue;
        }
        let end = first.checked_add(count).ok_or_else(|| UiError::Malformed {
            what: format!("widget {index} claims a child range that overflows"),
        })?;
        if end > total {
            return Err(UiError::Malformed {
                what: format!("widget {index} claims children {first}..{end} of {total} widgets"),
            });
        }
        // **The invariant.** A child that came before its parent could close a
        // cycle, and a cycle is an infinite walk in every consumer of this
        // tree. Refused here so nothing downstream has to think about it.
        if first <= index {
            return Err(UiError::Malformed {
                what: format!("widget {index} claims child {first}, which is not after it"),
            });
        }
    }

    // Pass two: depth, with an explicit stack. A node reached twice would be a
    // second parent claiming it — not a cycle, since indices only increase, but
    // still a shape no tree has, and it would make this pass quadratic.
    let mut seen = vec![false; total];
    let mut deepest = 0usize;
    let mut stack = vec![(0usize, 1usize)];
    seen[0] = true;
    while let Some((index, depth)) = stack.pop() {
        deepest = deepest.max(depth);
        if depth > limits.depth {
            return Err(UiError::TooDeep {
                found: depth,
                limit: limits.depth,
            });
        }
        for child in tree.children_of(index) {
            if std::mem::replace(&mut seen[child], true) {
                return Err(UiError::Malformed {
                    what: format!("widget {child} is claimed as a child twice"),
                });
            }
            stack.push((child, depth + 1));
        }
    }
    let _ = deepest;
    Ok(())
}

/// One widget's own strings and lists. Nothing here looks at shape.
fn check_node(node: &Node, limits: Limits) -> Result<(), UiError> {
    let text = |field: &'static str, value: &str| -> Result<(), UiError> {
        if value.len() > limits.text_bytes {
            return Err(UiError::TextTooLong {
                field,
                found: value.len(),
                limit: limits.text_bytes,
            });
        }
        Ok(())
    };
    text("name", &node.name)?;

    match &node.widget {
        Widget::Label { text: value } => text("label", value)?,
        Widget::Button { text: value } => text("button", value)?,
        Widget::Checkbox { text: value, .. } => text("checkbox", value)?,
        Widget::TextInput {
            initial,
            placeholder,
        } => {
            text("text_input", initial)?;
            text("placeholder", placeholder)?;
        }
        Widget::Dropdown { options, selected } => {
            if options.len() > limits.options {
                return Err(UiError::TooManyEntries {
                    field: "dropdown options",
                    found: options.len(),
                    limit: limits.options,
                });
            }
            for option in options {
                text("dropdown option", option)?;
            }
            // An index past the end is not a selection. Caught rather than
            // clamped, because a mod that built the list wrong wants telling.
            if !options.is_empty() && usize::from(*selected) >= options.len() {
                return Err(UiError::Malformed {
                    what: format!("dropdown selects option {selected} of {}", options.len()),
                });
            }
        }
        Widget::Slider { min, max, value } => {
            if min > max {
                return Err(UiError::Malformed {
                    what: format!("slider has min {min} above max {max}"),
                });
            }
            if value < min || value > max {
                return Err(UiError::Malformed {
                    what: format!("slider value {value} is outside {min}..={max}"),
                });
            }
        }
        Widget::ItemSlot { view, .. } => text("view", view)?,
        Widget::ItemGrid {
            view,
            columns,
            count,
            ..
        } => {
            text("view", view)?;
            if usize::from(*count) > limits.grid_slots {
                return Err(UiError::TooManyEntries {
                    field: "grid slots",
                    found: usize::from(*count),
                    limit: limits.grid_slots,
                });
            }
            // A grid with no columns has no rows either, and would divide by
            // zero in any layout that tried.
            if *columns == 0 && *count > 0 {
                return Err(UiError::Malformed {
                    what: "an item grid with slots must have at least one column".to_owned(),
                });
            }
        }
        Widget::Progress { permille } => {
            if *permille > 1000 {
                return Err(UiError::Malformed {
                    what: format!("progress of {permille} permille is over 1000"),
                });
            }
        }
        Widget::Container { .. } | Widget::Image { .. } | Widget::Scroll | Widget::Spacer => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::tree::{Align, Build, Children, Direction, Widget};

    fn container() -> Node {
        Node::new(Widget::Container {
            direction: Direction::Column,
            gap: 0,
            padding: 0,
            align: Align::Start,
        })
    }

    fn one(widget: Widget) -> Tree {
        Build::leaf(widget).flatten()
    }

    /// A chain `depth` nodes long, built FLAT.
    ///
    /// Built directly rather than through `Build`, which nests and would blow
    /// the stack on its own drop at these depths — which is the whole reason
    /// the wire form is flat. This is the hostile shape, so it is constructed
    /// the hostile way.
    fn chain(depth: usize) -> Tree {
        let mut nodes = Vec::with_capacity(depth);
        for index in 0..depth {
            let last = index + 1 == depth;
            nodes.push(Node {
                children: if last {
                    Children::default()
                } else {
                    Children {
                        first: u32::try_from(index + 1).expect("fits"),
                        count: 1,
                    }
                },
                ..Node::new(if last {
                    Widget::Spacer
                } else {
                    Widget::Container {
                        direction: Direction::Column,
                        gap: 0,
                        padding: 0,
                        align: Align::Start,
                    }
                })
            });
        }
        Tree { nodes }
    }

    #[test]
    fn a_tree_deeper_than_the_limit_is_refused_without_recursing_into_it() {
        let limits = Limits::default();
        assert!(check(&chain(limits.depth), limits).is_ok(), "at the limit");
        let err = check(&chain(limits.depth + 1), limits).expect_err("over the limit");
        assert!(matches!(err, UiError::TooDeep { .. }), "{err}");
    }

    #[test]
    fn a_very_deep_tree_is_an_error_rather_than_a_stack_overflow() {
        // **The case the whole flat representation exists for.** A hundred
        // thousand deep aborts the process for anything that recurses — a
        // recursive decode, a recursive drop, or a checker that recursed to
        // enforce the depth limit and had to survive the tree first.
        //
        // The node cap catches this one before the depth pass even runs, which
        // is the right order: the cheapest check that can refuse it, first.
        let err = check(&chain(100_000), Limits::default()).expect_err("a very deep tree");
        assert!(
            matches!(err, UiError::TooManyNodes { .. } | UiError::TooDeep { .. }),
            "{err}"
        );

        // And with the node cap lifted, so it is depth that has to catch it.
        let generous = Limits {
            nodes: 200_000,
            ..Limits::default()
        };
        let err = check(&chain(100_000), generous).expect_err("a very deep tree");
        assert!(matches!(err, UiError::TooDeep { .. }), "{err}");
    }

    #[test]
    fn a_tree_wider_than_the_limit_is_refused() {
        let limits = Limits::default();
        let wide = Build::of(
            container(),
            (0..limits.nodes + 8)
                .map(|_| Build::leaf(Widget::Spacer))
                .collect(),
        )
        .flatten();
        let err = check(&wide, limits).expect_err("too many nodes");
        assert!(matches!(err, UiError::TooManyNodes { .. }), "{err}");
    }

    #[test]
    fn a_tree_whose_child_ranges_are_nonsense_is_refused() {
        // **The attacks the flat form makes possible, and the invariant that
        // answers them.** None of these can be built by `Build`; all of them
        // can arrive off a wire.
        let limits = Limits::default();

        // Nothing at all. There is no root to lay out.
        let err = check(&Tree { nodes: Vec::new() }, limits).expect_err("empty");
        assert!(matches!(err, UiError::Malformed { .. }), "{err}");

        // A range off the end of the list.
        let mut tree = Build::of(container(), vec![Build::leaf(Widget::Spacer)]).flatten();
        tree.nodes[0].children = Children { first: 5, count: 9 };
        assert!(matches!(
            check(&tree, limits).expect_err("out of range"),
            UiError::Malformed { .. }
        ));

        // A range that overflows when added up.
        let mut tree = Build::of(container(), vec![Build::leaf(Widget::Spacer)]).flatten();
        tree.nodes[0].children = Children {
            first: u32::MAX,
            count: u32::MAX,
        };
        assert!(matches!(
            check(&tree, limits).expect_err("overflowing range"),
            UiError::Malformed { .. }
        ));

        // **A cycle.** A node claiming itself, which the index-ordering rule
        // makes unrepresentable — and this is the test that says so.
        let mut tree = Build::of(container(), vec![Build::leaf(Widget::Spacer)]).flatten();
        tree.nodes[0].children = Children { first: 0, count: 1 };
        assert!(matches!(
            check(&tree, limits).expect_err("a self-parenting node"),
            UiError::Malformed { .. }
        ));

        // A node claimed by two parents. Not a cycle, but not a tree either.
        let tree = Build::of(
            container(),
            vec![
                Build::of(container(), vec![Build::leaf(Widget::Spacer)]),
                Build::of(container(), vec![Build::leaf(Widget::Spacer)]),
            ],
        )
        .flatten();
        let mut shared = tree.clone();
        let last = shared.nodes.len() - 1;
        shared.nodes[1].children = Children {
            first: u32::try_from(last).expect("fits"),
            count: 1,
        };
        shared.nodes[2].children = Children {
            first: u32::try_from(last).expect("fits"),
            count: 1,
        };
        assert!(matches!(
            check(&shared, limits).expect_err("a shared child"),
            UiError::Malformed { .. }
        ));
    }

    #[test]
    fn oversized_text_is_refused_and_names_the_field() {
        let limits = Limits::default();
        let long = "x".repeat(limits.text_bytes + 1);
        let cases: Vec<(&str, Tree)> = vec![
            ("label", one(Widget::Label { text: long.clone() })),
            ("button", one(Widget::Button { text: long.clone() })),
            (
                "placeholder",
                one(Widget::TextInput {
                    initial: String::new(),
                    placeholder: long.clone(),
                }),
            ),
            (
                "name",
                Build::of(
                    Node {
                        name: long.clone(),
                        ..Node::new(Widget::Spacer)
                    },
                    Vec::new(),
                )
                .flatten(),
            ),
        ];
        for (field, tree) in cases {
            let err = check(&tree, limits).expect_err("oversized text was accepted");
            let UiError::TextTooLong { field: named, .. } = err else {
                panic!("refused for the wrong reason: {err}");
            };
            assert_eq!(named, field, "the error named the wrong field");
        }
    }

    #[test]
    fn a_widget_whose_own_fields_disagree_is_refused() {
        let limits = Limits::default();
        // Each of these is a mod's mistake that would otherwise reach a
        // renderer and become a panic, a divide by zero, or a silent wrong
        // answer. The message names the numbers, because "invalid dialog"
        // tells a mod author nothing.
        let bad = [
            Widget::Slider {
                min: 10,
                max: 0,
                value: 5,
            },
            Widget::Slider {
                min: 0,
                max: 10,
                value: 50,
            },
            Widget::Dropdown {
                options: vec!["a".to_owned()],
                selected: 7,
            },
            Widget::ItemGrid {
                view: "player:main".to_owned(),
                columns: 0,
                first: 0,
                count: 9,
            },
            Widget::Progress { permille: 5000 },
        ];
        for widget in bad {
            let err = check(&one(widget.clone()), limits).expect_err("a malformed widget");
            assert!(
                matches!(err, UiError::Malformed { .. }),
                "{widget:?} refused for the wrong reason: {err}"
            );
        }

        // An empty dropdown is NOT malformed, however odd: `selected` has
        // nothing to be out of range of, and a mod building a list from an
        // empty table should get an empty list rather than an error.
        assert!(
            check(
                &one(Widget::Dropdown {
                    options: Vec::new(),
                    selected: 0,
                }),
                limits
            )
            .is_ok()
        );
    }

    #[test]
    fn an_enormous_grid_or_dropdown_is_refused() {
        let limits = Limits::default();
        let grid = one(Widget::ItemGrid {
            view: "player:main".to_owned(),
            columns: 9,
            first: 0,
            count: u16::try_from(limits.grid_slots + 1).expect("fits"),
        });
        assert!(matches!(
            check(&grid, limits).expect_err("oversized grid"),
            UiError::TooManyEntries { .. }
        ));

        let dropdown = one(Widget::Dropdown {
            options: (0..=limits.options).map(|i| i.to_string()).collect(),
            selected: 0,
        });
        assert!(matches!(
            check(&dropdown, limits).expect_err("oversized dropdown"),
            UiError::TooManyEntries { .. }
        ));
    }

    #[test]
    fn an_ordinary_dialog_passes() {
        // The check must not be so strict that a real dialog trips it, which is
        // the way a limit fails.
        let tree = Build::of(
            container(),
            vec![
                Build::leaf(Widget::Label {
                    text: "Chest".to_owned(),
                }),
                Build::leaf(Widget::ItemGrid {
                    view: "container:chest".to_owned(),
                    columns: 9,
                    first: 0,
                    count: 27,
                }),
                Build::leaf(Widget::ItemGrid {
                    view: "player:main".to_owned(),
                    columns: 9,
                    first: 0,
                    count: 36,
                }),
                Build::of(
                    Node {
                        name: "close".to_owned(),
                        ..Node::new(Widget::Button {
                            text: "Close".to_owned(),
                        })
                    },
                    Vec::new(),
                ),
            ],
        )
        .flatten();
        assert!(check(&tree, Limits::default()).is_ok());
    }
}
