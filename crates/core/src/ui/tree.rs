// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The widget tree a server mod describes.

use serde::{Deserialize, Serialize};

/// Which way a container stacks its children.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    /// Left to right.
    Row,
    /// Top to bottom.
    Column,
}

/// How children sit on the container's cross axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Align {
    /// At the start: the top of a row, the left of a column.
    Start,
    /// Centred.
    Center,
    /// At the end.
    End,
    /// Filling the cross axis.
    Stretch,
}

/// A colour, as straight (non-premultiplied) RGBA bytes.
///
/// Bytes rather than floats: a colour is not simulation state, and four `u8`s
/// cannot carry a `NaN` into a renderer.
pub type Colour = [u8; 4];

/// The whole of what a mod may say about how a widget looks.
///
/// Deliberately small — see the module docs. Every field is optional, and
/// `None` means "whatever the client's theme does", so a mod that styles
/// nothing gets a dialog that matches the rest of the game rather than a
/// transparent rectangle.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Style {
    /// Fill behind the widget.
    pub background: Option<Colour>,
    /// Border colour. Width is the client's, so a mod cannot draw a 400-pixel
    /// frame over the whole window.
    pub border: Option<Colour>,
    /// A nine-slice image, by content hash, stretched around the widget.
    pub nine_slice: Option<crate::proto::ContentHash>,
    /// Text colour.
    pub text_colour: Option<Colour>,
    /// Text size in virtual pixels, clamped by the client to something legible.
    pub text_size: Option<u16>,
}

/// One widget, and what it holds.
///
/// # Why this is an enum rather than a bag of optional fields
///
/// A `Node` with `is_button: bool` and twelve maybe-fields makes every invalid
/// combination representable and pushes the checking to runtime. An enum makes
/// a slider that is also a text input unspeakable, which is the same reason the
/// protocol's messages are an enum.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum Widget {
    /// A box holding other widgets, stacked along [`Direction`].
    ///
    /// Its children are the node's own [`Node::children`] range, not a field
    /// here — see [`Tree`] for why nothing in this module nests.
    Container {
        /// Which way children stack.
        direction: Direction,
        /// Space between children, in virtual pixels.
        gap: u16,
        /// Space inside the container's own edges.
        padding: u16,
        /// How children sit on the cross axis.
        align: Align,
    },
    /// Static text.
    Label {
        /// What it says.
        text: String,
    },
    /// A button. Pressing it sends an event to the mod that owns the dialog.
    Button {
        /// What it says.
        text: String,
    },
    /// An image, by content hash, through the same pipeline as a block texture.
    Image {
        /// The file's hash, as sent in the material or content table.
        hash: crate::proto::ContentHash,
    },
    /// A single-line text field. Submitting sends its contents.
    TextInput {
        /// What is in it to begin with.
        initial: String,
        /// Shown when it is empty.
        placeholder: String,
    },
    /// An on/off box.
    Checkbox {
        /// Its label.
        text: String,
        /// Whether it starts ticked.
        checked: bool,
    },
    /// A value between two bounds.
    ///
    /// Integers, not floats: a slider is a mod's input and comes back across
    /// the wire, and an integer cannot arrive as a `NaN`.
    Slider {
        /// Lowest value.
        min: i32,
        /// Highest value.
        max: i32,
        /// Where it starts.
        value: i32,
    },
    /// A list of choices, one selected.
    Dropdown {
        /// The choices, in order.
        options: Vec<String>,
        /// Which one is selected, as an index into `options`.
        selected: u16,
    },
    /// One inventory slot, bound to a view.
    ItemSlot {
        /// The inventory view this slot belongs to, e.g. `"player:main"`.
        view: String,
        /// Which slot in that view.
        index: u16,
    },
    /// A rectangle of inventory slots from one view.
    ItemGrid {
        /// The inventory view, e.g. `"player:main"`.
        view: String,
        /// Slots per row.
        columns: u16,
        /// The first slot shown.
        first: u16,
        /// How many slots are shown.
        count: u16,
    },
    /// A clipped region its children can be scrolled inside.
    Scroll,
    /// Empty space. `grow` on the node is what makes it push things apart.
    Spacer,
    /// A filled bar, for health, progress, or anything measured.
    Progress {
        /// How full, in thousandths. Integer for the same reason a slider is.
        permille: u16,
    },
    /// A block a player can chisel, and the shape they chiselled it to.
    ///
    /// **Appended at the end** (protocol v25). These are position-encoded on
    /// the wire, so a variant filed tidily beside `ItemGrid` would renumber
    /// everything below it — the one change this format does not survive.
    ///
    /// # Why the engine draws this and a mod does not
    ///
    /// Everything else here is a rectangle. This is twenty-seven cells drawn in
    /// perspective with a picking rule, and a mod cannot express that with
    /// labels and buttons — nor should it have to, since the cells it is
    /// editing are the same [`crate::inventory::Shape`] mask the engine places
    /// blocks from. The MEANING is still the mod's: what a shape costs, what it
    /// is worth, whether there is a bench you have to stand at.
    ///
    /// The gesture matches the world's, because a player already knows it:
    /// left-click removes the nearest cell along the line of sight, right-click
    /// puts one back in front of it. Chiselling only ever removes from the
    /// outside, so nothing is ever hidden from the tool that would reach it.
    ///
    /// Reports [`crate::proto::DialogEvent::Chiselled`] with the whole mask
    /// rather than which cell moved: the client has to keep the mask anyway to
    /// stay responsive, and a mod handed the whole thing can never rebuild a
    /// different one from events that arrived out of order.
    ShapeEditor {
        /// Which cells are filled, as the 27-bit mask indexed `x + 3*y + 9*z`.
        ///
        /// Not a [`crate::inventory::Shape`], which refuses both the empty mask
        /// and the full one: a full block is where chiselling STARTS and an
        /// empty one is where it can end up. Turning a mask into a shape is the
        /// mod's step, and the point at which "you have chiselled it away to
        /// nothing" is something a player can be told.
        shape: u32,
        /// Which material the filled cells are drawn as.
        material: u16,
    },
}

/// Where a node's children live in the [`Tree`]'s flat list.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Children {
    /// Index of the first child.
    pub first: u32,
    /// How many there are.
    pub count: u32,
}

/// A widget, plus how it behaves in its parent's layout.
///
/// The split matters: [`Widget`] is WHAT this is, and everything here is how it
/// sits. A mod changing a label to a button should not have to restate its
/// sizing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Node {
    /// The widget itself.
    pub widget: Widget,
    /// The name events from this widget carry, so a mod can tell two buttons
    /// apart. Empty for widgets a mod never expects to hear from.
    pub name: String,
    /// How it looks.
    pub style: Style,
    /// Share of the parent's leftover space along its direction. `0` takes only
    /// what the widget needs.
    pub grow: u16,
    /// Fixed size along the parent's direction, in virtual pixels. `None` asks
    /// the widget what it wants.
    pub size: Option<u16>,
    /// Fixed size across the parent's direction.
    pub cross_size: Option<u16>,
    /// This node's children, as a range into the tree's flat list.
    pub children: Children,
}

impl Node {
    /// A childless node wrapping a widget, with everything else defaulted.
    #[must_use]
    pub fn new(widget: Widget) -> Self {
        Self {
            widget,
            name: String::new(),
            style: Style::default(),
            grow: 0,
            size: None,
            cross_size: None,
            children: Children::default(),
        }
    }
}

/// A whole widget tree, flat.
///
/// # Why this is a `Vec` and not a nest of boxes
///
/// The obvious shape for a widget tree is recursive — a container owning its
/// children — and it is the wrong one for a value that arrives from a server
/// nobody trusts. A recursive type recurses in three places a hostile tree can
/// reach:
///
/// - **decoding**, where serde nests one call per level and a megabyte of
///   nesting overflows the stack before any limit of ours is consulted;
/// - **dropping**, which recurses even if the tree was built safely, so merely
///   letting go of it aborts the process;
/// - every walk, including the one meant to enforce a depth limit — which
///   cannot enforce it, because it has to survive the tree first.
///
/// A flat list has none of those. Decoding is a loop over `Vec`, dropping is a
/// `Vec` drop, and depth is something computed rather than something the parser
/// can be made to perform.
///
/// # The invariant that makes it safe
///
/// **A child's index is always greater than its parent's.** That single rule
/// makes a cycle unrepresentable — so a walk always terminates — and it is
/// checkable in one linear pass with no bookkeeping. [`super::check`] enforces
/// it, and every walk in this module relies on it having been enforced.
///
/// Node `0` is the root. An empty tree has no root and is refused.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Tree {
    /// Every node. Index `0` is the root; children always sit after parents.
    pub nodes: Vec<Node>,
}

impl Tree {
    /// A tree of one childless node.
    #[must_use]
    pub fn leaf(widget: Widget) -> Self {
        Self {
            nodes: vec![Node::new(widget)],
        }
    }

    /// The root node, or `None` for an empty tree.
    #[must_use]
    pub fn root(&self) -> Option<&Node> {
        self.nodes.first()
    }

    /// The children of the node at `index`, as indices.
    ///
    /// Empty for a node whose range falls outside the list, so a walk over a
    /// tree that has NOT been checked still terminates rather than panicking.
    /// Correctness is [`super::check`]'s job; not panicking is this one's.
    #[must_use]
    pub fn children_of(&self, index: usize) -> std::ops::Range<usize> {
        let Some(node) = self.nodes.get(index) else {
            return 0..0;
        };
        let first = node.children.first as usize;
        let end = first.saturating_add(node.children.count as usize);
        if first >= self.nodes.len() || end > self.nodes.len() {
            return 0..0;
        }
        first..end
    }
}

/// A nested widget, for building a [`Tree`] by hand.
///
/// The wire form is flat because a server's tree is hostile input. A mod's Lua
/// table and a test's fixture are neither, and both read far better nested, so
/// this is the shape they are written in and [`Build::flatten`] is the one
/// place that converts.
///
/// Flattening is breadth-first, which puts every child at a higher index than
/// its parent — [`Tree`]'s invariant, established by construction rather than
/// hoped for and checked later.
///
/// **Not for decoding.** This nests, so it drops recursively; it is for trees
/// this process built, never for one that arrived from somewhere.
#[derive(Debug, Clone)]
pub struct Build {
    /// The widget and its layout.
    pub node: Node,
    /// What sits inside it.
    pub children: Vec<Build>,
}

impl Build {
    /// A childless node.
    #[must_use]
    pub fn leaf(widget: Widget) -> Self {
        Self {
            node: Node::new(widget),
            children: Vec::new(),
        }
    }

    /// A node with children.
    #[must_use]
    pub fn of(node: Node, children: Vec<Self>) -> Self {
        Self { node, children }
    }

    /// Replaces the node's layout, keeping the children.
    #[must_use]
    pub fn with(mut self, node: Node) -> Self {
        self.node = Node {
            widget: self.node.widget,
            ..node
        };
        self
    }

    /// Flattens into the wire form, breadth-first.
    #[must_use]
    pub fn flatten(self) -> Tree {
        let mut nodes: Vec<Node> = Vec::new();
        // (the build, the index its node will occupy)
        let mut queue: std::collections::VecDeque<Self> = std::collections::VecDeque::new();
        nodes.push(self.node.clone());
        queue.push_back(self);
        let mut at = 0usize;
        while let Some(build) = queue.pop_front() {
            let first = u32::try_from(nodes.len()).unwrap_or(u32::MAX);
            let count = u32::try_from(build.children.len()).unwrap_or(0);
            nodes[at].children = Children { first, count };
            for child in build.children {
                nodes.push(child.node.clone());
                queue.push_back(child);
            }
            at += 1;
        }
        Tree { nodes }
    }
}
