// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Regenerates the `dialog_schema` fuzz corpus seeds.
//!
//! Run: `cargo run --release -p tiamot-core --example dialog_seeds -- fuzz/corpus/dialog_schema`
//!
//! # Why seeds, when random bytes already decode
//!
//! postcard is compact and has no magic number, so a fuzzer reaches small valid
//! trees on its own — which is more than the Ogg target could say. What it does
//! NOT reach on its own is the interesting middle: a tree big enough to have
//! structure, nested enough to have depth, and valid enough to get past `check`
//! and into layout, where the arithmetic is.
//!
//! These are that middle. Every one is a shape a real mod would write, so the
//! mutations around them are shapes a real mod would nearly write.

use std::path::PathBuf;

use tiamot_core::ui::{Align, Build, Direction, Node, Style, Tree, Widget};

fn main() {
    let out = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from("fuzz/corpus/dialog_schema"), PathBuf::from);
    if let Err(err) = std::fs::create_dir_all(&out) {
        eprintln!("could not create `{}`: {err}", out.display());
        return;
    }

    for (name, tree) in seeds() {
        let bytes = match tiamot_core::proto::encode(&tree) {
            Ok(bytes) => bytes,
            Err(err) => {
                eprintln!("could not encode `{name}`: {err}");
                continue;
            }
        };
        let path = out.join(format!("seed-{name}.bin"));
        match std::fs::write(&path, &bytes) {
            Ok(()) => println!("wrote {} ({} bytes)", path.display(), bytes.len()),
            Err(err) => eprintln!("could not write `{}`: {err}", path.display()),
        }
    }
}

fn column(children: Vec<Build>) -> Build {
    Build::of(
        Node::new(Widget::Container {
            direction: Direction::Column,
            gap: 4,
            padding: 8,
            align: Align::Stretch,
        }),
        children,
    )
}

fn row(children: Vec<Build>) -> Build {
    Build::of(
        Node::new(Widget::Container {
            direction: Direction::Row,
            gap: 4,
            padding: 0,
            align: Align::Center,
        }),
        children,
    )
}

fn named(name: &str, widget: Widget) -> Build {
    Build::of(
        Node {
            name: name.to_owned(),
            ..Node::new(widget)
        },
        Vec::new(),
    )
}

/// The shapes worth mutating around.
fn seeds() -> Vec<(&'static str, Tree)> {
    vec![
        // The smallest thing there is.
        ("empty", Tree { nodes: Vec::new() }),
        (
            "one-label",
            Build::leaf(Widget::Label {
                text: "Hello".to_owned(),
            })
            .flatten(),
        ),
        // A container dialog: the case the item widgets exist for.
        (
            "chest",
            column(vec![
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
                row(vec![
                    Build::leaf(Widget::Spacer),
                    named(
                        "close",
                        Widget::Button {
                            text: "Close".to_owned(),
                        },
                    ),
                ]),
            ])
            .flatten(),
        ),
        // Every widget variant at least once, so a mutation can reach any of
        // them by changing bytes rather than by inventing a tag.
        (
            "every-widget",
            column(vec![
                Build::leaf(Widget::Label {
                    text: "All".to_owned(),
                }),
                named(
                    "go",
                    Widget::Button {
                        text: "Go".to_owned(),
                    },
                ),
                Build::leaf(Widget::Image { hash: [7; 32] }),
                named(
                    "who",
                    Widget::TextInput {
                        initial: String::new(),
                        placeholder: "name".to_owned(),
                    },
                ),
                named(
                    "on",
                    Widget::Checkbox {
                        text: "Enabled".to_owned(),
                        checked: true,
                    },
                ),
                named(
                    "amount",
                    Widget::Slider {
                        min: 0,
                        max: 100,
                        value: 50,
                    },
                ),
                named(
                    "mode",
                    Widget::Dropdown {
                        options: vec!["one".to_owned(), "two".to_owned()],
                        selected: 1,
                    },
                ),
                named(
                    "slot",
                    Widget::ItemSlot {
                        view: "player:hotbar".to_owned(),
                        index: 0,
                    },
                ),
                Build::of(
                    Node::new(Widget::Scroll),
                    vec![Build::leaf(Widget::Label {
                        text: "scrolled".to_owned(),
                    })],
                ),
                Build::leaf(Widget::Spacer),
                Build::leaf(Widget::Progress { permille: 250 }),
            ])
            .flatten(),
        ),
        // Styled, so the optional fields are present rather than always `None`
        // — a `None` costs one byte and a mutation rarely turns it into a whole
        // colour by accident.
        (
            "styled",
            Build::of(
                Node {
                    style: Style {
                        background: Some([20, 20, 24, 220]),
                        border: Some([90, 90, 100, 255]),
                        nine_slice: Some([3; 32]),
                        text_colour: Some([240, 240, 240, 255]),
                        text_size: Some(16),
                    },
                    grow: 1,
                    size: Some(320),
                    cross_size: Some(200),
                    ..Node::new(Widget::Container {
                        direction: Direction::Column,
                        gap: 6,
                        padding: 10,
                        align: Align::Center,
                    })
                },
                vec![Build::leaf(Widget::Label {
                    text: "Styled".to_owned(),
                })],
            )
            .flatten(),
        ),
        // Deep but legal: right at the default depth limit, so a mutation that
        // adds one level crosses it.
        ("deep", deep(32)),
    ]
}

/// A legal chain `depth` nodes long.
fn deep(depth: usize) -> Tree {
    let mut build = Build::leaf(Widget::Label {
        text: "bottom".to_owned(),
    });
    for _ in 1..depth {
        build = column(vec![build]);
    }
    build.flatten()
}
