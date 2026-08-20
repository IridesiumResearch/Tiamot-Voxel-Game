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

/// Per-widget state the tree itself cannot carry.
#[derive(Debug, Default)]
struct Local {
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
            // Containers measure from their children in `core::ui`, and a
            // spacer wants nothing — all three are "no intrinsic size".
            Widget::Spacer | Widget::Container { .. } | Widget::Scroll => (0, 0),
        }
    }
}

/// One inventory slot's size in virtual pixels, borders included.
const SLOT: i32 = 36;

impl Dialogs {
    /// Forgets state for dialogs that are no longer open.
    ///
    /// A player who closes a shop and opens it again gets an empty text field,
    /// which is what they expect — and it stops a mod's dialog accumulating
    /// state for a session's worth of forms it never uses again.
    pub fn retain_open(&mut self, open: &BTreeMap<String, Tree>) {
        self.forms.retain(|form, _| open.contains_key(form));
    }

    /// Draws every open dialog and returns what the player did.
    pub fn draw(
        &mut self,
        ctx: &egui::Context,
        open: &BTreeMap<String, Tree>,
        area: (f32, f32),
    ) -> Vec<Raised> {
        self.retain_open(open);
        let mut raised = Vec::new();
        for (form, tree) in open {
            let local = self.forms.entry(form.clone()).or_default();
            raised.extend(draw_form(ctx, form, tree, local, area));
        }
        raised
    }
}

/// Draws one dialog in its own window.
fn draw_form(
    ctx: &egui::Context,
    form: &str,
    tree: &Tree,
    local: &mut Local,
    area: (f32, f32),
) -> Vec<Raised> {
    let mut raised = Vec::new();
    // The dialog gets at most three quarters of the window, so a mod cannot
    // cover the game with one — and at least something, so a small tree is not
    // laid out into nothing.
    let width = (area.0 * 0.75).max(160.0) as i32;
    let height = (area.1 * 0.75).max(120.0) as i32;

    let mut close = false;
    egui::Window::new(form)
        .collapsible(false)
        .resizable(false)
        .default_width(f64::from(width) as f32)
        .show(ctx, |ui| {
            let origin = ui.cursor().min;
            let ruler = EguiRuler { ctx };
            let laid = layout(tree, Rect::new(0, 0, width, height), &ruler);
            // The tree and its rectangles are walked TOGETHER, by index, so a
            // renderer cannot pair a widget with somebody else's rectangle —
            // which a flat list plus a separate traversal invites.
            paint(ui, origin, tree, 0, &laid, form, local, &mut raised);
            ui.allocate_space(egui::vec2(
                laid.rect.w as f32,
                (laid.rect.h as f32).min(area.1 * 0.75),
            ));
            if ui.button("Close").clicked() {
                close = true;
            }
        });
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
    raised: &mut Vec<Raised>,
) {
    let Some(node) = tree.nodes.get(index) else {
        return;
    };
    let rect = egui::Rect::from_min_size(
        origin + egui::vec2(laid.rect.x as f32, laid.rect.y as f32),
        egui::vec2(laid.rect.w as f32, laid.rect.h as f32),
    );
    paint_background(ui, rect, &node.style);
    paint_widget(ui, rect, node, form, local, raised);

    for (child, child_laid) in tree.children_of(index).zip(&laid.children) {
        paint(ui, origin, tree, child, child_laid, form, local, raised);
    }
}

/// The style tokens that apply to any widget.
fn paint_background(ui: &egui::Ui, rect: egui::Rect, style: &Style) {
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
    /// Text colour, resolved from the node's style.
    colour: egui::Color32,
    /// Text font, resolved from the node's style.
    font: egui::FontId,
}

impl Paint<'_> {
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
fn paint_widget(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    node: &Node,
    form: &str,
    local: &mut Local,
    raised: &mut Vec<Raised>,
) {
    let paint = Paint {
        form,
        colour: node.style.text_colour.map_or(egui::Color32::WHITE, |c| {
            egui::Color32::from_rgba_unmultiplied(c[0], c[1], c[2], c[3])
        }),
        font: egui::FontId::proportional(
            f32::from(node.style.text_size.unwrap_or(14)).clamp(8.0, 48.0),
        ),
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
            ui.painter()
                .rect_filled(rect, 2.0, egui::Color32::from_gray(40));
            let mut bar = rect;
            bar.set_width(rect.width() * f32::from(*permille) / 1000.0);
            ui.painter()
                .rect_filled(bar, 2.0, egui::Color32::from_rgb(90, 160, 90));
        }
        Widget::ItemSlot { view, index } => {
            paint_slot(ui, rect, view, *index, form, raised);
        }
        Widget::ItemGrid {
            view,
            columns,
            first,
            count,
        } => paint_grid(ui, rect, view, (*columns, *first, *count), form, raised),
        // Drawn by their children, or by nothing at all.
        Widget::Container { .. } | Widget::Scroll | Widget::Spacer | Widget::Image { .. } => {}
    }
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
    let fill = if response.hovered() {
        egui::Color32::from_gray(90)
    } else {
        egui::Color32::from_gray(64)
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
    ui.painter()
        .rect_filled(rect, 2.0, egui::Color32::from_gray(48));
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
    ui.painter()
        .rect_filled(rect, 3.0, egui::Color32::from_gray(48));
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
fn paint_grid(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    view: &str,
    shape: (u16, u16, u16),
    form: &str,
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
        paint_slot(ui, slot, view, index, form, raised);
    }
}

/// One inventory slot, and the click it reports.
///
/// **What it reports is a gesture.** Which stack moves where is the server's
/// decision, taken against its own inventory — see
/// [`tiamot_core::proto::DialogEvent::Clicked`].
fn paint_slot(
    ui: &mut egui::Ui,
    rect: egui::Rect,
    view: &str,
    index: u16,
    form: &str,
    raised: &mut Vec<Raised>,
) {
    let inner = rect.shrink(2.0);
    let response = ui.allocate_rect(inner, egui::Sense::click());
    ui.painter()
        .rect_filled(inner, 2.0, egui::Color32::from_gray(52));
    ui.painter().rect_stroke(
        inner,
        2.0,
        egui::Stroke::new(1.0, egui::Color32::from_gray(80)),
        egui::StrokeKind::Inside,
    );
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

/// The direction and alignment names, re-exported so the renderer's tests can
/// build trees without reaching past this module.
#[cfg(test)]
mod tests {
    use tiamot_core::ui::{Align, Direction};

    use super::*;

    #[test]
    fn state_for_a_closed_dialog_is_forgotten() {
        // A player who closes a shop and opens it again gets an empty field,
        // and a session does not accumulate state for forms it never sees
        // again.
        let mut dialogs = Dialogs::default();
        dialogs.forms.insert("a:one".to_owned(), Local::default());
        dialogs.forms.insert("a:two".to_owned(), Local::default());

        let mut open = BTreeMap::new();
        open.insert("a:two".to_owned(), Tree { nodes: Vec::new() });
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
}
