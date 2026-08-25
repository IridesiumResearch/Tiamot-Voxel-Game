// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The screen before the game: worlds, mods, and what happens when you press
//! Play.
//!
//! # Why a screen exists at all
//!
//! Until now the client read `client.toml`, started a world or dialled a
//! server, and opened a window already in it. That is fine for one world and
//! impossible for two: there was nowhere to name a second world, no way to see
//! which servers had been visited, and no way to turn a mod off short of
//! moving its directory.
//!
//! This is the state of that screen. What it decides — which worlds exist,
//! which mods are ticked, whether a selection is going to surprise the player —
//! is [`crate::launcher`]'s and is tested without a window. What is here is the
//! egui, the tab the player is on, and the one [`Action`] a frame produces.

use crate::launcher::{Catalogue, Entry, Kind, Library, Mismatch};

/// Which page is showing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Tab {
    /// The world list, and the two buttons.
    #[default]
    Play,
    /// Which mods are on.
    Mods,
    /// Everything that is not about a particular world.
    Settings,
}

/// What a frame of the front screen decided.
///
/// **One action per frame, and the window acts on it.** The screen itself
/// starts nothing: opening a world means a server, a connection and a renderer,
/// none of which belong to a menu.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Nothing happened.
    None,
    /// Open this world or server.
    Open(Entry),
    /// Make a world with this name and the mods that are ticked.
    Create(String),
    /// Close the window.
    Quit,
}

/// The front screen.
pub struct Front {
    /// Which page is showing.
    pub tab: Tab,
    /// The player's worlds and servers.
    pub library: Library,
    /// Every installed mod, and which are on.
    pub catalogue: Catalogue,
    /// Which world is highlighted, as an index into the library.
    selected: Option<usize>,
    /// What is typed into the new-world box.
    name: String,
    /// What is typed into the address box.
    address: String,
    /// The mod-set warning waiting to be answered, and which world it is about.
    confirming: Option<(usize, Mismatch)>,
    /// Something that went wrong, for the player to read.
    pub notice: Option<String>,
    /// Whether a setting changed and `client.toml` wants writing.
    settings_dirty: bool,
    /// Where the interface-scale slider has been dragged to and not let go of.
    /// See [`crate::widget::settle`].
    scale_draft: Option<f32>,
}

impl Front {
    /// Builds the screen from what is on disk.
    #[must_use]
    pub fn new(library: Library, catalogue: Catalogue) -> Self {
        let name = library.unused_name("New World");
        Self {
            tab: Tab::default(),
            selected: (!library.entries.is_empty()).then_some(0),
            library,
            notice: catalogue.problem.clone(),
            catalogue,
            name,
            address: String::new(),
            confirming: None,
            settings_dirty: false,
            scale_draft: None,
        }
    }

    /// Draws a frame and returns what the player asked for.
    ///
    /// `config` is edited in place — the settings tab writes straight to it and
    /// the window saves when [`Front::settings_dirty`] says so.
    pub fn draw(&mut self, ctx: &egui::Context, config: &mut crate::config::Config) -> Action {
        let mut action = Action::None;
        // **The same sheet as every in-game screen**, so a page cannot decide
        // its own size and run off the edges — see `crate::panel::sheet`. The
        // tabs are this screen's own row under the shared bar, because it is
        // the one screen with nowhere to go Back to.
        crate::panel::sheet(ctx, "Tiamot", None, |ui| {
            ui.horizontal(|ui| {
                for (tab, label) in [
                    (Tab::Play, "Play"),
                    (Tab::Mods, "Mods"),
                    (Tab::Settings, "Settings"),
                ] {
                    if ui.selectable_label(self.tab == tab, label).clicked() {
                        self.tab = tab;
                    }
                }
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Quit").clicked() {
                        action = Action::Quit;
                    }
                });
            });
            ui.separator();

            if let Some(notice) = &self.notice {
                ui.colored_label(egui::Color32::from_rgb(230, 170, 90), notice);
                ui.separator();
            }

            match self.tab {
                Tab::Play => action = self.play_tab(ui),
                Tab::Mods => self.mods_tab(ui),
                Tab::Settings => self.settings_tab(ui, config),
            }
        });

        if let Some(confirmed) = self.confirmation(ctx) {
            action = confirmed;
        }
        action
    }

    /// The world list, and the two buttons.
    fn play_tab(&mut self, ui: &mut egui::Ui) -> Action {
        let mut action = Action::None;
        {
            {
                if self.library.entries.is_empty() {
                    ui.label("No worlds yet. Name one below and press New World.");
                }
                for index in 0..self.library.entries.len() {
                    let entry = &self.library.entries[index];
                    // **Local or multiplayer, said on the line itself.** A list
                    // where the two look alike is a list where somebody deletes
                    // a friend's server thinking it is their own save.
                    let label = match &entry.kind {
                        Kind::Local { .. } => format!("🖴  {}", entry.name),
                        Kind::Remote { address } => format!("🌐  {}  —  {address}", entry.name),
                    };
                    if ui
                        .selectable_label(self.selected == Some(index), label)
                        .clicked()
                    {
                        self.selected = Some(index);
                    }
                }
            }
        }

        ui.separator();
        ui.horizontal(|ui| {
            if ui.button("Play").clicked()
                && let Some(index) = self.selected
                && let Some(entry) = self.library.entries.get(index).cloned()
            {
                action = match Library::mismatch(&entry, &self.catalogue.enabled()) {
                    Some(difference) => {
                        self.confirming = Some((index, difference));
                        Action::None
                    }
                    None => Action::Open(entry),
                };
            }
            if ui.button("Forget").clicked()
                && let Some(index) = self.selected
                && index < self.library.entries.len()
            {
                // The list only. Deleting somebody's world from a menu they
                // were browsing is not a thing a button should be able to do.
                self.library.entries.remove(index);
                self.selected = None;
            }
        });

        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label("New world");
            ui.text_edit_singleline(&mut self.name);
            if ui.button("Create").clicked() {
                let name = self.library.unused_name(self.name.trim());
                action = Action::Create(name);
            }
        });
        ui.horizontal(|ui| {
            ui.label("Server");
            ui.text_edit_singleline(&mut self.address);
            if ui.button("Add").clicked() {
                let address = self.address.trim().to_owned();
                if address.is_empty() {
                    self.notice = Some("a server needs an address, like example.com:4433".into());
                } else {
                    self.library.add(Entry {
                        name: self.library.unused_name(&address),
                        kind: Kind::Remote { address },
                        mods: Vec::new(),
                    });
                    self.address.clear();
                    self.selected = Some(self.library.entries.len() - 1);
                }
            }
        });
        action
    }

    /// The mod list: a box each, and a box at the top for all of them.
    fn mods_tab(&mut self, ui: &mut egui::Ui) {
        let mut all = self.catalogue.all_on();
        if ui.checkbox(&mut all, "Everything").changed() {
            self.catalogue.set_all(all);
        }
        ui.separator();
        {
            if self.catalogue.mods.is_empty() {
                ui.label("No mods installed. A client with none can still join servers.");
            }
            for listing in &mut self.catalogue.mods {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut listing.enabled, &listing.name);
                    ui.weak(&listing.id);
                });
                if !listing.description.is_empty() {
                    ui.indent(&listing.id, |ui| ui.weak(&listing.description));
                }
            }
        }
    }

    /// The mod-set warning, when one is waiting.
    ///
    /// **A warning and not a refusal.** Charter rule 8 makes a changed mod set
    /// survivable — an id nothing registered becomes `engine:unknown` and comes
    /// back byte for byte — so this is a surprise to be told about rather than
    /// a door to be shut.
    fn confirmation(&mut self, ctx: &egui::Context) -> Option<Action> {
        let (index, difference) = self.confirming.clone()?;
        let mut decided = None;
        egui::Window::new("Different mods")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::vec2(0.0, 0.0))
            .show(ctx, |ui| {
                ui.label("This world was last played with a different set of mods.");
                if !difference.missing.is_empty() {
                    ui.label(format!("Now off: {}", difference.missing.join(", ")));
                }
                if !difference.added.is_empty() {
                    ui.label(format!("Now on: {}", difference.added.join(", ")));
                }
                ui.label(
                    "Blocks from a mod that is off are kept and come back if you turn it on again.",
                );
                ui.horizontal(|ui| {
                    if ui.button("Play anyway").clicked() {
                        decided = self
                            .library
                            .entries
                            .get(index)
                            .cloned()
                            .map(Action::Open)
                            .or(Some(Action::None));
                    }
                    if ui.button("Cancel").clicked() {
                        decided = Some(Action::None);
                    }
                });
            });
        if decided.is_some() {
            self.confirming = None;
        }
        decided
    }

    /// Whether the settings changed and want writing out.
    ///
    /// One-shot, so the file is written once per change rather than once per
    /// frame a slider is held.
    pub const fn take_settings_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.settings_dirty, false)
    }

    /// The settings that need no world.
    ///
    /// # Why the keys are not here
    ///
    /// **There is nothing to bind yet.** Actions come from the server's
    /// `ActionTable` — a mod registers a name and the engine owns the key
    /// (charter rule 11) — so before joining, the list of things a key could
    /// do is not merely unknown, it does not exist. Volume is the same: the
    /// mixer belongs to a running world. Both are on the in-game screen, which
    /// is the only place they can be.
    ///
    /// Everything below is a field of `client.toml` and nothing else, which is
    /// why it can be set from here.
    fn settings_tab(&mut self, ui: &mut egui::Ui, config: &mut crate::config::Config) {
        use crate::config::{LightingMode, RenderMode, ShadowQuality};

        let mut changed = false;
        {
            ui.heading("Player");
            ui.horizontal(|ui| {
                ui.label("Display name");
                // A display string only. Identity is the key and the UUID
                // derived from it (charter rule 13); nothing keys on this.
                changed |= ui.text_edit_singleline(&mut config.display_name).changed();
            });
            ui.separator();

            ui.heading("Graphics");
            changed |= choice(
                ui,
                "Lighting",
                &mut config.lighting_mode,
                &[
                    (LightingMode::Simple, "Simple"),
                    (LightingMode::Classic, "Classic"),
                    (LightingMode::Beautiful, "Beautiful"),
                ],
            );
            changed |= choice(
                ui,
                "Shadows",
                &mut config.shadow_quality,
                &[
                    (ShadowQuality::Off, "Off"),
                    (ShadowQuality::Low, "Low"),
                    (ShadowQuality::Medium, "Medium"),
                    (ShadowQuality::High, "High"),
                ],
            );
            changed |= choice(
                ui,
                "Draw",
                &mut config.render_mode,
                &[
                    (RenderMode::Textured, "Textured"),
                    (RenderMode::Flat, "Flat"),
                    (RenderMode::Wireframe, "Wireframe"),
                ],
            );
            changed |= ui
                .checkbox(&mut config.vsync, "Wait for the display")
                .changed();
            changed |= ui
                .add(egui::Slider::new(&mut config.fov_degrees, 60.0..=110.0).text("field of view"))
                .changed();
            ui.separator();

            ui.heading("World");
            // **What the client ASKS for.** The server caps it — the interest
            // volume is its cost, not this machine's — so a number here is a
            // request and the HUD reports what was granted.
            changed |= ui
                .add(egui::Slider::new(&mut config.view_distance, 2..=24).text("view distance"))
                .changed();
            changed |= ui
                .add(
                    egui::Slider::new(&mut config.vertical_view_distance, 1..=12)
                        .text("vertical view distance"),
                )
                .changed();
            ui.separator();

            ui.heading("Interface");
            // On release, not live: the scale rescales the slider, so applying
            // it mid-drag moves the control out from under the pointer. See
            // [`crate::widget::settle`].
            if let Some(scale) = crate::widget::on_release(
                ui,
                "scale",
                crate::config::UI_SCALE_RANGE,
                crate::config::UI_SCALE_STEP,
                config.ui_scale,
                &mut self.scale_draft,
            ) {
                changed |= (config.ui_scale - scale).abs() > f32::EPSILON;
                config.ui_scale = scale;
            }
            changed |= ui
                .checkbox(&mut config.hud_visible, "Show the HUD")
                .changed();
            changed |= ui
                .checkbox(&mut config.debug_overlay, "Debug readouts")
                .changed();
            ui.add_space(8.0);
            ui.weak("Keys and volume are on the in-game screen — press Escape in a world.");
            ui.weak("A mod's controls only exist once a server has told the client about them.");
        }
        self.settings_dirty |= changed;
    }

    /// Which mods are ticked, for a world about to start.
    #[must_use]
    pub fn enabled_mods(&self) -> Vec<String> {
        self.catalogue.enabled()
    }

    /// Highlights a world by name, if it is in the list.
    pub fn select(&mut self, name: &str) {
        self.selected = self
            .library
            .entries
            .iter()
            .position(|entry| entry.name == name);
    }
}

/// One setting with a fixed set of answers, as a row of buttons.
///
/// Buttons rather than a dropdown: there are three or four of each, a player
/// wants to see what the choices ARE, and a menu that has to be opened to find
/// out is a menu that gets left on its default.
fn choice<T: PartialEq + Copy>(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut T,
    options: &[(T, &str)],
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.add_sized([120.0, 18.0], egui::Label::new(label));
        for (option, text) in options {
            if ui.selectable_label(*value == *option, *text).clicked() && *value != *option {
                *value = *option;
                changed = true;
            }
        }
    });
    changed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn world(name: &str, mods: &[&str]) -> Entry {
        Entry {
            name: name.to_owned(),
            kind: Kind::Local {
                path: PathBuf::from("worlds").join(name),
            },
            mods: mods.iter().map(|id| (*id).to_owned()).collect(),
        }
    }

    fn front(entries: Vec<Entry>) -> Front {
        Front::new(
            Library { entries },
            Catalogue {
                mods: Vec::new(),
                problem: None,
            },
        )
    }

    #[test]
    fn the_new_world_box_starts_on_a_name_nothing_is_using() {
        let screen = front(vec![world("New World", &[])]);
        assert_eq!(screen.name, "New World 2");
    }

    #[test]
    fn a_scan_problem_is_the_first_thing_the_screen_says() {
        let screen = Front::new(
            Library::default(),
            Catalogue {
                mods: Vec::new(),
                problem: Some("game/broken has no init.lua".to_owned()),
            },
        );
        assert!(
            screen.notice.is_some(),
            "a mod directory that would not scan must not look like no mods installed"
        );
    }

    #[test]
    fn a_setting_changed_asks_to_be_saved_exactly_once() {
        // **Written when it changes, not when the screen closes**, because a
        // front screen has no close: a player presses Play. The flag is
        // one-shot so a held slider writes the file once per change rather than
        // once per frame.
        let mut screen = front(Vec::new());
        assert!(!screen.take_settings_dirty(), "nothing has changed yet");
        screen.settings_dirty = true;
        assert!(screen.take_settings_dirty());
        assert!(
            !screen.take_settings_dirty(),
            "the same change asked to be saved twice"
        );
    }

    #[test]
    fn selecting_by_name_finds_the_world_just_created() {
        let mut screen = front(vec![world("First", &[]), world("Second", &[])]);
        screen.select("Second");
        assert_eq!(screen.selected, Some(1));
        screen.select("Nothing");
        assert_eq!(screen.selected, None, "a name nothing has selects nothing");
    }
}
