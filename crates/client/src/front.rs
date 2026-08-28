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

use crate::discovery::Discovery;
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
    /// Keep this entry in the list. A server somebody typed an address for.
    ///
    /// Same reason as [`Action::Forget`]: a server added to this screen's copy
    /// and never played was forgotten again the moment the screen was rebuilt.
    Remember(Box<Entry>),
    /// Drop this entry from the list, by name. Its files are not touched.
    ///
    /// **Carried out by the window rather than here**, because this screen is
    /// given a COPY of the library and the window owns the one that gets
    /// written. A removal that only happened here lasted until the screen was
    /// rebuilt — which is what leaving a world does.
    Forget(String),
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
    /// Whether a mod was ticked or unticked, so `mods.toml` wants writing and
    /// the window wants told.
    catalogue_dirty: bool,
    /// Where the interface-scale slider has been dragged to and not let go of.
    /// See [`crate::widget::settle`].
    scale_draft: Option<f32>,
    /// Worlds heard on the local network, when the port could be opened.
    ///
    /// **Started with the screen and dropped with it**, so nothing listens
    /// while a world is being played — a client in a world is not looking for
    /// one, and a socket held open for the session is a socket to explain.
    network: Option<Discovery>,
    /// Whether opening a local world should also listen for other machines.
    ///
    /// **Off by default, and deliberately.** A world that quietly accepted
    /// connections from the network because somebody once ticked a box is not
    /// something to remember for them — this is a per-session choice, made
    /// beside the button that acts on it.
    pub host_on_lan: bool,
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
            network: Discovery::start(),
            settings_dirty: false,
            catalogue_dirty: false,
            scale_draft: None,
            host_on_lan: false,
        }
    }

    /// Draws a frame and returns what the player asked for.
    ///
    /// `config` is edited in place — the settings tab writes straight to it and
    /// the window saves when [`Front::settings_dirty`] says so.
    pub fn draw(&mut self, ctx: &egui::Context, config: &mut crate::config::Config) -> Action {
        const TABS: [(Tab, &str); 3] = [
            (Tab::Play, "Play"),
            (Tab::Mods, "Mods"),
            (Tab::Settings, "Settings"),
        ];

        let mut action = Action::None;
        // **The same sheet as every in-game screen**, so a page cannot decide
        // its own size and run off the edges — see `crate::panel::sheet`. The
        // tabs are this screen's own row under the shared bar, because it is
        // the one screen with nowhere to go Back to.
        crate::panel::sheet(ctx, "Tiamot", None, |ui| {
            // **Quit above the tabs, not beside them.** The strip draws the
            // page edge across the whole sheet — that line under the inactive
            // tabs and around the active one is what makes them tabs — so it
            // needs the full width, and a button sharing the row would either
            // be pushed off it or cut the line short.
            //
            // **The `horizontal` is what gives it a row.** `with_layout` on
            // its own claims the whole REMAINING height of a top-down `Ui` and
            // centres its contents in it, so the button came out floating two
            // hundred points above everything else — reported from the window
            // as "the quit button is way up above the tabs randomly".
            ui.horizontal(|ui| {
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.button("Quit").clicked() {
                        action = Action::Quit;
                    }
                });
            });

            let active = TABS
                .iter()
                .position(|(tab, _)| *tab == self.tab)
                .unwrap_or(0);
            let labels: Vec<&str> = TABS.iter().map(|(_, label)| *label).collect();
            if let Some(picked) = crate::widget::tabs(ui, active, &labels) {
                self.tab = TABS[picked].0;
            }
            ui.add_space(6.0);

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

    /// Drops the highlighted entry from the list.
    ///
    /// **The list only.** Deleting somebody's world from a menu they were
    /// browsing is not a thing a button should be able to do.
    ///
    /// Lifted out of the button so it can be tested: what went wrong here was
    /// never the removal, it was that the removal stopped at this screen's copy
    /// of the library, and the emitted [`Action`] is the half that fixes it.
    fn forget_selected(&mut self) -> Action {
        let Some(index) = self
            .selected
            .filter(|index| *index < self.library.entries.len())
        else {
            return Action::None;
        };
        let gone = self.library.entries.remove(index);
        self.selected = None;
        Action::Forget(gone.name)
    }

    /// Adds the server whose address is typed into the box.
    fn remember_typed(&mut self) -> Action {
        let address = self.address.trim().to_owned();
        if address.is_empty() {
            self.notice = Some("a server needs an address, like example.com:4433".into());
            return Action::None;
        }
        let entry = Entry {
            name: self.library.unused_name(&address),
            kind: Kind::Remote { address },
            mods: Vec::new(),
            // A server nobody has played yet, stamped so it sorts to the top:
            // it is the line the player just typed and is about to click, not
            // the oldest thing they own.
            last_played: crate::launcher::now_seconds(),
        };
        self.library.add(entry.clone());
        self.address.clear();
        // **By name, not by length.** The list is ordered by when things were
        // last played, so the entry just added is not necessarily the last one.
        self.selected = self
            .library
            .entries
            .iter()
            .position(|existing| existing.name == entry.name);
        Action::Remember(Box::new(entry))
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
            if ui.button("Forget").clicked() {
                action = self.forget_selected();
            }
            // **Only for a world this machine runs.** Joining somebody else's
            // server is not hosting one, and a tick box that did nothing on
            // half the list would be a control that lies.
            let local = self
                .selected
                .and_then(|index| self.library.entries.get(index))
                .is_some_and(Entry::is_local);
            ui.add_enabled_ui(local, |ui| {
                ui.checkbox(&mut self.host_on_lan, "Open to LAN")
                    .on_hover_text(
                        "Others on your network can join by typing this machine's address. \
                         Off means the world is reachable only from this computer.",
                    );
            });
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
                action = self.remember_typed();
            }
        });

        self.network_list(ui).unwrap_or(action)
    }

    /// The mod list: a box each, and a box at the top for all of them.
    fn mods_tab(&mut self, ui: &mut egui::Ui) {
        let mut changed = false;
        let mut all = self.catalogue.all_on();
        if ui.checkbox(&mut all, "Everything").changed() {
            self.catalogue.set_all(all);
            changed = true;
        }
        ui.separator();
        {
            if self.catalogue.mods.is_empty() {
                ui.label("No mods installed. A client with none can still join servers.");
            }
            for listing in &mut self.catalogue.mods {
                ui.horizontal(|ui| {
                    changed |= ui.checkbox(&mut listing.enabled, &listing.name).changed();
                    ui.weak(&listing.id);
                });
                if !listing.description.is_empty() {
                    ui.indent(&listing.id, |ui| ui.weak(&listing.description));
                }
            }
        }
        // Reported from the window: unticking a mod did nothing — the world
        // still had it. The screen holds its OWN catalogue and the window
        // starts worlds from the one it kept, so a tick had to be carried back
        // across; see `Front::take_catalogue_dirty`.
        self.catalogue_dirty |= changed;
    }

    /// Worlds heard on the local network, and a way into one.
    ///
    /// Returns an action only when one was clicked, so the caller keeps
    /// whatever it already had. Its own function because `play_tab` is long
    /// enough without it, and because this half has nothing to do with the
    /// library the rest of that tab is about.
    fn network_list(&mut self, ui: &mut egui::Ui) -> Option<Action> {
        let mut action = None;
        // **Nobody types an address to join the machine next to them.** The
        // report this is here for: "I don't want kids to have to type in a LAN
        // server address." A world someone on this network has opened puts
        // itself on this list; joining is one click and the ordinary join.
        ui.add_space(10.0);
        ui.separator();
        ui.horizontal(|ui| {
            ui.label("On your network");
            match &self.network {
                Some(_) => ui.weak("listening"),
                None => ui.weak("not listening — another program has the port"),
            };
        });
        let worlds = self
            .network
            .as_ref()
            .map(Discovery::worlds)
            .unwrap_or_default();
        if worlds.is_empty() {
            ui.weak("Nothing found yet. A world has to be opened to the LAN to appear here.");
        }
        for world in worlds {
            ui.horizontal(|ui| {
                // A name from another machine, drawn as ONE label so it cannot
                // be mistaken for the interface around it, and already filtered
                // of anything that could rewrite the line (`discover::decode`).
                let label = format!(
                    "🖧  {}  —  {}/{} players",
                    world.name, world.players, world.max_players
                );
                if world.compatible {
                    if ui.button(label).clicked() {
                        action = Some(Action::Open(Entry {
                            name: world.name.clone(),
                            kind: Kind::Remote {
                                address: world.address.to_string(),
                            },
                            // A server somebody else runs decides its own mod
                            // set and says so at join; this list is not a
                            // claim about it.
                            mods: Vec::new(),
                            last_played: crate::launcher::now_seconds(),
                        }));
                    }
                } else {
                    // Shown and refused rather than hidden: "that world is a
                    // different version" is an answer, and a world missing
                    // from the list is a mystery.
                    ui.add_enabled(false, egui::Button::new(label));
                    ui.weak("different version");
                }
            });
        }
        action
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

    /// Whether the mod selection changed since this was last asked.
    ///
    /// **The screen's catalogue is not the window's.** The window builds this
    /// screen from a clone, because the screen outlives no world and the window
    /// outlives every one of them — so a tick here has to be handed back, and a
    /// window that never asked was starting worlds with whatever was ticked
    /// when the client launched.
    pub const fn take_catalogue_dirty(&mut self) -> bool {
        std::mem::replace(&mut self.catalogue_dirty, false)
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
            last_played: 0,
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

    fn listing(id: &str) -> crate::launcher::Listing {
        crate::launcher::Listing {
            id: id.to_owned(),
            name: id.to_owned(),
            description: String::new(),
            enabled: true,
        }
    }

    /// Draws one frame, optionally clicking at `at` first.
    ///
    /// A real `egui` pass rather than a call to the tab function, because the
    /// thing being tested is a checkbox reporting a change — which only a
    /// widget that was actually clicked does.
    fn frame(screen: &mut Front, ctx: &egui::Context, events: Vec<egui::Event>) -> Action {
        // egui is built without `default_fonts`, so a bare context has no
        // glyphs and every label lays out to nothing — which is a page with no
        // widgets on it to click.
        crate::app::install_fonts(ctx);
        let input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(1280.0, 720.0),
            )),
            events,
            ..Default::default()
        };
        let mut config = crate::config::Config::default();
        let mut acted = Action::None;
        let _ = ctx.run_ui(input, |_| {
            let context = ctx.clone();
            acted = screen.draw(&context, &mut config);
        });
        acted
    }

    /// Clicks at `at`, across the two frames egui needs to see one.
    ///
    /// A press and a release in a single pass is not a click: the widget has
    /// to be drawn under the pointer while the button is down before the
    /// release can land on it.
    fn click(screen: &mut Front, ctx: &egui::Context, at: egui::Pos2) -> Action {
        let button = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::default(),
        };
        frame(screen, ctx, vec![egui::Event::PointerMoved(at)]);
        frame(
            screen,
            ctx,
            vec![egui::Event::PointerMoved(at), button(true)],
        );
        let acted = frame(screen, ctx, vec![button(false)]);
        frame(screen, ctx, Vec::new());
        acted
    }

    #[test]
    fn opening_to_the_lan_is_off_until_somebody_asks_for_it() {
        // **A world reachable from the network is a decision, not a default.**
        // Off every time the screen is built, and deliberately not remembered:
        // a box ticked once should not quietly open every world afterwards.
        let screen = front(vec![world("Home", &[])]);
        assert!(!screen.host_on_lan);
    }

    #[test]
    fn forgetting_a_world_asks_the_window_to_write_the_list() {
        // **The bug this is here for:** the removal used to happen only in this
        // screen's copy of the library, so it came back on the next run — and
        // on leaving a world, which rebuilds the screen from the window's copy.
        let mut screen = front(vec![world("Home", &[]), world("Other", &[])]);
        screen.selected = Some(0);
        let action = screen.forget_selected();
        assert_eq!(action, Action::Forget("Home".to_owned()));
        assert_eq!(screen.library.entries.len(), 1, "gone from the screen too");
        assert_eq!(screen.selected, None);
    }

    #[test]
    fn forgetting_with_nothing_selected_asks_for_nothing() {
        let mut screen = front(vec![world("Home", &[])]);
        screen.selected = None;
        assert_eq!(screen.forget_selected(), Action::None);
        assert_eq!(screen.library.entries.len(), 1);
    }

    #[test]
    fn a_server_typed_in_is_handed_to_the_window_to_keep() {
        // Same fault as Forget, the other way round: a server added and not
        // played was lost, because only playing one ever reached the library
        // the window writes.
        let mut screen = front(Vec::new());
        screen.address = "  example.com:4433  ".to_owned();
        let action = screen.remember_typed();
        let Action::Remember(entry) = action else {
            panic!("a typed address must reach the window");
        };
        assert_eq!(
            entry.kind,
            Kind::Remote {
                address: "example.com:4433".to_owned()
            },
            "trimmed, because a pasted address brings whitespace with it"
        );
        assert_eq!(screen.library.entries.len(), 1);
        assert!(screen.address.is_empty(), "the box is cleared");
        assert_eq!(screen.selected, Some(0));
    }

    #[test]
    fn a_server_just_added_is_the_one_highlighted() {
        // **Selection is by name, not by length.** The list is ordered by when
        // things were last played, so the entry just added is not the last one
        // whenever anything else was played more recently... which cannot
        // happen with a fresh stamp, so the case that bites is the reverse: an
        // existing entry with a LATER stamp than the clock, from a file copied
        // between machines. Either way, position beats arithmetic.
        let mut screen = front(vec![Entry {
            last_played: u64::MAX,
            ..world("From the future", &[])
        }]);
        screen.address = "example.com:4433".to_owned();
        screen.remember_typed();
        let selected = screen.selected.expect("something is highlighted");
        assert_eq!(
            screen.library.entries[selected].name, "example.com:4433",
            "the highlight followed the entry rather than the end of the list"
        );
    }

    #[test]
    fn an_empty_address_is_said_out_loud_rather_than_added() {
        let mut screen = front(Vec::new());
        screen.address = "   ".to_owned();
        assert_eq!(screen.remember_typed(), Action::None);
        assert!(screen.library.entries.is_empty());
        assert!(screen.notice.is_some());
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
    fn unticking_a_mod_is_handed_back_to_the_window() {
        // **Reported from the window**: unticking a core mod left it in the
        // world, and unticking everything left everything. The screen edits its
        // OWN catalogue — the window builds it from a clone — so a tick that is
        // not carried back reaches nothing that starts a world.
        //
        // Clicked for real, because the bug is downstream of a checkbox saying
        // it changed, and a test that sets the field by hand skips the part
        // that was missing.
        let ctx = egui::Context::default();
        let mut screen = front(Vec::new());
        screen.catalogue.mods = vec![listing("core_worldgen")];
        screen.tab = Tab::Mods;
        frame(&mut screen, &ctx, Vec::new());
        assert!(
            !screen.take_catalogue_dirty(),
            "nothing has been clicked yet"
        );

        // The box is found by sweeping the page rather than by computing where
        // egui put it: the test is about there being a box that turns the mod
        // off and hands the change back, not about exact spacing. The WHOLE
        // sheet, because a narrower window is a guess about the layout that
        // goes stale the first time anything above the list changes height —
        // which is exactly what fixing the Quit button's row did.
        let mut at = None;
        'sweep: for y in (0..720).step_by(4) {
            for x in (200..1100).step_by(4) {
                let point = egui::pos2(x as f32, y as f32);
                click(&mut screen, &ctx, point);
                if screen.enabled_mods().is_empty() {
                    at = Some(point);
                    break 'sweep;
                }
                // A click that changed page is not one that says anything
                // about this one; go back and carry on.
                screen.tab = Tab::Mods;
            }
        }
        let at = at.expect("no click anywhere on the mods page turned the mod off");
        assert!(
            screen.take_catalogue_dirty(),
            "the mod was turned off at {at:?} without asking the window to notice"
        );
        assert!(
            !screen.take_catalogue_dirty(),
            "the same change was handed back twice"
        );
    }

    #[test]
    fn quit_sits_on_the_row_above_the_tabs() {
        // **Reported from the window**: "the quit button on the main menu is
        // way up above the tabs randomly."
        //
        // `with_layout` in a top-down `Ui` claims the whole REMAINING height
        // and centres its contents in it, so a right-aligned button on its own
        // came out floating half a sheet away from everything else. Wrapping it
        // in a row is what gives it a row's height.
        //
        // Found by sweeping rather than computed, because the assertion is
        // about where the two end up and not about which egui calls were made.
        let ctx = egui::Context::default();
        let mut screen = front(vec![world("Home", &[])]);
        frame(&mut screen, &ctx, Vec::new());

        let mut quit = None;
        let mut strip = None;
        'sweep: for y in (0..720).step_by(4) {
            for x in (200..1100).step_by(6) {
                let point = egui::pos2(x as f32, y as f32);
                screen.tab = Tab::Mods;
                let acted = click(&mut screen, &ctx, point);
                if quit.is_none() && matches!(acted, Action::Quit) {
                    quit = Some(point);
                }
                if strip.is_none() && screen.tab != Tab::Mods {
                    strip = Some(point);
                }
                if quit.is_some() && strip.is_some() {
                    break 'sweep;
                }
            }
        }
        let quit = quit.expect("no click anywhere on the screen quit");
        let strip = strip.expect("no click anywhere on the screen changed tab");
        assert!(
            (quit.y - strip.y).abs() < 60.0,
            "Quit is at {quit:?} and the tab strip is at {strip:?} — {:.0} points apart, \
             which reads as a button floating somewhere of its own",
            (quit.y - strip.y).abs()
        );
    }

    #[test]
    fn a_click_on_nothing_leaves_the_mod_set_alone() {
        // The counter-example the sweep above needs: if any click anywhere
        // marked the set changed, finding one that turned a mod off would
        // prove nothing.
        let ctx = egui::Context::default();
        let mut screen = front(Vec::new());
        screen.catalogue.mods = vec![listing("core_worldgen")];
        screen.tab = Tab::Mods;
        frame(&mut screen, &ctx, Vec::new());
        click(&mut screen, &ctx, egui::pos2(1270.0, 710.0));
        assert_eq!(screen.enabled_mods(), vec!["core_worldgen".to_owned()]);
        assert!(
            !screen.take_catalogue_dirty(),
            "a click on empty space asked the window to save the mod set"
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
