// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Rendering a server's widget tree, and reporting what the player did to it.
//!
//! # The schema is the contract; egui is an implementation detail
//!
//! `core::ui` holds no egui type and charter rule 3 makes that structural. This
//! module is the one place the two meet, and it is deliberately thin: **the
//! layout is not egui's.** `core::ui::layout` computes every rectangle, and
//! egui is used for painting and for hit-testing, not for deciding where
//! anything goes.
//!
//! That split is what makes the layout testable headlessly, and what would let
//! egui be replaced without touching the schema, the protocol, or any mod.
//!
//! # State a declarative tree cannot hold
//!
//! A server describes what a dialog IS. It does not describe what a player has
//! half-typed into a text field, or which dropdown is open. That state belongs
//! to the client and lives here, keyed by form and widget name, and is dropped
//! when the dialog closes.
//!
//! The server stays authoritative over everything that matters: a text field's
//! contents are the player's until they submit, and a slot move is a REQUEST
//! (see [`tiamot_core::proto::DialogEvent`]).

use std::collections::BTreeMap;

use crate::icons::Icons;

use tiamot_core::proto::{Click, DialogEvent};
use tiamot_core::ui::{Laid, Measure, Node, Rect, Style, Tree, Widget, layout};

/// What the player has done to a dialog that the server has not been told yet.
#[derive(Debug, Clone, PartialEq)]
pub struct Raised {
    /// Which dialog.
    pub form: String,
    /// What happened.
    pub event: DialogEvent,
}

/// What one inventory view holds, as the server last said.
///
/// The client draws this and never edits it. A click sends a request; the slots
/// change when a `ViewUpdate` says they did. That is the whole of why an
/// inventory cannot be desynced by a client that lies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ViewContents {
    /// What each slot holds, or `None` where it is empty.
    pub slots: Vec<Option<tiamot_core::proto::StackDef>>,
    /// What is on the cursor.
    pub held: Option<tiamot_core::proto::StackDef>,
}

/// Per-widget state the tree itself cannot carry.
#[derive(Debug, Default)]
struct Local {
    /// How far each scroll box has been scrolled, in points, by node index.
    ///
    /// **By index rather than by name**, unlike everything else here: a scroll
    /// box is a container and containers are rarely named, so keying on the
    /// name would give every unnamed one in a dialog the same offset. The index
    /// is stable for as long as the tree is, and a tree that changes shape is a
    /// tree whose scroll position was about to be wrong anyway.
    scroll: BTreeMap<usize, f32>,
    /// What is in each text input, by widget name.
    text: BTreeMap<String, String>,
    /// Where each slider sits, by widget name.
    ///
    /// Held locally while it is dragged so the bar follows the mouse; the
    /// server hears the value when the drag ends.
    slider: BTreeMap<String, i32>,
    /// Which option each dropdown shows.
    dropdown: BTreeMap<String, u16>,
    /// Whether each checkbox is ticked.
    checked: BTreeMap<String, bool>,
    /// What each shape editor has been chiselled to, by widget name.
    ///
    /// Held locally for the same reason a dragged slider is: a chisel that
    /// waited for the server to agree it happened would land a tick late, and
    /// carving is a run of clicks rather than one. The mod is told after every
    /// change and its next tree wins — see [`Local::adopted`].
    shape: BTreeMap<String, u32>,
    /// Which shape the server last SAID each editor holds.
    ///
    /// Without this the local mask would never let go: a mod that reset an
    /// editor, or opened it on a different block, would send a tree the client
    /// quietly ignored because it already had an opinion. Comparing against
    /// what the server said last is how "the mod changed it" is told apart
    /// from "the mod is repeating itself".
    adopted: BTreeMap<String, u32>,
    /// Which way round each shape editor's cube is being looked at.
    ///
    /// **A view and not a value.** The cut is always held in authored
    /// coordinates, so turning the cube changes what a player is looking at
    /// and never what they are making — and the mod is never told, because
    /// there is nothing here it could act on.
    turn: BTreeMap<String, crate::shape_view::Turn>,
}

impl Local {
    /// Applies the wheel to a scroll box and returns how far it is scrolled.
    ///
    /// # Why the offset is clamped every frame rather than only when it moves
    ///
    /// The content changes under it. A mod redrawing a list one item shorter
    /// leaves a box scrolled past its own end, and the symptom is a panel that
    /// looks empty until the player scrolls back up — which reads as the mod
    /// having lost its contents. Clamping on read means the offset can only
    /// ever be somewhere the content is.
    ///
    /// Only when the pointer is inside the box, so a dialog with two lists does
    /// not scroll both, and so the wheel still reaches the world when nothing
    /// is under it.
    fn scroll_by(&mut self, ui: &egui::Ui, index: usize, rect: egui::Rect, content: f32) -> f32 {
        let room = (content - rect.height()).max(0.0);
        let offset = self.scroll.entry(index).or_insert(0.0);
        let hovered = ui
            .ctx()
            .pointer_latest_pos()
            .is_some_and(|at| rect.contains(at));
        if hovered && room > 0.0 {
            *offset -= ui.ctx().input(|input| input.smooth_scroll_delta.y);
        }
        *offset = offset.clamp(0.0, room);
        *offset
    }
}

/// Every open dialog's local state.
#[derive(Debug, Default)]
pub struct Dialogs {
    forms: BTreeMap<String, Local>,
}

/// Measures leaves with egui's real font metrics.
///
/// The other half of `core::ui`'s [`Measure`] seam: the arithmetic is in
/// `core`, and this is the font `core` is not allowed to have.
struct EguiRuler<'a> {
    ctx: &'a egui::Context,
}

impl EguiRuler<'_> {
    /// The size a run of text wants, at a widget's style.
    ///
    /// `ceil` is on charter rule 4's disallowed list and is used deliberately:
    /// rule 4's Scope paragraph exempts UI layout in as many words, and a
    /// glyph's width is not simulation state. The same exemption
    /// `audio::mixer::amplitude_to_db` takes, for the same reason.
    #[expect(
        clippy::disallowed_methods,
        reason = "UI layout is presentation; float-determinism.md Scope"
    )]
    fn text(&self, text: &str, style: &Style) -> (i32, i32) {
        let size = f32::from(style.text_size.unwrap_or(14)).clamp(8.0, 48.0);
        let galley = self.ctx.fonts_mut(|fonts| {
            fonts.layout_no_wrap(
                text.to_owned(),
                egui::FontId::proportional(size),
                egui::Color32::WHITE,
            )
        });
        (galley.size().x.ceil() as i32, galley.size().y.ceil() as i32)
    }
}

impl Measure for EguiRuler<'_> {
    fn natural(&self, widget: &Widget, style: &Style) -> (i32, i32) {
        match widget {
            Widget::Label { text } => self.text(text, style),
            // Buttons and checkboxes carry padding around their text.
            Widget::Button { text } => {
                let (w, h) = self.text(text, style);
                (w + 16, h + 8)
            }
            Widget::Checkbox { text, .. } => {
                let (w, h) = self.text(text, style);
                (w + 24, h.max(16))
            }
            Widget::TextInput { placeholder, .. } => {
                let (_, h) = self.text(placeholder, style);
                (160, h + 8)
            }
            Widget::Slider { .. } => (160, 20),
            Widget::Dropdown { options, selected } => {
                let shown = options
                    .get(usize::from(*selected))
                    .map_or("", String::as_str);
                let (w, h) = self.text(shown, style);
                (w + 32, h + 8)
            }
            Widget::Image { .. } => (64, 64),
            Widget::ItemSlot { .. } => (SLOT, SLOT),
            Widget::ItemGrid { columns, count, .. } => {
                let columns = i32::from((*columns).max(1));
                let count = i32::from(*count);
                let rows = count.div_euclid(columns) + i32::from(count.rem_euclid(columns) != 0);
                (columns * SLOT, rows.max(1) * SLOT)
            }
            Widget::Progress { .. } => (120, 12),
            // Square, and large: this is the widget a player carves in, and
            // the cells are twenty-seven ninths of it.
            Widget::ShapeEditor { .. } => (192, 192),
            // Containers measure from their children in `core::ui`, and a
            // spacer wants nothing — all three are "no intrinsic size".
            Widget::Spacer | Widget::Container { .. } | Widget::Scroll => (0, 0),
        }
    }
}

/// The font a count sits in, for a slot of a given size.
///
/// **Proportional to the slot, not a constant.** It was a flat 11 points
/// everywhere, which is legible on the 36-point slot the engine draws and
/// unreadable on the big one a mod asks for — the count stayed the same size
/// while the box around it grew, so a large inventory looked like it had lost
/// its numbers. Reported by a mod author building a bigger interface.
///
/// Just under a third of the slot, and never below the 11 that was there
/// before: shrinking a count out of legibility on a small slot would trade one
/// complaint for the other.
fn count_font(slot: f32) -> egui::FontId {
    egui::FontId::proportional((slot * 0.3).max(11.0))
}

/// One inventory slot's size in virtual pixels, borders included.
const SLOT: i32 = 36;

impl Dialogs {
    /// Forgets state for dialogs that are no longer open.
    ///
    /// A player who closes a shop and opens it again gets an empty text field,
    /// which is what they expect — and it stops a mod's dialog accumulating
    /// state for a session's worth of forms it never uses again.
    pub fn retain_open(&mut self, open: &BTreeMap<String, Screen>) {
        self.forms.retain(|form, _| open.contains_key(form));
    }

    /// Draws every open dialog and returns what the player did.
    pub fn draw(
        &mut self,
        ctx: &egui::Context,
        open: &BTreeMap<String, Screen>,
        views: &BTreeMap<String, ViewContents>,
        icons: Icons<'_>,
        art: &BTreeMap<String, crate::pictures::Resolved>,
        area: (f32, f32),
    ) -> Vec<Raised> {
        self.retain_open(open);
        let mut raised = Vec::new();
        for (form, screen) in open {
            let local = self.forms.entry(form.clone()).or_default();
            // Uploaded before the walk, so the walk stays immutable — see
            // `Pictures::resolve` and `App::dialog_art`. A form with no art
            // resolves to nothing and every lookup in it misses, which is what
            // every dialog written before pictures existed does.
            let empty = crate::pictures::Resolved::default();
            let form_art = art.get(form).unwrap_or(&empty);
            raised.extend(draw_form(
                ctx, form, screen, local, views, icons, form_art, area,
            ));
        }
        // **Last, and over everything.** What is on the cursor is drawn after
        // every screen, because it is above them by definition — a stack in
        // your hand passes over the slots you are choosing between.
        if !open.is_empty() {
            paint_cursor_stack(ctx, views, icons);
        }
        raised
    }
}

/// Draws the stack on the player's cursor, under the pointer.
///
/// # Why this was missing and what it looked like
///
/// Reported from the window: clicking a slot made the stack **vanish**. It was
/// never lost — `Slots::grab` holds it on the SERVER, which is the right model
/// (a move is two half-gestures, and a client that owned the middle of one
/// could invent items by lying about what it took) — and `ViewUpdate::held`
/// has always carried it to the client, and `ViewContents::held` has always
/// stored it.
///
/// Nothing drew it. The whole path existed and the last step was never written,
/// so a click emptied a slot and put the stack somewhere invisible.
///
/// # Where it comes from
///
/// Any view will do: the cursor is one stack for the whole player, and every
/// `ViewUpdate` carries the same answer. Taking the first that has one means a
/// screen over a mod's container shows it as readily as the inventory does.
fn paint_cursor_stack(
    ctx: &egui::Context,
    views: &BTreeMap<String, ViewContents>,
    icons: Icons<'_>,
) {
    let Some(stack) = views.values().find_map(|contents| contents.held.as_ref()) else {
        return;
    };
    let Some(at) = ctx.pointer_latest_pos() else {
        return;
    };

    // **A layer above every window**, or the stack would slide under the sheet
    // it is being dragged across.
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Tooltip,
        egui::Id::new("carried-stack"),
    ));
    // Centred on the pointer and a little smaller than a slot, so the cursor
    // stays visible past its edges rather than being buried by it.
    let side = (SLOT as f32) * 0.8;
    let box_ = egui::Rect::from_center_size(at, egui::vec2(side, side));
    icons.paint_stack(&painter, box_, stack.material, stack.shape);
    painter.text(
        box_.right_bottom(),
        egui::Align2::RIGHT_BOTTOM,
        stack_label(stack.units, stack.shape),
        count_font(side),
        egui::Color32::WHITE,
    );
}

/// One dialog a server has open on this screen.
///
/// The flag travels WITH the tree rather than being remembered per form: a
/// redraw carries it too, so a mod cannot change the shape of the window its
/// screen lives in halfway through — which is exactly what a remembered flag
/// eventually does.
#[derive(Debug, Clone, PartialEq)]
pub struct Screen {
    /// What to draw.
    pub tree: Tree,
    /// Whether the mod built a prompt rather than a screen.
    pub compact: bool,
}

impl Screen {
    /// One dialog, as the server described it.
    #[must_use]
    pub const fn new(tree: Tree, compact: bool) -> Self {
        Self { tree, compact }
    }
}

/// The rectangle a `compact` dialog is drawn in, in points.
///
/// # Why only a compact one is measured
///
/// **A player reads every screen the game puts in front of them as one thing.**
/// Sizing each one to its contents made the inventory a different size from the
/// crafting tab of the same dialog, so switching tabs grew and shrank the
/// window under the pointer, and no two screens agreed with each other.
/// Reported from the window. A screen is therefore the sheet — see
/// [`crate::panel::sheet_with`], which decides that and hands over a `Ui`
/// already inside it.
///
/// The engine cannot tell a two-button prompt from an inventory by looking at
/// the tree — both are containers of widgets — so the mod says which it built,
/// and a `compact` one is measured the way every dialog used to be. It is still
/// capped by the sheet: a mod that asks for a window bigger than the screen
/// does not get one.
#[must_use]
pub fn prompt_size(wanted: (i32, i32), area: (f32, f32)) -> (i32, i32) {
    let sheet = crate::panel::size(area);
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a window's size in points, which is small and positive"
    )]
    let cap = (sheet.0 as i32, sheet.1 as i32);
    (wanted.0.clamp(160, cap.0), wanted.1.clamp(120, cap.1))
}

/// What a slot says it holds.
///
/// **Two different questions, and they have two different answers.** Loose
/// material is charter rule 5's blocks and spare nodes, because a player thinks
/// in blocks and `1+13` is what forty units actually is. A CUT is counted:
/// thirteen units cut to a thirteen-cell shape is one stair, and labelling it
/// `+13` told a player they had thirteen of something. Reported from the
/// window. See [`tiamot_core::inventory::items`], which decides which it is.
#[must_use]
pub fn stack_label(units: u32, shape: u32) -> String {
    if let Some(count) = tiamot_core::inventory::items(units, shape) {
        return count.to_string();
    }
    let (blocks, nodes) = tiamot_core::inventory::display(units);
    if nodes == 0 {
        blocks.to_string()
    } else if blocks == 0 {
        format!("+{nodes}")
    } else {
        format!("{blocks}+{nodes}")
    }
}

/// Lays a tree out into `size` and paints it at the cursor.
///
/// Shared by both shapes of dialog, because what differs between a screen and a
/// prompt is the window around the tree and nothing about the tree itself.
#[expect(
    clippy::too_many_arguments,
    reason = "a paint walk carries its context; grouping it would hide the recursion"
)]
fn paint_tree(
    ui: &mut egui::Ui,
    size: (i32, i32),
    tree: &Tree,
    ruler: &EguiRuler<'_>,
    form: &str,
    local: &mut Local,
    views: &BTreeMap<String, ViewContents>,
    icons: Icons<'_>,
    art: &crate::pictures::Resolved,
    raised: &mut Vec<Raised>,
) {
    let origin = ui.cursor().min;
    let laid = layout(tree, Rect::new(0, 0, size.0, size.1), ruler);
    // The tree and its rectangles are walked TOGETHER, by index, so a renderer
    // cannot pair a widget with somebody else's rectangle — which a flat list
    // plus a separate traversal invites.
    paint(
        ui, origin, tree, 0, &laid, form, local, views, icons, art, raised,
    );
    ui.allocate_space(egui::vec2(laid.rect.w as f32, laid.rect.h as f32));
}

/// Draws one dialog in its own window.
#[expect(
    clippy::too_many_arguments,
    reason = "the same context a paint walk carries, one level up from it"
)]
fn draw_form(
    ctx: &egui::Context,
    form: &str,
    screen: &Screen,
    local: &mut Local,
    views: &BTreeMap<String, ViewContents>,
    icons: Icons<'_>,
    art: &crate::pictures::Resolved,
    area: (f32, f32),
) -> Vec<Raised> {
    let tree = &screen.tree;
    let mut raised = Vec::new();
    let ruler = EguiRuler { ctx };
    let mut close = false;

    if screen.compact {
        // A prompt: measured, capped by the sheet, and draggable. Centred is
        // only a DEFAULT, so a player who moves one keeps it where they put it.
        let (width, height) = prompt_size(tiamot_core::ui::natural(tree, &ruler), area);
        egui::Window::new(form)
            .collapsible(false)
            .resizable(false)
            .default_pos(egui::pos2(
                (area.0 - width as f32) / 2.0,
                (area.1 - height as f32) / 2.0,
            ))
            .default_width(width as f32)
            .show(ctx, |ui| {
                paint_tree(
                    ui,
                    (width, height),
                    tree,
                    &ruler,
                    form,
                    local,
                    views,
                    icons,
                    art,
                    &mut raised,
                );
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
    } else {
        // **A screen goes through the engine's own sheet**, which is the only
        // way it can be the same shape as the engine's own screens.
        //
        // It used to build its own window at the sheet's size and lay the tree
        // into the whole of it, then add a Close button underneath — and
        // `fixed_size` on an `egui::Window` is a REQUEST, so the window grew by
        // however much did not fit. Reported from the window: the inventory
        // still reached the top and the bottom of the screen. `panel::sheet`
        // hands over a `Ui` that is already inside the sheet, so a screen
        // cannot escape one because it never gets to say how big it is.
        //
        // No heading: a dialog's own title is inside its tree, where the mod
        // put it, and the `id` here is the namespaced form the server named.
        close |= crate::panel::sheet_with(ctx, form, None, Some("Close"), |ui| {
            // The room the sheet handed over, which inside its scrolling body
            // is the viewport rather than the endless height a scroll area can
            // hold. Measured rather than assumed: the clip rectangle, which is
            // the other candidate, is a few points WIDER than the room.
            let room = ui.available_size();
            #[expect(
                clippy::cast_possible_truncation,
                reason = "a window's size in points, which is small and positive"
            )]
            let size = (room.x.max(1.0) as i32, room.y.max(1.0) as i32);
            paint_tree(
                ui,
                size,
                tree,
                &ruler,
                form,
                local,
                views,
                icons,
                art,
                &mut raised,
            );
        });
    }

    if close {
        raised.push(Raised {
            form: form.to_owned(),
            event: DialogEvent::Closed,
        });
    }
    raised
}

/// Paints one node and everything under it.
#[expect(
    clippy::too_many_arguments,
    reason = "a paint walk carries its context; grouping it would hide the recursion"
)]
fn paint(
    ui: &mut egui::Ui,
    origin: egui::Pos2,
    tree: &Tree,
    index: usize,
    laid: &Laid,
    form: &str,
    local: &mut Local,
    views: &BTreeMap<String, ViewContents>,
    icons: Icons<'_>,
    art: &crate::pictures::Resolved,
    raised: &mut Vec<Raised>,
) {
    let Some(node) = tree.nodes.get(index) else {
        return;
    };
    let rect = egui::Rect::from_min_size(
        origin + egui::vec2(laid.rect.x as f32, laid.rect.y as f32),
        egui::vec2(laid.rect.w as f32, laid.rect.h as f32),
    );
    paint_background(ui, rect, &node.style, art);
    paint_widget(ui, rect, node, form, local, views, icons, art, raised);

    // **A scroll box clips its children and moves them under the clip.**
    // `core::ui` already lays them out at their full height inside it — "the
    // renderer clips it", says the layout — and this is the renderer finally
    // doing so. Before it, an oversized dialog drew its contents straight over
    // whatever was below and none of it could be reached.
    let scrolled = matches!(node.widget, Widget::Scroll).then(|| {
        let content = laid
            .children
            .iter()
            .map(|child| (child.rect.y + child.rect.h) as f32)
            .fold(0.0f32, f32::max)
            - laid.rect.y as f32;
        let offset = local.scroll_by(ui, index, rect, content);
        let saved = ui.clip_rect();
        ui.set_clip_rect(saved.intersect(rect));
        (saved, offset)
    });
    let child_origin = origin - egui::vec2(0.0, scrolled.map_or(0.0, |(_, offset)| offset));

    for (child, child_laid) in tree.children_of(index).zip(&laid.children) {
        paint(
            ui,
            child_origin,
            tree,
            child,
            child_laid,
            form,
            local,
            views,
            icons,
            art,
            raised,
        );
    }

    if let Some((saved, _)) = scrolled {
        ui.set_clip_rect(saved);
    }
}

/// The style tokens that apply to any widget.
fn paint_background(
    ui: &egui::Ui,
    rect: egui::Rect,
    style: &Style,
    art: &crate::pictures::Resolved,
) {
    // **The frame goes under the fill and the border.** A nine-slice IS the
    // background where a mod supplies one, and a mod that supplies both meant
    // the flat colour to sit inside the frame rather than over it.
    if let Some(hash) = style.nine_slice
        && let Some(picture) = art.get(&hash)
    {
        // **Corner size in source pixels, one for one.** The layout is already
        // in the same points egui draws in, so a frame drawn at 48 pixels
        // across has 16-point corners whatever the interface scale — which is
        // the behaviour a nine-slice exists for: the corners keep their size
        // and the edges take up the slack.
        crate::pictures::paint_nine_slice(
            ui.painter(),
            picture.texture,
            rect,
            1.0,
            (picture.width, picture.height),
        );
    }
    if let Some(fill) = style.background {
        ui.painter().rect_filled(
            rect,
            2.0,
            egui::Color32::from_rgba_unmultiplied(fill[0], fill[1], fill[2], fill[3]),
        );
    }
    if let Some(border) = style.border {
        ui.painter().rect_stroke(
            rect,
            2.0,
            egui::Stroke::new(
                1.0,
                egui::Color32::from_rgba_unmultiplied(border[0], border[1], border[2], border[3]),
            ),
            egui::StrokeKind::Inside,
        );
    }
}

/// What every widget painter needs and none of them owns.
struct Paint<'a> {
    /// Which dialog, for the events raised.
    form: &'a str,
    /// The atlas, for whatever draws a material.
    ///
    /// Carried here rather than as a sixth parameter through four painters:
    /// it is what a slot needs and nothing above a slot looks at it.
    icons: Icons<'a>,
    /// Text colour, resolved from the node's style.
    colour: egui::Color32,
    /// Text font, resolved from the node's style.
    font: egui::FontId,
    /// The mod's fill, if it named one.
    ///
    /// **`paint_background` is not enough on its own.** It paints
    /// `style.background` under every widget, and then any widget with a fill
    /// of its own — a button, a slot, a dropdown, a slider track — painted an
    /// unconditional grey straight over it. Reported from the window as slots
    /// and buttons painting grey over the colours a mod supplied.
    ///
    /// So a painter asks for its fill through [`Paint::fill`] rather than
    /// reaching for a constant, and the mod wins where it said something.
    fill: Option<egui::Color32>,
    /// The mod's border colour, if it named one. Same reasoning as `fill`.
    edge: Option<egui::Color32>,
}

impl Paint<'_> {
    /// The fill to use, preferring what the mod asked for.
    ///
    /// `default` is the shade this widget uses when nothing was said, which is
    /// most of the time — the engine still has a look of its own.
    fn fill(&self, default: u8) -> egui::Color32 {
        or_grey(self.fill, default)
    }

    /// The border to use, preferring what the mod asked for.
    fn edge(&self, default: u8) -> egui::Color32 {
        or_grey(self.edge, default)
    }

    /// Queues an event against this dialog.
    fn raise(&self, raised: &mut Vec<Raised>, event: DialogEvent) {
        raised.push(Raised {
            form: self.form.to_owned(),
            event,
        });
    }
}

/// The widget itself, and any event it raises.
///
/// A dispatcher: each interactive widget has its own painter, because they are
/// where the interaction rules live and one 200-line match hid them.
#[expect(
    clippy::too_many_arguments,
    reason = "the dispatcher carries what every widget painter might need"
)]
fn paint_widget(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    node: &Node,
    form: &str,
    local: &mut Local,
    views: &BTreeMap<String, ViewContents>,
    icons: Icons<'_>,
    art: &crate::pictures::Resolved,
    raised: &mut Vec<Raised>,
) {
    let paint = Paint {
        form,
        icons,
        colour: node.style.text_colour.map_or(egui::Color32::WHITE, |c| {
            egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3])
        }),
        font: egui::FontId::proportional(
            f32::from(node.style.text_size.unwrap_or(14)).clamp(8.0, 48.0),
        ),
        fill: node
            .style
            .background
            .map(|c| egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3])),
        edge: node
            .style
            .border
            .map(|c| egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3])),
    };

    match &node.widget {
        Widget::Label { text } => {
            ui.painter().text(
                rect.left_center(),
                egui::Align2::LEFT_CENTER,
                text,
                paint.font.clone(),
                paint.colour,
            );
        }
        Widget::Button { text } => paint_button(ui, rect, node, text, &paint, raised),
        Widget::Checkbox { text, checked } => {
            paint_checkbox(ui, rect, node, text, *checked, &paint, local, raised);
        }
        Widget::Slider { min, max, value } => {
            paint_slider(ui, rect, node, (*min, *max, *value), &paint, local, raised);
        }
        Widget::Dropdown { options, selected } => {
            paint_dropdown(ui, rect, node, options, *selected, &paint, local, raised);
        }
        Widget::TextInput {
            initial,
            placeholder,
        } => paint_text_input(ui, rect, node, initial, placeholder, &paint, local, raised),
        Widget::Progress { permille } => {
            ui.painter().rect_filled(rect, 2.0, paint.fill(40));
            let mut bar = rect;
            bar.set_width(rect.width() * f32::from(*permille) / 1000.0);
            ui.painter()
                .rect_filled(bar, 2.0, egui::Color32::from_rgb(90, 160, 90));
        }
        Widget::ItemSlot { view, index } => {
            paint_slot(ui, rect, view, *index, form, views, &paint, raised);
        }
        Widget::ItemGrid {
            view,
            columns,
            first,
            count,
        } => paint_grid(
            ui,
            rect,
            view,
            (*columns, *first, *count),
            form,
            views,
            &paint,
            raised,
        ),
        Widget::ShapeEditor { shape, material } => {
            paint_shape_editor(ui, rect, node, (*shape, *material), &paint, local, raised);
        }
        // **A picture, if its bytes have arrived.** Nothing until they do,
        // rather than a placeholder: art landing a few frames after the dialog
        // it belongs to is the ordinary case, and a magenta square flashing on
        // every panel open helps nobody. A picture that will NOT decode is
        // dropped with a warning where it is decoded — see `net::offer_picture`.
        Widget::Image { hash } => {
            if let Some(picture) = art.get(hash) {
                crate::pictures::paint(ui.painter(), picture.texture, rect);
            }
        }
        // Drawn by their children, or by nothing at all.
        Widget::Container { .. } | Widget::Scroll | Widget::Spacer => {}
    }
}

/// A block being chiselled, and the cell the player took off it.
///
/// # The two masks
///
/// What is DRAWN is the local mask, so a click lands under the cursor rather
/// than a tick later — carving is a run of clicks and each one waiting for a
/// round trip would feel like carving through treacle. What is AUTHORITATIVE
/// is still the mod's: every change is reported, and a tree carrying a shape
/// different from the last one the server sent replaces the local mask
/// outright. That is how a "reset" button, or opening the editor on another
/// block, gets through.
///
/// # The gesture
///
/// Left-click removes the nearest cell along the line of sight and right-click
/// puts one back against the face that was clicked, which is what digging and
/// placing already do in the world. Removal never needs to reach a cell it
/// cannot see, because taking the visible one reveals the next.
fn paint_shape_editor(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    node: &Node,
    state: (u32, u16),
    paint: &Paint,
    local: &mut Local,
    raised: &mut Vec<Raised>,
) {
    let (sent, material) = state;
    // The mod's word, when the mod has changed its mind.
    if local.adopted.get(&node.name) != Some(&sent) {
        local.adopted.insert(node.name.clone(), sent);
        local.shape.insert(node.name.clone(), sent);
    }
    let mut mask = local.shape.get(&node.name).copied().unwrap_or(sent);
    let mut turn = local.turn.get(&node.name).copied().unwrap_or(0);

    // Square, and centred: the projection fits a six-by-six box and stretching
    // it would put the cells' faces out of true with each other.
    let side = rect.width().min(rect.height());
    let area = egui::Rect::from_center_size(rect.center(), egui::vec2(side, side));
    let response = ui.allocate_rect(rect, egui::Sense::click());

    // Cells, not a stack: the editor's whole block is twenty-seven cells to
    // chisel at, where a whole block in a slot is loose material.
    let seen = crate::shape_view::as_seen(mask, turn);
    paint.icons.paint_cells(ui.painter(), area, material, seen);
    paint_face_labels(ui, area, seen, turn);

    let clicked = if response.clicked() {
        Some(false)
    } else if response.secondary_clicked() {
        Some(true)
    } else {
        None
    };

    // **Two real buttons, not a hand-rolled hit test.** Asked for from the
    // window: a cube you turn with left and right arrows. A drag was the first
    // answer and the wrong one — the gesture that turns the cube and the
    // gesture that chisels a cell were the same button, so a click with a
    // little movement in it did both.
    //
    // Placed AFTER the cube is allocated, so egui gives them the click: a
    // later widget wins an overlap. Testing the pointer against the arrows'
    // rectangles by hand instead looks equivalent and is not — the cube's own
    // response has already decided whether it is the thing being interacted
    // with.
    let (left, right) = turn_arrows(rect);
    let step = if turn_arrow(ui, left, true) {
        Some(3)
    } else if turn_arrow(ui, right, false) {
        Some(1)
    } else {
        None
    };
    if let Some(step) = step {
        turn = (turn + step) % 4;
        local.turn.insert(node.name.clone(), turn);
        return;
    }

    if let Some(adding) = clicked
        && let Some(at) = response.interact_pointer_pos()
    {
        // **Picked in the turned cube and applied to the authored one.** The
        // player is clicking what they can see; the cut is stored the way it
        // was made, so every cell has to come back through the turn.
        mask = match crate::shape_view::pick(area, seen, at) {
            Some((cell, face)) if adding => crate::shape_view::restore_seen(mask, turn, cell, face),
            Some((cell, _)) => crate::shape_view::chisel_seen(mask, turn, cell),
            // Nothing under the cursor. A right click on an empty block seeds
            // the middle cell, so a player who chiselled everything away is not
            // left with a screen they cannot get out of.
            None if adding && mask == 0 => crate::shape_view::seed(),
            None => mask,
        };
        if local.shape.insert(node.name.clone(), mask) != Some(mask) {
            paint.raise(
                raised,
                DialogEvent::Chiselled {
                    name: node.name.clone(),
                    shape: mask,
                },
            );
        }
    }
}

/// How big the turn arrows are, in points.
const ARROW: f32 = 26.0;

/// Where the two turn arrows sit inside a shape editor.
///
/// The TOP corners, out of the way of the cube — which is drawn in the largest
/// centred SQUARE of the widget, so the corners of a wider one are empty and
/// the corners of a square one are the parts of it furthest from anything a
/// player wants to click.
///
/// **Top rather than bottom, and that is not a matter of taste.** A dialog is
/// drawn inside the shared sheet's scroll area, and the bottom of a widget that
/// fills it does not take clicks — the arrows were down there first, drew
/// perfectly, and did nothing. Caught by
/// `an_arrow_turns_the_cube_and_does_not_chisel_it`, which sweeps the whole
/// screen for a click that turns the cube and found none.
fn turn_arrows(rect: egui::Rect) -> (egui::Rect, egui::Rect) {
    let size = egui::vec2(ARROW, ARROW);
    let inset = 2.0;
    let left = egui::Rect::from_min_size(egui::pos2(rect.left() + inset, rect.top() + inset), size);
    let right = egui::Rect::from_min_size(
        egui::pos2(rect.right() - ARROW - inset, rect.top() + inset),
        size,
    );
    (left, right)
}

/// One turn arrow, as a button. Reports whether it was pressed.
///
/// Quiet on purpose: they are a way to look at the thing being made rather
/// than part of making it, and drawing them as loudly as the Make button would
/// say otherwise.
fn turn_arrow(ui: &mut egui::Ui, rect: egui::Rect, pointing_left: bool) -> bool {
    let glyph = if pointing_left { "◀" } else { "▶" };
    ui.put(
        rect,
        egui::Button::new(egui::RichText::new(glyph).size(13.0).weak()),
    )
    .on_hover_text(if pointing_left {
        "Turn the block left"
    } else {
        "Turn the block right"
    })
    .clicked()
}

/// Writes `front`, `top` and `side` on the faces they belong to.
///
/// **The arrows are what give a cut an orientation a player can ask for.** The
/// engine turns a placed cut so its front faces whoever placed it, and toward
/// their feet on a wall (`place::oriented`) — which is a rule nobody can use
/// without being able to see which face the front is.
///
/// Deliberately quiet: small, dim, and drawn only on a face the cube is
/// actually showing. Two of the three are visible at once from most angles,
/// and the back and the left carry nothing, which is itself the answer to
/// "which way round is this".
fn paint_face_labels(ui: &egui::Ui, area: egui::Rect, mask: u32, turn: crate::shape_view::Turn) {
    // Nothing to write on. A block chiselled away to nothing has no faces, and
    // labels floating in the space where it was would be worse than none.
    if mask == 0 {
        return;
    }
    let painter = ui.painter();
    let colour = ui.visuals().weak_text_color();
    for face in [
        crate::shape_view::Face::Top,
        crate::shape_view::Face::Right,
        crate::shape_view::Face::Front,
    ] {
        let Some(label) = crate::shape_view::label(face, turn) else {
            continue;
        };
        // The whole block's face, not a cell's: the label belongs to the cut
        // rather than to whichever cell happens to be in the corner, and a
        // carved block has no single cell that is "the middle of this side".
        let corners = crate::shape_view::block_corners(area, face);
        let centre = corners
            .iter()
            .fold(egui::Vec2::ZERO, |sum, corner| sum + corner.to_vec2())
            / 4.0;
        painter.text(
            centre.to_pos2(),
            egui::Align2::CENTER_CENTER,
            label.text(),
            // The editor's own size, for the reason a slot's count uses the
            // slot's: a mod that asks for a bigger chiselling area should get
            // bigger writing on it, not the same eleven points in a larger box.
            egui::FontId::proportional((area.width() * 0.06).max(11.0)),
            colour,
        );
    }
}

/// One face of one cell: the block's own texture, on a parallelogram.
///
/// A textured mesh rather than a flat polygon, because a shape editor that
/// showed untextured lozenges would be asking the player to imagine what they
/// were carving. The four screen corners take the four corners of the
/// material's atlas tile, in the same order, so the tile follows the
/// projection's skew instead of being drawn square and floating.
///
/// Falls back to the flat tint when there is no atlas — the frames before the
/// material table arrives, exactly as a slot does.
/// # Panics
///
/// Never: the `expect` reads back the vertex pushed on the line above it.
pub fn paint_cell_face(
    painter: &egui::Painter,
    corners: [egui::Pos2; 4],
    icons: crate::icons::Icons<'_>,
    material: u16,
    face: crate::shape_view::Face,
) {
    let outline = egui::Stroke::new(1.0, egui::Color32::from_black_alpha(90));
    if let Some((texture, uv)) = icons.of(material) {
        // **Grey, not the material's colour.** A mesh multiplies its vertex
        // colour into the tile, so anything but neutral would apply the hashed
        // stand-in colour ON TOP of the real texture and tint every block
        // towards its own id.
        let tint = shade(egui::Color32::WHITE, face);
        let mut mesh = egui::Mesh::with_texture(texture);
        let uvs = [
            uv.left_top(),
            uv.right_top(),
            uv.right_bottom(),
            uv.left_bottom(),
        ];
        for (corner, uv) in corners.iter().zip(uvs) {
            // **Pushed rather than `colored_vertex`.** That helper is for an
            // UNTEXTURED mesh and debug-asserts as much, so this panicked in
            // any build with debug assertions on — which `cargo run -p client`
            // is — the moment a slot or the shape editor had an atlas to draw
            // from. It has been latent since the atlas was bridged into egui:
            // every test that reached this line did so without an atlas, and
            // took the flat-tint branch below instead.
            mesh.vertices.push(egui::epaint::Vertex {
                pos: *corner,
                uv,
                color: tint,
            });
        }
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        painter.add(egui::Shape::mesh(mesh));
        painter.add(egui::Shape::closed_line(corners.to_vec(), outline));
    } else {
        // No atlas yet: the same scaling on the hashed colour, which still
        // reads as three planes.
        let tint = shade(material_tint(material), face);
        painter.add(egui::Shape::convex_polygon(corners.to_vec(), tint, outline));
    }
}

/// What a mod named, or the engine's own shade.
///
/// A free function so the decision is testable without an egui context: what
/// went wrong was never the painting, it was a widget reaching for a constant
/// when the style had an answer.
fn or_grey(named: Option<egui::Color32>, default: u8) -> egui::Color32 {
    named.unwrap_or_else(|| egui::Color32::from_gray(default))
}

/// A colour a step brighter, for a hovered control.
///
/// 1.4 is what takes the button's resting `from_gray(64)` to the
/// `from_gray(90)` it used to hover at, so nothing about the engine's own look
/// moved when mod colours started reaching these widgets.
fn lighter(base: egui::Color32) -> egui::Color32 {
    let channel = |value: u8| {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a scaled colour channel, clamped into a byte"
        )]
        {
            (f32::from(value) * 1.4).clamp(0.0, 255.0) as u8
        }
    };
    egui::Color32::from_rgba_unmultiplied(
        channel(base.r()),
        channel(base.g()),
        channel(base.b()),
        base.a(),
    )
}

/// How light one face of a cell is.
///
/// Three fixed levels rather than a light calculation: the point is that the
/// three visible faces read as three planes, and a player looking at a shape
/// needs to see its corners, not to know where the sun is.
fn shade(base: egui::Color32, face: crate::shape_view::Face) -> egui::Color32 {
    let scale = match face {
        crate::shape_view::Face::Top => 1.0,
        crate::shape_view::Face::Right => 0.78,
        crate::shape_view::Face::Front => 0.6,
    };
    let channel = |value: u8| {
        #[expect(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            reason = "a scaled colour channel, clamped into a byte"
        )]
        {
            (f32::from(value) * scale).clamp(0.0, 255.0) as u8
        }
    };
    egui::Color32::from_rgb(channel(base.r()), channel(base.g()), channel(base.b()))
}

/// The colour that stands in for a material when there is no atlas.
///
/// **The fallback, not the normal path** — see [`crate::icons::Icons`], which
/// draws the real tile once the server's material table has arrived. This is
/// what a slot shows on the frames before that, and for a client that never
/// receives one.
///
/// Shared with the tier-2 HUD's `Icon` command, so a mod's hotbar and the
/// engine's inventory slots fall back the same way. Two independent hashes of
/// the same id would be the sort of difference a player notices and nobody can
/// explain.
#[must_use]
pub fn material_tint(material: u16) -> egui::Color32 {
    // Keyed off the id so two materials look different.
    #[expect(
        clippy::cast_possible_truncation,
        reason = "a deliberate hash into a byte"
    )]
    egui::Color32::from_rgb(
        60u8.wrapping_add(material.wrapping_mul(37) as u8),
        90u8.wrapping_add(material.wrapping_mul(59) as u8),
        120u8.wrapping_add(material.wrapping_mul(17) as u8),
    )
}

/// A button, and the press it reports.
fn paint_button(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    node: &Node,
    text: &str,
    paint: &Paint,
    raised: &mut Vec<Raised>,
) {
    let response = ui.allocate_rect(rect, egui::Sense::click());
    // **Lightened rather than a second constant.** The hover was `from_gray(90)`
    // against a base of `from_gray(64)`, which is this scale applied to that
    // grey — so a mod's colour keeps the cue instead of losing it to a shade of
    // grey, and the default looks exactly as it did.
    let base = paint.fill(64);
    let fill = if response.hovered() {
        lighter(base)
    } else {
        base
    };
    ui.painter().rect_filled(rect, 3.0, fill);
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        text,
        paint.font.clone(),
        paint.colour,
    );
    // An unnamed button raises nothing: a mod that did not name it has no way
    // to tell it apart from any other, so telling it would be noise.
    if response.clicked() && !node.name.is_empty() {
        paint.raise(
            raised,
            DialogEvent::Pressed {
                name: node.name.clone(),
            },
        );
    }
}

/// A checkbox. Its ticked state is the CLIENT's until the server replaces the
/// tree, so a player's click shows immediately rather than after a round trip.
#[expect(
    clippy::too_many_arguments,
    reason = "a widget painter takes its widget, its box, and where events go"
)]
fn paint_checkbox(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    node: &Node,
    text: &str,
    checked: bool,
    paint: &Paint,
    local: &mut Local,
    raised: &mut Vec<Raised>,
) {
    let response = ui.allocate_rect(rect, egui::Sense::click());
    let state = local.checked.entry(node.name.clone()).or_insert(checked);
    if response.clicked() {
        *state = !*state;
        if !node.name.is_empty() {
            paint.raise(
                raised,
                DialogEvent::Toggled {
                    name: node.name.clone(),
                    checked: *state,
                },
            );
        }
    }
    let box_rect = egui::Rect::from_min_size(rect.left_top(), egui::vec2(16.0, 16.0));
    ui.painter().rect_stroke(
        box_rect,
        2.0,
        egui::Stroke::new(1.0, paint.colour),
        egui::StrokeKind::Inside,
    );
    if *state {
        ui.painter().text(
            box_rect.center(),
            egui::Align2::CENTER_CENTER,
            "x",
            paint.font.clone(),
            paint.colour,
        );
    }
    ui.painter().text(
        rect.left_center() + egui::vec2(24.0, 0.0),
        egui::Align2::LEFT_CENTER,
        text,
        paint.font.clone(),
        paint.colour,
    );
}

/// A slider. Reports on RELEASE, not per frame.
///
/// A drag across a slider would otherwise send one message per frame of the
/// drag — sixty a second, for a value the server only needs once.
fn paint_slider(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    node: &Node,
    bounds: (i32, i32, i32),
    paint: &Paint,
    local: &mut Local,
    raised: &mut Vec<Raised>,
) {
    let (min, max, value) = bounds;
    let response = ui.allocate_rect(rect, egui::Sense::click_and_drag());
    let current = local.slider.entry(node.name.clone()).or_insert(value);
    if let Some(pos) = response
        .interact_pointer_pos()
        .filter(|_| response.dragged() || response.clicked())
    {
        let t = ((pos.x - rect.left()) / rect.width().max(1.0)).clamp(0.0, 1.0);
        let span = i64::from(max) - i64::from(min);
        // Integer arithmetic for the pick, so no float rounding decides which
        // notch a slider lands on — and `round` is on rule 4's banned list.
        let picked = i64::from(min) + (f64::from(t) * span as f64) as i64;
        *current = i32::try_from(picked).unwrap_or(min).clamp(min, max);
    }
    ui.painter().rect_filled(rect, 2.0, paint.fill(48));
    let filled = if max > min {
        (f64::from(*current - min) / f64::from(max - min)) as f32
    } else {
        0.0
    };
    let mut bar = rect;
    bar.set_width(rect.width() * filled.clamp(0.0, 1.0));
    ui.painter()
        .rect_filled(bar, 2.0, egui::Color32::from_gray(120));
    ui.painter().text(
        rect.center(),
        egui::Align2::CENTER_CENTER,
        current.to_string(),
        paint.font.clone(),
        paint.colour,
    );
    if response.drag_stopped() && !node.name.is_empty() {
        paint.raise(
            raised,
            DialogEvent::Slid {
                name: node.name.clone(),
                value: *current,
            },
        );
    }
}

/// A dropdown, which cycles rather than opening a list.
///
/// A popup is a second interaction model — focus, dismissal, keyboard — and the
/// same schema renders either way, so the list can arrive later without a
/// protocol change or a mod noticing.
#[expect(
    clippy::too_many_arguments,
    reason = "a widget painter takes its widget, its box, and where events go"
)]
fn paint_dropdown(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    node: &Node,
    options: &[String],
    selected: u16,
    paint: &Paint,
    local: &mut Local,
    raised: &mut Vec<Raised>,
) {
    let response = ui.allocate_rect(rect, egui::Sense::click());
    let current = local.dropdown.entry(node.name.clone()).or_insert(selected);
    if response.clicked() && !options.is_empty() {
        *current = (*current + 1) % u16::try_from(options.len()).unwrap_or(1);
        if !node.name.is_empty() {
            paint.raise(
                raised,
                DialogEvent::Chose {
                    name: node.name.clone(),
                    index: *current,
                },
            );
        }
    }
    let shown = options
        .get(usize::from(*current))
        .map_or("", String::as_str);
    ui.painter().rect_filled(rect, 3.0, paint.fill(48));
    ui.painter().text(
        rect.left_center() + egui::vec2(6.0, 0.0),
        egui::Align2::LEFT_CENTER,
        shown,
        paint.font.clone(),
        paint.colour,
    );
}

/// A text field. Submits on Enter, not on every keystroke.
///
/// What a player is half-way through typing is not something the server needs,
/// and sending it would put every keystroke of a password field on the wire.
#[expect(
    clippy::too_many_arguments,
    reason = "a widget painter takes its widget, its box, and where events go"
)]
fn paint_text_input(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    node: &Node,
    initial: &str,
    placeholder: &str,
    paint: &Paint,
    local: &mut Local,
    raised: &mut Vec<Raised>,
) {
    let buffer = local
        .text
        .entry(node.name.clone())
        .or_insert_with(|| initial.to_owned());
    let mut edit = buffer.clone();
    let response = ui.put(
        rect,
        egui::TextEdit::singleline(&mut edit).hint_text(placeholder),
    );
    buffer.clone_from(&edit);
    if response.lost_focus()
        && ui.input(|i| i.key_pressed(egui::Key::Enter))
        && !node.name.is_empty()
    {
        paint.raise(
            raised,
            DialogEvent::Submitted {
                name: node.name.clone(),
                text: edit,
            },
        );
    }
}

/// A rectangle of slots from one view.
#[expect(
    clippy::too_many_arguments,
    reason = "a widget painter takes its widget, its box, and where events go"
)]
fn paint_grid(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    view: &str,
    shape: (u16, u16, u16),
    form: &str,
    views: &BTreeMap<String, ViewContents>,
    paint: &Paint,
    raised: &mut Vec<Raised>,
) {
    let (columns, first, count) = shape;
    let columns = i32::from(columns.max(1));
    for offset in 0..i32::from(count) {
        let (row, column) = (offset.div_euclid(columns), offset.rem_euclid(columns));
        let slot = egui::Rect::from_min_size(
            rect.left_top() + egui::vec2((column * SLOT) as f32, (row * SLOT) as f32),
            egui::vec2(SLOT as f32, SLOT as f32),
        );
        let index = first.saturating_add(u16::try_from(offset).unwrap_or(0));
        paint_slot(ui, slot, view, index, form, views, paint, raised);
    }
}

/// One inventory slot, and the click it reports.
///
/// **What it reports is a gesture.** Which stack moves where is the server's
/// decision, taken against its own inventory — see
/// [`tiamot_core::proto::DialogEvent::Clicked`]. What it DRAWS is likewise the
/// server's last word: this never edits a slot locally, so a client that lied
/// about a click still sees the truth a moment later.
#[expect(
    clippy::too_many_arguments,
    reason = "a widget painter takes its widget, its box, and where events go"
)]
fn paint_slot(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    view: &str,
    index: u16,
    form: &str,
    views: &BTreeMap<String, ViewContents>,
    paint: &Paint,
    raised: &mut Vec<Raised>,
) {
    let inner = rect.shrink(2.0);
    let mut response = ui.allocate_rect(inner, egui::Sense::click());
    ui.painter().rect_filled(inner, 2.0, paint.fill(52));
    ui.painter().rect_stroke(
        inner,
        2.0,
        egui::Stroke::new(1.0, paint.edge(80)),
        egui::StrokeKind::Inside,
    );

    // What the server last said is in it.
    if let Some(stack) = views
        .get(view)
        .and_then(|contents| contents.slots.get(usize::from(index)).cloned().flatten())
    {
        let (material, units) = (stack.material, stack.units);
        paint
            .icons
            .paint_stack(ui.painter(), inner.shrink(6.0), material, stack.shape);
        let label = stack_label(units, stack.shape);
        ui.painter().text(
            inner.right_bottom() - egui::vec2(2.0, 2.0),
            egui::Align2::RIGHT_BOTTOM,
            label,
            count_font(inner.width()),
            paint.colour,
        );

        // **What it is, on hover.** A slot showed a picture and a count and
        // never its name, so telling two greys apart meant placing one. Asked
        // for from the window.
        //
        // Only when the name table has arrived: a tooltip reading `#7` is worse
        // than no tooltip at all, because it looks like the name.
        if let Some(name) = paint.icons.name_of(material) {
            let (blocks, spares) = tiamot_core::inventory::display(units);
            response = response.on_hover_text(format!("{name}\n{blocks} blocks + {spares} nodes"));
        }
    }

    let click = if response.clicked() {
        let shift = ui.input(|i| i.modifiers.shift);
        Some(if shift { Click::ShiftLeft } else { Click::Left })
    } else if response.secondary_clicked() {
        Some(Click::Right)
    } else {
        None
    };
    if let Some(click) = click {
        raised.push(Raised {
            form: form.to_owned(),
            event: DialogEvent::Clicked {
                view: view.to_owned(),
                index,
                click,
            },
        });
    }
}

#[cfg(test)]
mod tests {
    use tiamot_core::ui::{Align, Direction};

    use super::*;

    #[test]
    fn a_widgets_own_fill_defers_to_what_the_mod_asked_for() {
        // **Reported from the window**: inventory slots and buttons painting
        // grey over the colours a mod supplied.
        //
        // `paint_background` drew `style.background` under every widget, and
        // then any widget with a fill of its own — a button, a slot, a
        // dropdown, a slider track, a progress bar — painted an unconditional
        // grey straight over it. The mod's colour was on screen for the length
        // of one draw call.
        let mods_own = egui::Color32::from_rgb(200, 40, 40);
        assert_eq!(
            or_grey(Some(mods_own), 52),
            mods_own,
            "a widget reached for its constant with a style in hand"
        );
        assert_eq!(
            or_grey(None, 52),
            egui::Color32::from_gray(52),
            "the engine still has a look of its own where nothing was said"
        );

        // And a hovered button lightens whatever it is rather than becoming a
        // shade of grey — 1.4 is exactly what took the old resting 64 to the
        // old hover 90, so the default is unmoved.
        assert_eq!(
            lighter(egui::Color32::from_gray(64)),
            egui::Color32::from_gray(89)
        );
        let hot = lighter(mods_own);
        assert!(
            hot.r() > mods_own.r() && hot.g() > mods_own.g(),
            "a mod-coloured button lost its hover cue: {mods_own:?} -> {hot:?}"
        );
    }

    #[test]
    fn a_prompt_is_its_own_size_and_never_larger_than_the_sheet() {
        let area = (1920.0, 1080.0);
        let sheet = crate::panel::size(area);
        #[expect(clippy::cast_possible_truncation, reason = "a size in points")]
        let sheet = (sheet.0 as i32, sheet.1 as i32);

        assert_eq!(prompt_size((200, 150), area), (200, 150));
        // A mod that asks for a window bigger than the screen does not get one.
        assert_eq!(prompt_size((4000, 4000), area), sheet);
        // And a floor, so a tree that measures to nothing is still clickable.
        assert_eq!(prompt_size((0, 0), area), (160, 120));
    }

    /// Draws `tree` as a screen into a headless egui and returns what it
    /// covered, in points.
    fn drawn_extent(tree: Tree, area: (f32, f32)) -> egui::Rect {
        let ctx = egui::Context::default();
        crate::app::install_fonts(&ctx);
        let raw = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(area.0, area.1),
            )),
            ..Default::default()
        };
        let mut dialogs = Dialogs::default();
        let mut open = BTreeMap::new();
        open.insert("mod:screen".to_owned(), Screen::new(tree, false));
        let views = BTreeMap::new();

        // Twice: the first pass has no fonts laid out yet, so what it measures
        // is not what a player sees. The second is a steady frame.
        let mut covered = egui::Rect::NOTHING;
        for _ in 0..2 {
            let output = ctx.run_ui(raw.clone(), |root| {
                let ctx = root.ctx().clone();
                dialogs.draw(
                    &ctx,
                    &open,
                    &views,
                    Icons::default(),
                    &BTreeMap::new(),
                    area,
                );
            });
            covered = egui::Rect::NOTHING;
            for clipped in &output.shapes {
                // **Intersected with its clip rectangle.** A shape scrolled out
                // of view still reports its own bounds, so the union of those
                // measures the CONTENT rather than the window. A window that
                // really did grow grows its clip rectangle with it, so this
                // still catches the thing the test is about.
                let visible = clipped
                    .shape
                    .visual_bounding_rect()
                    .intersect(clipped.clip_rect);
                if visible.is_positive() {
                    covered = covered.union(visible);
                }
            }
        }
        covered
    }

    /// A shape editor on screen, that a test can click at.
    ///
    /// One context and one `Dialogs` for the whole sweep: building them per
    /// point costs a font atlas each time, which turned a sub-second test into
    /// a fifty-second one.
    struct Editor {
        ctx: egui::Context,
        dialogs: Dialogs,
        open: BTreeMap<String, Screen>,
        views: BTreeMap<String, ViewContents>,
    }

    impl Editor {
        fn new() -> Self {
            let ctx = egui::Context::default();
            crate::app::install_fonts(&ctx);
            let mut node = Node::new(Widget::ShapeEditor {
                shape: 0x7FF_FFFF,
                material: 1,
            });
            node.name = "cut".to_owned();
            let mut open = BTreeMap::new();
            open.insert(
                "mod:screen".to_owned(),
                Screen::new(Tree { nodes: vec![node] }, false),
            );
            let mut editor = Self {
                ctx,
                dialogs: Dialogs::default(),
                open,
                views: BTreeMap::new(),
            };
            // Twice: the first pass has no fonts laid out, so nothing is where
            // a player would find it.
            editor.frame(Vec::new());
            editor.frame(Vec::new());
            editor
        }

        fn frame(&mut self, events: Vec<egui::Event>) -> Vec<Raised> {
            let raw = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 720.0),
                )),
                events,
                ..Default::default()
            };
            let mut raised = Vec::new();
            let dialogs = &mut self.dialogs;
            let open = &self.open;
            let views = &self.views;
            let _ = self.ctx.run_ui(raw, |root| {
                let ctx = root.ctx().clone();
                raised = dialogs.draw(
                    &ctx,
                    open,
                    views,
                    Icons::default(),
                    &BTreeMap::new(),
                    (1280.0, 720.0),
                );
            });
            raised
        }

        fn click(&mut self, at: egui::Pos2) -> Vec<Raised> {
            let button = |pressed| egui::Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed,
                modifiers: egui::Modifiers::default(),
            };
            self.frame(vec![egui::Event::PointerMoved(at)]);
            self.frame(vec![egui::Event::PointerMoved(at), button(true)]);
            let raised = self.frame(vec![button(false)]);
            self.frame(Vec::new());
            raised
        }

        fn turn(&self) -> crate::shape_view::Turn {
            self.dialogs
                .forms
                .get("mod:screen")
                .and_then(|local| local.turn.get("cut").copied())
                .unwrap_or(0)
        }
    }

    #[test]
    fn an_arrow_turns_the_cube_and_does_not_chisel_it() {
        // **Asked for from the window**: left and right arrows rather than a
        // drag. The drag was the wrong answer for a reason worth keeping
        // written down — the gesture that turned the cube and the gesture that
        // chiselled a cell were the same button, so a click with a little
        // movement in it did both.
        //
        // Two claims: the arrow turns the view, and it does not touch the cut.
        // The second is the one that matters, because a chisel raises an event
        // the mod acts on and spends the player's material.
        let mut editor = Editor::new();
        let mut found = None;
        'sweep: for y in (0..720).step_by(6) {
            for x in (0..1280).step_by(6) {
                let at = egui::pos2(x as f32, y as f32);
                let raised = editor.click(at);
                if editor.turn() != 0 {
                    found = Some((at, raised));
                    break 'sweep;
                }
            }
        }
        let (at, raised) = found.expect("no click anywhere on the editor turned the cube");
        assert!(
            !raised
                .iter()
                .any(|event| matches!(event.event, DialogEvent::Chiselled { .. })),
            "the arrow at {at:?} also chiselled: {raised:?}"
        );
    }

    #[test]
    fn the_two_arrows_are_apart_and_inside_the_widget() {
        // Geometry only, so a layout change that overlapped them or pushed one
        // off the widget fails here rather than as a button that does the
        // other one's job.
        let rect = egui::Rect::from_min_size(egui::pos2(10.0, 20.0), egui::vec2(200.0, 200.0));
        let (left, right) = super::turn_arrows(rect);
        assert!(rect.contains_rect(left) && rect.contains_rect(right));
        assert!(
            !left.intersects(right),
            "the two arrows overlap: {left:?} and {right:?}"
        );
        assert!(
            left.center().x < right.center().x,
            "left is not on the left"
        );
    }

    #[test]
    fn a_screen_stays_inside_the_sheet_however_much_is_in_it() {
        /// egui's window frame: `panel::size` is the room the CONTENTS get, and
        /// the window drawn around it is that plus its own margin and stroke.
        /// Every screen in the game carries the same one.
        const FRAME: f32 = 64.0;

        // **The reported bug, twice over.** A screen used to build its own
        // window at the sheet's size, lay the tree into the whole of it, and
        // then add a Close button underneath — and `fixed_size` on an
        // `egui::Window` is a request rather than a bound, so the window grew
        // by however much did not fit and the inventory reached the top and the
        // bottom of the screen.
        //
        // Measured on what was actually PAINTED rather than on a number the
        // sizing function returned, because the number was already right: it
        // was the window that ignored it.
        let area = (1920.0, 1080.0);
        let sheet = crate::panel::size(area);

        let full = |labels: usize| {
            let children: Vec<_> = (0..labels)
                .map(|index| {
                    tiamot_core::ui::Build::leaf(Widget::Label {
                        text: format!("line {index} of a screen with a great deal in it"),
                    })
                })
                .collect();
            tiamot_core::ui::Build::of(
                Node::new(Widget::Container {
                    direction: Direction::Column,
                    gap: 4,
                    padding: 8,
                    align: Align::Start,
                }),
                children,
            )
            .flatten()
        };

        let little = drawn_extent(full(1), area);
        let lots = drawn_extent(full(200), area);

        // **It does not reach the top and the bottom**, which is the words the
        // report used. A sixteenth of the screen clear at each end — the bound
        // is loose because it is guarding against a window with NO margin,
        // which is what was reported, and because the sheet is centred by the
        // room its contents get rather than by the frame drawn around them.
        let margin = area.1 / 16.0;
        assert!(
            lots.min.y > margin && lots.max.y < area.1 - margin,
            "a screen with a lot in it covered {lots:?} of a {area:?} window, so it runs off \
             the top and the bottom"
        );
        assert!(
            (lots.height() - little.height()).abs() < 1.0
                && (lots.width() - little.width()).abs() < 1.0,
            "two screens are different sizes ({little:?} against {lots:?}), so switching tabs \
             moves the window"
        );
        // And it is the SHEET rather than some other stable size — a window
        // pinned to the whole screen would pass the two checks above.
        //
        assert!(
            lots.height() - sheet.1 < FRAME && lots.height() > sheet.1 - FRAME,
            "a screen covered {} points and the sheet is {}",
            lots.height(),
            sheet.1
        );
    }

    #[test]
    fn state_for_a_closed_dialog_is_forgotten() {
        // A player who closes a shop and opens it again gets an empty field,
        // and a session does not accumulate state for forms it never sees
        // again.
        let mut dialogs = Dialogs::default();
        dialogs.forms.insert("a:one".to_owned(), Local::default());
        dialogs.forms.insert("a:two".to_owned(), Local::default());

        let mut open = BTreeMap::new();
        open.insert(
            "a:two".to_owned(),
            Screen {
                tree: Tree { nodes: Vec::new() },
                compact: false,
            },
        );
        dialogs.retain_open(&open);

        assert!(
            !dialogs.forms.contains_key("a:one"),
            "state outlived its dialog"
        );
        assert!(dialogs.forms.contains_key("a:two"));
    }

    #[test]
    fn a_grid_reports_the_slot_that_was_clicked() {
        // The index arithmetic, which is the part of slot handling that is
        // wrong silently: a grid starting at `first` with `columns` per row.
        let columns = 9i32;
        let first = 27u16;
        for offset in [0i32, 1, 8, 9, 26] {
            let (row, column) = (offset.div_euclid(columns), offset.rem_euclid(columns));
            let index = first.saturating_add(u16::try_from(offset).unwrap_or(0));
            assert_eq!(
                usize::from(index),
                usize::from(first) + usize::try_from(row * columns + column).expect("fits"),
                "offset {offset} resolved to the wrong slot"
            );
        }
    }

    #[test]
    fn direction_and_align_survive_the_round_trip_into_layout() {
        // Not a rendering test — a guard that this module keeps agreeing with
        // `core::ui` about what a row is. It has no font here, so it uses the
        // same trait a test would.
        struct Ruler;
        impl Measure for Ruler {
            fn natural(&self, _widget: &Widget, _style: &Style) -> (i32, i32) {
                (10, 10)
            }
        }
        let tree = tiamot_core::ui::Build::of(
            Node::new(Widget::Container {
                direction: Direction::Row,
                gap: 0,
                padding: 0,
                align: Align::Start,
            }),
            vec![
                tiamot_core::ui::Build::leaf(Widget::Spacer),
                tiamot_core::ui::Build::leaf(Widget::Spacer),
            ],
        )
        .flatten();
        let laid = layout(&tree, Rect::new(0, 0, 100, 50), &Ruler);
        assert_eq!(laid.children.len(), 2);
        assert!(
            laid.children[1].rect.x > laid.children[0].rect.x,
            "a row did not lay out left to right"
        );
    }
    #[test]
    fn a_count_grows_with_the_slot_it_sits_in() {
        // **Reported by a mod author building a bigger interface.** The count
        // was a flat eleven points everywhere, so a slot twice the size had the
        // same small number in the corner of a much larger box — which reads as
        // an inventory that lost its numbers rather than as a font size.
        let small = count_font(36.0).size;
        let large = count_font(96.0).size;
        assert!(
            large > small * 2.0,
            "a slot nearly three times the size got {large} against {small}"
        );

        // And never below what it was: shrinking a count out of legibility on
        // a small slot would trade one complaint for the other.
        assert!((count_font(8.0).size - 11.0).abs() < f32::EPSILON);
        assert!((count_font(36.0).size - 11.0).abs() < f32::EPSILON);
    }

    #[test]
    fn a_scroll_box_stops_at_both_ends_and_ignores_a_wheel_it_does_not_need() {
        // The clamp is applied on READ rather than only when the wheel turns,
        // because the content changes under it: a mod redrawing a list one item
        // shorter leaves a box scrolled past its own end, and a panel that
        // looks empty until you scroll back up reads as lost contents.
        let ctx = egui::Context::default();
        let _ = ctx.run_ui(egui::RawInput::default(), |root| {
            let ui = root;
            let mut local = Local::default();
            let rect = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(100.0, 50.0));

            // Content shorter than the box: nothing to scroll, and the offset
            // stays at the top however the content changed.
            local.scroll.insert(0, 40.0);
            assert!(
                (local.scroll_by(ui, 0, rect, 20.0) - 0.0).abs() < f32::EPSILON,
                "a box with nothing to scroll kept an offset"
            );

            // Content taller than the box: the offset is allowed up to the
            // difference and no further.
            local.scroll.insert(1, 500.0);
            assert!(
                (local.scroll_by(ui, 1, rect, 130.0) - 80.0).abs() < f32::EPSILON,
                "the offset should clamp to content minus box height"
            );
        });
    }
}
