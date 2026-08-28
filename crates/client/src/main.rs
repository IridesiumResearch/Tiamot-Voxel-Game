// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! The Tiamot client: a window onto a running server.
//!
//! # Singleplayer starts a server
//!
//! Charter rule 2, literally. `server = "embedded"` in `client.toml` starts a
//! real [`ServerHandle`] in this process and connects to it over loopback —
//! the same `ServerHandle::start` the standalone binary calls, the same QUIC
//! transport, the same join flow. There is no singleplayer code path, because
//! a second path is a second set of bugs that only appear in one mode.
//!
//! # What this file is and is not
//!
//! It is the window: winit events in, [`Input`] out, and a surface for the
//! renderer to draw into. Everything between frames lives in [`client::app`],
//! which knows nothing about windows and can therefore be tested without one.

use std::sync::Arc;

use client::app::{App, Input, Phases, Teleport};
use client::cache::ContentCache;
use client::config::{Config, ServerChoice};
use client::input::{Bindings, Input as Control};
use client::net::Connection;
use client::render::{Gpu, Renderer};
use tiamot_core::identity::Identity;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{CursorGrabMode, Window, WindowId};

/// The config file, relative to the working directory.
const CONFIG_FILE: &str = "client.toml";

/// Where mods are installed, beside the executable.
///
/// The same directory a dedicated server is pointed at, so a world a player
/// makes here and a world they host run the same content.
const MODS_DIR: &str = "game";

/// The address to dial a server this process just started.
///
/// A listener bound to the unspecified address reports it back, and the
/// unspecified address is not a destination: QUIC refuses it as an invalid
/// remote. The server is on this machine either way, so loopback is both
/// correct and the shortest path to it.
fn own_address(listening: std::net::SocketAddr) -> std::net::SocketAddr {
    if listening.ip().is_unspecified() {
        std::net::SocketAddr::from(([127, 0, 0, 1], listening.port()))
    } else {
        listening
    }
}

/// This machine's address on the local network, if it has one.
///
/// **Asked of the routing table rather than of a name server.** A UDP socket
/// that is "connected" to an address on the local network sends nothing — it
/// only decides which interface it would use — and its own local address is
/// then the one another machine on that network would reach this one at.
/// `hostname` lookups answer with loopback on plenty of machines, which is the
/// one answer that is never useful here.
///
/// `None` when there is no route out, which is what a machine with no network
/// looks like.
fn lan_address(port: u16) -> Option<String> {
    let probe = std::net::UdpSocket::bind(("0.0.0.0", 0)).ok()?;
    // Any address on a private range will do: nothing is sent to it.
    probe.connect(("192.168.1.1", 80)).ok()?;
    let local = probe.local_addr().ok()?;
    Some(format!("{}:{port}", local.ip()))
}

/// The port a world opened to the LAN listens on.
///
/// The same one the dedicated server defaults to, so there is one number to
/// remember and one to open on a firewall. Fixed rather than chosen at random
/// because the whole point is that somebody else can type it.
const LAN_PORT: u16 = 47811;

/// Where key bindings live, beside the config.
///
/// Its own file rather than a section of `client.toml`: the settings screen
/// rewrites it whenever a player rebinds something, and rewriting a file that
/// also holds hand-edited server details would lose their comments.
const BINDINGS_FILE: &str = "bindings.toml";

/// Starting window size.
const DEFAULT_SIZE: (u32, u32) = (1280, 720);

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    match run() {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            // Printed as well as logged: a player who ran this from a desktop
            // launcher never sees the log, and "it closed immediately" is not
            // a bug report anyone can act on.
            eprintln!("tiamot client: {err}");
            let mut source = std::error::Error::source(&*err);
            while let Some(cause) = source {
                eprintln!("  caused by: {cause}");
                source = std::error::Error::source(cause);
            }
            std::process::ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let config = Config::load_or_default(std::path::Path::new(CONFIG_FILE))?;
    let bindings = Bindings::load_or_default(std::path::Path::new(BINDINGS_FILE))?;
    let data = config.data_dir();
    std::fs::create_dir_all(&data)?;

    // The identity is created on first run and never leaves this machine
    // unless the player asks for its recovery phrase (charter rule 13).
    let identity = Identity::load_or_create(&data.join("identity.key"))?;
    tracing::info!(uuid = %identity.uuid_as_root().short(), "identity ready");

    let library =
        client::launcher::Library::load(&data.join("worlds.toml")).unwrap_or_else(|err| {
            // A world list that will not parse is reported and then left alone:
            // starting with an empty one would save over it the next time
            // anything was added.
            tracing::warn!("{err}");
            client::launcher::Library::default()
        });
    let catalogue =
        client::launcher::Catalogue::scan(std::path::Path::new(MODS_DIR), &data.join("mods.toml"));

    // **The world somebody already had, before there was a list to put it in.**
    // Without this the first run after the front screen landed would show an
    // empty list beside a `singleplayer/` directory full of a player's
    // building, and the only way forward would be to make a second world.
    let mut library = library;
    // **`Library::exists`, not `entries.is_empty()`.** An emptied list and a
    // missing one both have no entries and mean opposite things: this is for
    // somebody who has never had a list, and asking about the length instead
    // resurrected the world they had just pressed Forget on, every launch.
    if !client::launcher::Library::exists(&data.join("worlds.toml")) && config.world_path.is_dir() {
        library.add(client::launcher::Entry {
            name: config.world_path.file_name().map_or_else(
                || "Singleplayer".to_owned(),
                |name| name.to_string_lossy().into_owned(),
            ),
            kind: client::launcher::Kind::Local {
                path: config.world_path.clone(),
            },
            // Whatever it was played with was not recorded, so it is recorded
            // as what is on now: inventing a set would invent a warning.
            mods: catalogue.enabled(),
            last_played: client::launcher::now_seconds(),
        });
        if let Err(err) = library.save(&data.join("worlds.toml")) {
            tracing::warn!("{err}");
        }
    }

    // **The front screen is the default, and `menu = false` is the way past
    // it.** A client that dialled a server before opening a window could only
    // ever have one world in it, because there was nowhere to name a second.
    // The old behaviour is still exactly one line of config away, because that
    // is what the bot harness and a dedicated test rig want.
    let (connection, embedded) = if config.menu {
        (None, None)
    } else {
        // An embedded server is started BEFORE the window, so a world that
        // fails to open is a clear error on the terminal rather than a window
        // that appears and then vanishes.
        let (address, embedded) = match config.server {
            ServerChoice::Remote(address) => (address, None),
            ServerChoice::Embedded => {
                let handle = start_local_world(
                    &config.world_path,
                    config.view(),
                    catalogue.enabled(),
                    &identity.uuid_as_root(),
                    // `menu = false` skips the front screen, so there is
                    // nowhere to have ticked the box. A config-driven start is
                    // the bot harness and a test rig, which want loopback.
                    false,
                )?;
                tracing::info!(addr = %handle.local_addr(), "embedded server listening");
                (handle.local_addr(), Some(handle))
            }
        };
        let connection = Connection::open(
            address,
            identity,
            config.display_name.clone(),
            ContentCache::open(&data.join("content"))?,
            &data.join("known-hosts"),
        )?;
        (Some(connection), embedded)
    };

    let event_loop = EventLoop::new()?;
    // Poll rather than Wait: this is a game, and a frame is due whether or not
    // an input event arrived.
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut client = Client {
        config,
        library,
        catalogue,
        identity_path: data.join("identity.key"),
        data,
        parked: None,
        present_mode: "",
        connection,
        embedded,
        beacon: None,
        window: None,
        held: Held::default(),
        bindings: Some(bindings),
        last_frame: std::time::Instant::now(),
        pending_teleport: None,
        grabbed: false,
        released_for_dialog: false,
        digging: false,
        error: None,
    };
    event_loop.run_app(&mut client)?;

    client.error.map_or(Ok(()), Err)
}

/// What the window is showing.
///
/// **A window before a world.** The client used to read `client.toml`, dial a
/// server and open a window already in it — fine for one world and impossible
/// for two, because there was nowhere to name a second one and no way to see
/// which servers had been visited. So the window opens on the front screen and
/// the world is built when the player presses Play.
enum Stage {
    /// The front screen: worlds, mods, and Play.
    Front(Box<client::front::Front>),
    /// In a world.
    Playing(Box<App>),
}

/// The window, the surface, and everything drawn into it.
struct Surface {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    /// **Kept here rather than reached through the renderer**, because the
    /// surface has to be configured and the front screen drawn before there is
    /// a renderer to ask. A clone is a second handle, not a second device.
    gpu: client::render::Gpu,
    stage: Stage,
    egui: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
    /// Local state for the server's dialogs — what is half-typed into a field,
    /// which is the client's business and not the server's.
    dialogs: client::dialog::Dialogs,
    /// The world's texture atlas, as egui knows it.
    ///
    /// `None` until the server sends its material table, which is after the
    /// window exists — so this cannot be built at startup, and every slot
    /// drawn before it arrives falls back to a tint. Registered once per
    /// atlas: see [`App::take_atlas_change`].
    atlas_texture: Option<egui::TextureId>,
    size: (u32, u32),
}

/// Keys currently down, as intents rather than key codes.
#[derive(Debug, Default, Clone, Copy)]
struct Held {
    forward: bool,
    back: bool,
    left: bool,
    right: bool,
    up: bool,
    down: bool,
    sprint: bool,
    /// Mouse movement accumulated since the last frame.
    look: (f32, f32),
}

impl Held {
    fn as_input(&mut self, teleport: Option<Teleport>) -> Input {
        let axis = |positive: bool, negative: bool| f32::from(positive) - f32::from(negative);
        let input = Input {
            forward: axis(self.forward, self.back),
            right: axis(self.right, self.left),
            up: axis(self.up, self.down),
            look: self.look,
            sprint: self.sprint,
            // The same keys serve both movement modes: before the join there is
            // no body, so space and shift fly the camera; afterwards they jump
            // and sneak. One binding either way, which is what a player
            // expects — see `App::advance`.
            sneak: self.down,
            jump: self.up,
            // Set by the App from what the server granted, not from a key —
            // see `App::advance`. The key only asks.
            fly: false,
            teleport,
        };
        // Mouse movement is a delta, so it is consumed rather than held: a
        // frame that read it twice would turn the camera twice as fast at low
        // frame rates, which is the worst time for it.
        self.look = (0.0, 0.0);
        input
    }
}

/// Where a NEW world's files should go.
///
/// The name a player typed, made safe by [`world_directory`], and then made
/// unique against what is already on disk.
fn unused_world_directory(data: &std::path::Path, name: &str) -> std::path::PathBuf {
    let worlds = std::path::Path::new("worlds");
    let slug = world_directory(name);
    let free = |candidate: &std::path::Path| !data.join(candidate).exists();

    let first = worlds.join(&slug);
    if free(&first) {
        return first;
    }
    // **Checked against the DISK, not against the list.** Forgetting a world
    // takes it out of the list and deliberately leaves its files alone, so a
    // name that is free in the list can still have a save sitting under it —
    // and a new world pointed at that directory opens the old one. Reported
    // from the window as a fresh "New world" arriving with all the previous
    // one's building in it.
    //
    // Bounded rather than looped for ever: a thousand worlds of one name is
    // somebody's directory being unwritable, and the last candidate is used
    // whatever it holds, because refusing to make a world at all is worse.
    for suffix in 2..1000 {
        let candidate = worlds.join(format!("{slug}-{suffix}"));
        if free(&candidate) {
            return candidate;
        }
    }
    first
}

/// A directory name a world's title can safely become.
///
/// **Not the title itself.** A player may call a world anything, including
/// something with a `/` or a `..` in it, and that string must never reach a
/// path. Anything but letters, digits, dash and underscore becomes a dash, and
/// an empty result becomes `world` — a directory named for nothing is still a
/// directory that opens.
fn world_directory(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();
    let trimmed = cleaned.trim_matches('-').to_owned();
    if trimmed.is_empty() {
        "world".to_owned()
    } else {
        trimmed
    }
}

/// Paints the whole frame one colour.
///
/// The front screen has no world behind it, and the interface pass loads rather
/// than clears — so without this a menu is drawn over whatever the driver left
/// in the buffer.
fn clear(gpu: &Gpu, view: &wgpu::TextureView) {
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("front"),
        });
    drop(encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
        label: Some("front"),
        color_attachments: &[Some(wgpu::RenderPassColorAttachment {
            view,
            depth_slice: None,
            resolve_target: None,
            ops: wgpu::Operations {
                load: wgpu::LoadOp::Clear(wgpu::Color {
                    r: 0.05,
                    g: 0.06,
                    b: 0.08,
                    a: 1.0,
                }),
                store: wgpu::StoreOp::Store,
            },
        })],
        depth_stencil_attachment: None,
        timestamp_writes: None,
        occlusion_query_set: None,
        multiview_mask: None,
    }));
    gpu.queue.submit(Some(encoder.finish()));
}

/// Starts an embedded server for one of this player's own worlds.
///
/// **Loopback only.** An embedded server is this player's world, and binding it
/// to every interface would silently publish it.
fn start_local_world(
    world_path: &std::path::Path,
    view: tiamot_core::interest::ViewDistance,
    enabled_mods: Vec<String>,
    operator: &tiamot_core::identity::PlayerUuid,
    lan: bool,
) -> Result<tiamot_server::ServerHandle, Box<dyn std::error::Error>> {
    Ok(tiamot_server::ServerHandle::start(
        &tiamot_server::Settings {
            // **Loopback unless asked otherwise.** A world hosted for one
            // person should not be reachable from the network because it
            // happens to be running; opening it is a decision somebody makes
            // beside the button, per session.
            //
            // `LAN_PORT` rather than a random one when open, because the
            // address has to be typeable by somebody standing next to you.
            bind_addr: if lan {
                std::net::SocketAddr::from(([0, 0, 0, 0], LAN_PORT))
            } else {
                "127.0.0.1:0".parse()?
            },
            world_path: world_path.to_path_buf(),
            // One is right for a world nobody else can reach, and would be a
            // baffling refusal for one they can.
            max_players: if lan { 8 } else { 1 },
            allowlist: tiamot_core::identity::Allowlist::open(),
            // **Your own world, your own powers.** A player hosting a world for
            // themselves is its operator, which is what makes flight available
            // for testing without a command to type. A world somebody ELSE
            // hosts decides for itself, and says so at join.
            operators: vec![operator.to_hex()],
            view_distance: view,
            mods_path: Some(std::path::PathBuf::from(MODS_DIR)),
            enabled_mods: Some(enabled_mods),
            seed: None,
            rcon: None,
            materials: Vec::new(),
        },
    )?)
}

/// The renderer, built with the window and waiting for a world.
struct Parked {
    renderer: Renderer,
    present_mode: &'static str,
    /// What its pipelines were built for.
    ///
    /// Kept because the front screen can change the draw mode after the
    /// renderer exists, and that one setting cannot be pushed into it — the
    /// mode selects the pipelines, so a change means building a new one.
    mode: client::config::RenderMode,
}

struct Client {
    config: Config,
    /// The player's worlds and servers, as the front screen shows them.
    library: client::launcher::Library,
    /// Every installed mod, and which are ticked.
    catalogue: client::launcher::Catalogue,
    /// Where the identity and the content cache live.
    data: std::path::PathBuf,
    /// Where this machine's identity is kept.
    ///
    /// **The path rather than the key.** A player who leaves one world and
    /// opens another is the same player (charter rule 13), so each connection
    /// loads the same file — and a key that had been copied around the process
    /// is a secret in more places than it needs to be.
    identity_path: std::path::PathBuf,
    /// The renderer, from window creation until a world takes it.
    parked: Option<Parked>,
    /// The present mode the surface actually got, for the next world's HUD.
    present_mode: &'static str,
    /// Taken when the window is created.
    connection: Option<Connection>,
    /// Kept alive for as long as the client runs. Dropping it stops the world.
    embedded: Option<tiamot_server::ServerHandle>,
    /// Saying this world is here, while it is open to the network.
    ///
    /// Dropped when the world is left, which stops the beacon — a world nobody
    /// is hosting must not still be advertised.
    beacon: Option<tiamot_server::announce::Announcer>,
    window: Option<Surface>,
    held: Held,
    /// The player's saved bindings, until the `App` exists to hold them.
    ///
    /// Read from disk before a window is created and handed over at
    /// construction, because the registry lives on the `App` — that is where a
    /// server's `ActionTable` lands.
    bindings: Option<Bindings>,
    last_frame: std::time::Instant,
    pending_teleport: Option<Teleport>,
    grabbed: bool,
    /// Whether the cursor is free because a server dialog is open.
    ///
    /// Tracked rather than recomputed so that a player who pressed Escape while
    /// a dialog was up does not have the cursor taken back off them when the
    /// dialog closes. The window releases for a dialog and takes back only what
    /// it released.
    released_for_dialog: bool,
    /// Whether the dig button is down.
    ///
    /// Held rather than clicked: a dig is counted in ticks by the server, so
    /// the client re-aims at the crosshair every frame the button is down and
    /// cancels when it comes up.
    digging: bool,
    /// Set when something went wrong badly enough to stop, so `run` can report
    /// it rather than exiting zero after printing a log line.
    error: Option<Box<dyn std::error::Error>>,
}

impl ApplicationHandler for Client {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        match self.create_window(event_loop) {
            Ok(surface) => self.window = Some(surface),
            Err(err) => {
                self.error = Some(err);
                event_loop.exit();
            }
        }
    }

    fn device_event(&mut self, _: &ActiveEventLoop, _: DeviceId, event: DeviceEvent) {
        // Raw device motion rather than cursor position: a cursor stops at the
        // edge of the screen and a camera should not.
        if let DeviceEvent::MouseMotion { delta } = event
            && self.grabbed
        {
            self.held.look.0 += delta.0 as f32;
            self.held.look.1 += delta.1 as f32;
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        let Some(surface) = self.window.as_mut() else {
            return;
        };

        // egui gets first refusal. When it wants an event — a click on a
        // widget, a keystroke in a text field — the world must not also act on
        // it, or clicking a button also swings the camera.
        //
        // **But only while there is a pointer to point with.** A grabbed cursor
        // is invisible and locked to the middle of the window, so winit stops
        // reporting where it is and egui goes on believing it is wherever it
        // was last seen — which, now that a world is entered from a menu, is
        // over the Play button that started it. egui then claims every mouse
        // event for a widget nobody can see or reach, and the one that goes
        // missing is the button RELEASE: the client never learns the dig
        // stopped and keeps digging for as long as the world is open.
        //
        // Reported from the window as being stuck digging. The rule is the
        // honest one: with the cursor grabbed the interface is not being
        // pointed at, so it gets no say. Chat, dialogs and the menus all
        // release the cursor first, which is exactly when egui should have it.
        let response = surface.egui_state.on_window_event(&surface.window, &event);
        if response.consumed && !self.grabbed {
            return;
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                surface.size = (size.width.max(1), size.height.max(1));
                configure_surface(
                    &surface.surface,
                    &surface.gpu,
                    surface.size,
                    self.config.vsync,
                );
            }

            // **Nothing is held down when the window is not being looked at.**
            // A player who alt-tabs mid-dig gets no release event at all, and
            // the same stuck dig as the bug above. Cheap to be sure about.
            WindowEvent::Focused(false) => {
                if let Stage::Playing(app) = &mut surface.stage {
                    self.grabbed = hand_over(
                        &surface.window,
                        false,
                        app,
                        &mut self.held,
                        &mut self.digging,
                    );
                } else {
                    self.held = Held::default();
                    self.digging = false;
                    self.grabbed = grab(&surface.window, false);
                }
            }

            WindowEvent::RedrawRequested if !self.frame() => event_loop.exit(),

            // **Everything else is about a world**, and on the front screen
            // there is not one: no camera to swing, no action table to look a
            // key up in, and no cursor to grab. egui has already had its
            // refusal above, so the menu is fully usable and this is silent.
            event => self.world_event(event),
        }
    }

    fn about_to_wait(&mut self, _: &ActiveEventLoop) {
        if let Some(surface) = &self.window {
            surface.window.request_redraw();
        }
    }

    fn exiting(&mut self, _: &ActiveEventLoop) {
        // Order matters: leave the server before stopping it, or the last thing
        // in the log is a connection dropping rather than a clean goodbye.
        if let Some(surface) = self.window.take()
            && let Stage::Playing(app) = surface.stage
        {
            app.shutdown();
        }
        // **Before the world, every time.** A beacon outliving its server
        // advertises an address nothing answers on, and the client that dials
        // it gets a timeout rather than a reason.
        if let Some(beacon) = self.beacon.take() {
            beacon.stop();
        }
        if let Some(handle) = self.embedded.take() {
            handle.stop();
        }
    }
}

impl Client {
    /// Window events that only mean something once a world is running.
    fn world_event(&mut self, event: WindowEvent) {
        let Some(surface) = self.window.as_mut() else {
            return;
        };
        let Stage::Playing(app) = &mut surface.stage else {
            return;
        };
        let window = &surface.window;

        match event {
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                // **Click to look FIRST, and before any action lookup.** Until
                // the cursor is grabbed a click is the player asking for
                // mouse-look, not asking to dig a hole under an unaimed
                // crosshair — and that is a property of the window rather than
                // of whatever `engine:dig` happens to be bound to.
                if button == MouseButton::Left && pressed && !self.grabbed {
                    self.grabbed = grab(window, true);
                    return;
                }
                // The same rule for the mouse: a capture owns the button.
                if app.rebinding().is_some() {
                    if pressed {
                        app.capture(Control::Mouse(button));
                    }
                    return;
                }
                let Some(action) = app.action_for(Control::Mouse(button)) else {
                    return;
                };
                match action.as_str() {
                    // Held, not clicked: a dig takes a second or two of ticks
                    // and the server counts them. Releasing cancels, which is
                    // why the state is tracked rather than acted on once.
                    "engine:dig" => {
                        self.digging = pressed;
                        if !pressed {
                            app.stop_digging();
                        }
                    }
                    // A single action, unlike digging. Repeating while held
                    // would build a wall out of one click.
                    "engine:place" if pressed && self.grabbed => app.place(),
                    _ => {}
                }
            }

            // The hotbar, on the wheel as well as the number keys.
            WindowEvent::MouseWheel { delta, .. } => {
                let forward = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y < 0.0,
                    winit::event::MouseScrollDelta::PixelDelta(position) => position.y < 0.0,
                };
                app.select_next(forward);
            }

            WindowEvent::KeyboardInput { event, .. } => {
                let pressed = event.state == ElementState::Pressed;
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                // **The window no longer knows what a key means.** It asks the
                // registry which ACTION this position is bound to and acts on
                // that — charter rule 11, and the reason a mod can add a
                // control without the client learning a new key.
                // **While a capture is waiting, the key belongs to it.**
                if app.rebinding().is_some() {
                    if !pressed {
                        // The release of the key that was just bound, or of the
                        // one that opened the prompt. Neither is a binding.
                        return;
                    }
                    // Escape abandons the capture rather than binding itself.
                    // A player who opened the prompt by accident needs a way
                    // out that is not "bind something", and it is checked
                    // BEFORE the capture or it would bind Escape.
                    if code == winit::keyboard::KeyCode::Escape {
                        app.cancel_rebind();
                        return;
                    }
                    // Otherwise this key is the answer. Taken here rather than
                    // acted on, or rebinding would also fire whatever the key
                    // currently does — at best a jump, at worst the very thing
                    // being rebound away from.
                    app.capture(Control::Key(code));
                    return;
                }
                let Some(action) = app.action_for(Control::Key(code)) else {
                    return;
                };
                // **Typing is not walking.** Every action but the ones that
                // close chat is swallowed while the input line is open, so a
                // player writing "sssh" does not sneak-strafe across the map.
                if app.chat_open() && !matches!(action.as_str(), "engine:menu") {
                    return;
                }
                match action.as_str() {
                    "engine:move_forward" => self.held.forward = pressed,
                    "engine:move_back" => self.held.back = pressed,
                    "engine:move_left" => self.held.left = pressed,
                    "engine:move_right" => self.held.right = pressed,
                    "engine:jump" => self.held.up = pressed,
                    "engine:sneak" | "engine:sneak_alt" => self.held.down = pressed,
                    "engine:sprint" => self.held.sprint = pressed,
                    // **The shortcut still works, and lands in the same place
                    // Escape does.** F1 opens the menu WITH the controls page
                    // showing rather than the controls on their own, so closing
                    // them always leaves a player somewhere they can act — and
                    // Escape always gets them out, whichever key let them in.
                    "engine:settings" if pressed => {
                        let showing = app.settings_open();
                        app.set_menu_open(!showing);
                        if !showing {
                            app.open_settings();
                        }
                        // The cursor has to come back to click anything.
                        let wanted = wants_cursor(app.menu_open(), app.chat_open(), false);
                        self.grabbed =
                            hand_over(window, wanted, app, &mut self.held, &mut self.digging);
                    }
                    // Chat takes the cursor and the keyboard: a player typing
                    // "west" must not walk west while they do it. The window
                    // stops feeding movement to `held` while it is open — see
                    // where this is checked before the match.
                    "engine:chat" if pressed => {
                        app.set_chat_open(true);
                        self.grabbed = hand_over(
                            window,
                            wants_cursor(false, true, false),
                            app,
                            &mut self.held,
                            &mut self.digging,
                        );
                    }
                    "engine:debug_overlay" if pressed => {
                        let on = !app.debug_overlay();
                        app.set_debug_overlay(on);
                    }
                    // **Escape is the front door.** It used to only release the
                    // cursor, which left the settings screen reachable by one
                    // undocumented function key and the interface with no way
                    // in. Now it opens a menu — and closes chat, a dialog, or
                    // the menu itself, whichever is in the way.
                    //
                    // **Front to back, and one thing per press.** A dialog is
                    // in front of the menu, so Escape closes the inventory
                    // rather than pausing the game behind it — which is what it
                    // did, and it took two presses to get back to the world.
                    // Reported from the window.
                    "engine:menu" if pressed => {
                        if app.chat_open() {
                            app.set_chat_open(false);
                        } else if app.close_top_dialog() {
                            // Nothing else to do here. The dialog belongs to a
                            // mod and closes when the server agrees it has, and
                            // the cursor goes back to the camera on that
                            // transition — the same one that released it.
                            return;
                        } else {
                            let open = !app.menu_open();
                            app.set_menu_open(open);
                        }
                        let wanted = wants_cursor(app.menu_open(), app.chat_open(), false);
                        self.grabbed =
                            hand_over(window, wanted, app, &mut self.held, &mut self.digging);
                    }
                    // The floating-origin check from Task 08's criteria: out
                    // and home. The world travels with the camera, so a working
                    // floating origin shows an identical picture from fifty
                    // thousand blocks away — the HUD's position moves and the
                    // frame must not.
                    "engine:teleport_far" | "engine:teleport_far_alt" if pressed => {
                        self.pending_teleport = Some(Teleport::Far);
                    }
                    "engine:teleport_home" | "engine:teleport_home_alt" if pressed => {
                        self.pending_teleport = Some(Teleport::Home);
                    }
                    // A cycle rather than a key per tool, because the engine
                    // does not know how many there are — charter rule 1 puts
                    // that in the mods and a server could register twenty.
                    "engine:next_tool" if pressed => app.next_tool(),
                    "engine:offhand" if pressed => app.swap_offhand(),
                    "engine:lighting_mode" | "engine:lighting_mode_alt" if pressed => {
                        app.cycle_lighting_mode();
                    }
                    // Its own control rather than part of the lighting mode:
                    // the cascades are the largest thing the client allocates
                    // and the right setting depends entirely on the card.
                    "engine:shadow_quality" if pressed => app.cycle_shadow_quality(),
                    "engine:third_person" | "engine:third_person_alt" if pressed => {
                        app.toggle_third_person();
                    }
                    "engine:fly" if pressed => {
                        let on = app.toggle_fly();
                        if app.may_fly() {
                            tracing::info!(on, "flight");
                        } else {
                            tracing::info!("flight is for operators; this server said no");
                        }
                    }
                    "engine:chunk_borders" if pressed => {
                        let on = app.toggle_chunk_borders();
                        tracing::info!(on, "chunk borders");
                    }
                    // A twentieth of a day a press, so a full circuit is twenty
                    // presses and dawn is findable.
                    "engine:time_back" | "engine:time_back_alt" if pressed => {
                        app.nudge_time(-0.05);
                    }
                    "engine:time_forward" | "engine:time_forward_alt" if pressed => {
                        app.nudge_time(0.05);
                    }
                    "engine:time_resync" | "engine:time_resync_alt" if pressed => {
                        app.resync_time();
                    }
                    // Singleplayer only, and through the embedded server's own
                    // handle rather than over the wire — a client cannot edit a
                    // world it is a guest in, and this does not make it one.
                    "engine:material_row" if pressed => {
                        if let Some(server) = self.embedded.as_ref() {
                            for (pos, material) in app.debug_material_row() {
                                server.seed_block(pos, material);
                            }
                        }
                    }
                    id => {
                        if let Some(slot) = hotbar_slot(id).filter(|_| pressed) {
                            app.select_slot(slot);
                        } else {
                            // Anything else is a mod's, and the mod is told
                            // BOTH edges so it can implement a held control.
                            // `send_action` drops engine ids, so an unhandled
                            // arm above cannot leak onto the wire.
                            app.send_action(id, pressed);
                        }
                    }
                }
            }

            _ => {}
        }
    }
}

impl Client {
    fn create_window(
        &mut self,
        event_loop: &ActiveEventLoop,
    ) -> Result<Surface, Box<dyn std::error::Error>> {
        let attributes = Window::default_attributes()
            .with_title("Tiamot")
            .with_inner_size(winit::dpi::LogicalSize::new(DEFAULT_SIZE.0, DEFAULT_SIZE.1));
        let window = Arc::new(event_loop.create_window(attributes)?);

        let instance = wgpu::Instance::new(wgpu::InstanceDescriptor::new_with_display_handle(
            Box::new(Arc::clone(&window)),
        ));
        let surface = instance.create_surface(Arc::clone(&window))?;
        // The adapter is chosen for THIS surface. Asking for one without it and
        // hoping the surface is compatible is how a machine with two GPUs ends
        // up rendering on the one the window is not on.
        let gpu = Gpu::open(&instance, Some(&surface))?;

        let physical = window.inner_size();
        let size = (physical.width.max(1), physical.height.max(1));
        let present_mode = configure_surface(&surface, &gpu, size, self.config.vsync);

        let egui_renderer = egui_wgpu::Renderer::new(
            &gpu.device,
            // The interface is drawn over the world, onto the surface.
            gpu.surface_format(),
            egui_wgpu::RendererOptions {
                // No MSAA and no depth: the HUD is flat text drawn last, and
                // testing it against the world's depth buffer would let a
                // nearby block occlude the frame counter.
                msaa_samples: 1,
                depth_stencil_format: None,
                ..Default::default()
            },
        );
        // Settings are pushed into it by `app::apply_to_renderer` when a world
        // starts, and NOT here: the front screen sits between this moment and
        // that one, so anything applied now is whatever the file said before
        // the player touched it.
        let renderer = Renderer::new(gpu.clone(), self.config.render_mode, size.0, size.1)?;

        let egui = egui::Context::default();
        // egui is built without `default_fonts`, so it has no glyphs until this
        // runs. Skipping it renders an empty HUD and reports nothing.
        client::app::install_fonts(&egui);
        let egui_state = egui_winit::State::new(
            egui.clone(),
            egui.viewport_id(),
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );

        // **The renderer is built here and parked until there is a world.**
        // It owns the depth buffer, the pipelines and the shadow cascades, and
        // building it takes long enough that doing it on the Play button would
        // be a visible stall between pressing it and anything happening.
        self.present_mode = present_mode;
        self.parked = Some(Parked {
            renderer,
            present_mode,
            mode: self.config.render_mode,
        });

        let stage = match self.connection.take() {
            // `menu = false` in `client.toml`, or a `--connect` run: the world
            // was dialled before the window and there is nothing to choose.
            Some(connection) => Stage::Playing(Box::new(self.start_app(connection))),
            None => Stage::Front(Box::new(client::front::Front::new(
                self.library.clone(),
                self.catalogue.clone(),
            ))),
        };

        Ok(Surface {
            window,
            surface,
            gpu,
            stage,
            egui,
            egui_state,
            egui_renderer,
            dialogs: client::dialog::Dialogs::default(),
            atlas_texture: None,
            size,
        })
    }

    /// Builds the running world around a connection.
    ///
    /// Its own function because it happens twice — once at startup when the
    /// world was chosen before the window, and once when the player presses
    /// Play — and the two must set the same things up.
    fn start_app(&mut self, connection: Connection) -> App {
        let Parked {
            mut renderer,
            present_mode,
            mode,
        } = self
            .parked
            .take()
            .expect("the renderer is parked when the window is created");
        // **The one setting that needs a new renderer.** Draw mode picks the
        // pipelines when they are built, so a player who changed it on the
        // front screen gets a rebuild here rather than the old pipelines and a
        // setting that appears to do nothing. The stall is on the Play button,
        // which is the one moment in the session where a pause is expected.
        if mode != self.config.render_mode
            && let Some(surface) = self.window.as_ref()
        {
            match Renderer::new(
                surface.gpu.clone(),
                self.config.render_mode,
                surface.size.0,
                surface.size.1,
            ) {
                Ok(rebuilt) => renderer = rebuilt,
                Err(err) => {
                    tracing::warn!(%err, "could not switch draw mode; keeping the last one");
                }
            }
        }
        // **And the one setting that lives on the surface.** Vsync is chosen
        // when the surface is configured, which until now happened at window
        // creation, on a resize, and on a surface loss — none of which a player
        // ticking the box on the front screen performs.
        let present_mode = match self.window.as_ref() {
            Some(surface) => configure_surface(
                &surface.surface,
                &surface.gpu,
                surface.size,
                self.config.vsync,
            ),
            None => present_mode,
        };
        self.present_mode = present_mode;

        let mut app = App::with_bindings(
            self.config.clone(),
            connection,
            renderer,
            self.bindings.take().unwrap_or_default(),
        );
        // So the HUD reports the mode in force rather than the flag that asked
        // for one. See `configure_surface`.
        app.set_present_mode(present_mode);
        // The environment belongs to the binary, not to `App`: a library
        // reading it would be process-global state a caller cannot control,
        // which is a poor thing for tests and a worse one for an embedded
        // server.
        if let Some(path) = std::env::var_os("TIAMOT_TRACE_FRAMES") {
            let path = std::path::PathBuf::from(path);
            if app.log_frames_to(&path) {
                tracing::info!(path = %path.display(), "logging every frame");
            } else {
                tracing::warn!(path = %path.display(), "could not open the frame log");
            }
        }
        if let Some(path) = std::env::var_os("TIAMOT_TRACE_PHYSICS") {
            let path = std::path::PathBuf::from(path);
            if app.trace_physics_to(&path) {
                tracing::info!(path = %path.display(), "tracing physics per tick");
            } else {
                tracing::warn!(path = %path.display(), "could not open the physics trace");
            }
        }
        app
    }

    /// Leaves the running world and goes back to the front screen.
    ///
    /// Returns whether to keep going, which is always yes — the window stays,
    /// and only the world ends.
    fn leave_world(&mut self) -> bool {
        let Some(surface) = self.window.as_mut() else {
            return false;
        };
        // Replaced rather than taken, because a `Surface` without a stage is
        // not a thing this can hold even for a statement.
        let stage = std::mem::replace(
            &mut surface.stage,
            Stage::Front(Box::new(client::front::Front::new(
                self.library.clone(),
                self.catalogue.clone(),
            ))),
        );
        if let Stage::Playing(app) = stage {
            // Order matters: leave the server before stopping it, or the last
            // thing in the log is a connection dropping rather than a clean
            // goodbye.
            let (renderer, bindings, config) = app.leave();
            self.config = config;
            self.parked = Some(Parked {
                renderer,
                mode: self.config.render_mode,
                present_mode: self.present_mode,
            });
            self.bindings = Some(bindings);
        }
        // **Before the world, every time.** A beacon outliving its server
        // advertises an address nothing answers on, and the client that dials
        // it gets a timeout rather than a reason.
        if let Some(beacon) = self.beacon.take() {
            beacon.stop();
        }
        if let Some(handle) = self.embedded.take() {
            handle.stop();
        }
        // The cursor comes back, or the menu cannot be clicked.
        self.grabbed = grab(&surface.window, false);
        self.released_for_dialog = false;
        self.digging = false;
        self.held = Held::default();
        // The atlas belonged to the world that just ended; the next one
        // registers its own.
        if let Some(texture) = surface.atlas_texture.take() {
            surface.egui_renderer.free_texture(&texture);
        }
        true
    }

    /// Draws one frame of the front screen. Returns whether to keep going.
    ///
    /// **Its own loop, not a branch inside the world's.** A menu has no camera
    /// to advance, no network to pump and no chunks to mesh, and threading
    /// "unless there is no world" through every one of those would put the
    /// menu's existence into code that has nothing to do with it.
    fn front_frame(&mut self) -> bool {
        let Some(surface) = self.window.as_mut() else {
            return false;
        };
        let frame = match surface.surface.get_current_texture() {
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                configure_surface(
                    &surface.surface,
                    &surface.gpu,
                    surface.size,
                    self.config.vsync,
                );
                return true;
            }
            // A window nobody can see. Nothing to draw and nothing to say
            // about it — the menu is not paced by anything, so the next frame
            // simply tries again.
            wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => {
                return true;
            }
            wgpu::CurrentSurfaceTexture::Validation => {
                self.error = Some("the surface rejected a frame request".into());
                return false;
            }
        };
        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());

        // **Cleared here rather than by the interface pass.** `draw_front`
        // loads what is already in the buffer, exactly as the HUD does over the
        // world — and with no world there is nothing in it, so the menu would
        // be drawn over whatever the last frame left behind.
        clear(&surface.gpu, &view);
        let (action, ticked) = draw_front(surface, &mut self.config, &view);
        // **Carried back before the action is acted on.** `Action::Open` is in
        // the same frame as the tick that changed the set, and a world started
        // from the old catalogue is exactly the report: unticking a mod left
        // it in the world.
        if let Some(ticked) = ticked {
            self.catalogue = ticked;
            if let Err(err) = self.catalogue.save(&self.data.join("mods.toml")) {
                tracing::warn!(%err, "could not save which mods are on");
            }
        }
        // **Read after drawing and before acting.** The tick box lives on the
        // screen that produced the action, and `open` borrows the window.
        let lan = match &surface.stage {
            Stage::Front(front) => front.host_on_lan,
            Stage::Playing(_) => false,
        };
        frame.present();

        match action {
            client::front::Action::None => true,
            client::front::Action::Quit => false,
            client::front::Action::Open(entry) => self.open(&entry, lan),
            client::front::Action::Create(name) => self.create(&name, lan),
            client::front::Action::Remember(entry) => {
                self.library.add(*entry);
                self.save_library();
                true
            }
            client::front::Action::Forget(name) => {
                self.library.forget(&name);
                self.save_library();
                true
            }
        }
    }

    /// Opens a world or dials a server, and hands the window the running world.
    ///
    /// A failure is reported ON the front screen rather than to the terminal: a
    /// player who typed an address wrong is looking at the menu, and a log line
    /// they never see is the same as no message at all.
    fn open(&mut self, entry: &client::launcher::Entry, lan: bool) -> bool {
        match self.connect(entry, lan) {
            Ok(()) => {
                self.library.add(client::launcher::Entry {
                    // The mods it is being played with NOW, so the next visit
                    // compares against the truth rather than against whatever
                    // it was made with.
                    mods: if entry.is_local() {
                        self.catalogue.enabled()
                    } else {
                        Vec::new()
                    },
                    // **Stamped on the way IN, not on the way out.** A world
                    // is last played from the moment it opens; recording it on
                    // leaving would lose the stamp for a session that ended in
                    // a crash, which is exactly the session a player wants to
                    // get back to.
                    last_played: client::launcher::now_seconds(),
                    ..entry.clone()
                });
                self.save_library();
                true
            }
            Err(err) => {
                if let Some(surface) = self.window.as_mut()
                    && let Stage::Front(front) = &mut surface.stage
                {
                    front.notice = Some(format!("could not open `{}`: {err}", entry.name));
                }
                true
            }
        }
    }

    /// Makes a world and opens it.
    fn create(&mut self, name: &str, lan: bool) -> bool {
        let entry = client::launcher::Entry {
            name: name.to_owned(),
            kind: client::launcher::Kind::Local {
                path: unused_world_directory(&self.data, name),
            },
            mods: self.catalogue.enabled(),
            last_played: client::launcher::now_seconds(),
        };
        self.open(&entry, lan)
    }

    /// Starts whatever `entry` names and swaps the window into it.
    fn connect(&mut self, entry: &client::launcher::Entry, lan: bool) -> Result<(), String> {
        let identity = Identity::load_or_create(&self.identity_path)
            .map_err(|err| format!("this machine's identity could not be read: {err}"))?;
        let address = match &entry.kind {
            client::launcher::Kind::Local { path } => {
                let handle = start_local_world(
                    &self.data.join(path),
                    self.config.view(),
                    self.catalogue.enabled(),
                    &identity.uuid_as_root(),
                    lan,
                )
                .map_err(|err| err.to_string())?;
                // **Loopback when the world listens on everything.** A server
                // bound to `0.0.0.0` reports that as its address, and
                // `0.0.0.0` is somewhere to listen rather than somewhere to
                // connect — dialling it fails outright with "invalid remote
                // address", so opening a world to the LAN would have stopped
                // the host from joining their own world.
                let address = own_address(handle.local_addr());
                // **A world open to the network says so on it.** Otherwise
                // joining still means somebody reading an address off a screen
                // and typing it, which is the report this exists to answer.
                // Announced under the world's own name, which is what its
                // owner would say it was called.
                if lan {
                    self.beacon = handle.announce(&entry.name);
                    if self.beacon.is_none() {
                        tracing::warn!(
                            "this world is open but is not announcing itself; \
                             others will have to type its address"
                        );
                    }
                }
                // Held on `Client`, because dropping it stops the world.
                self.embedded = Some(handle);
                address
            }
            client::launcher::Kind::Remote { address } => {
                use std::net::ToSocketAddrs;
                address
                    .to_socket_addrs()
                    .map_err(|err| format!("`{address}` is not an address: {err}"))?
                    .next()
                    .ok_or_else(|| format!("`{address}` resolved to nothing"))?
            }
        };

        let cache = ContentCache::open(&self.data.join("content"))
            .map_err(|err| format!("the content cache could not be opened: {err}"))?;
        let connection = Connection::open(
            address,
            identity,
            self.config.display_name.clone(),
            cache,
            &self.data.join("known-hosts"),
        )
        .map_err(|err| {
            // The server this failed to reach is stopped again, or a second
            // attempt would find the world already locked by the first.
            if let Some(beacon) = self.beacon.take() {
                beacon.stop();
            }
            if let Some(handle) = self.embedded.take() {
                handle.stop();
            }
            err.to_string()
        })?;

        let mut app = self.start_app(connection);
        // **Only when it was actually opened**, and only for a world this
        // machine runs: joining somebody else's server is not hosting one.
        if lan && matches!(entry.kind, client::launcher::Kind::Local { .. }) {
            app.set_hosting(lan_address(LAN_PORT));
        }
        if let Some(surface) = self.window.as_mut() {
            surface.stage = Stage::Playing(Box::new(app));
            // **Taken on the way in.** A world opened from the menu used to
            // start with the pointer sitting in the middle of it: the client
            // only ever grabbed on the first click, which was the right rule
            // when it opened already in a world and there was nothing to click.
            self.grabbed = grab(&surface.window, true);
            self.released_for_dialog = false;
        }
        Ok(())
    }

    /// Writes the world list, complaining to the log if it cannot.
    fn save_library(&self) {
        if let Err(err) = self.library.save(&self.data.join("worlds.toml")) {
            tracing::warn!("{err}");
        }
    }

    /// Draws one frame. Returns whether to keep going.
    fn frame(&mut self) -> bool {
        if self
            .window
            .as_ref()
            .is_some_and(|surface| matches!(surface.stage, Stage::Front(_)))
        {
            return self.front_frame();
        }
        let Some(surface) = self.window.as_mut() else {
            return false;
        };
        let Stage::Playing(app) = &mut surface.stage else {
            return false;
        };

        // **A dialog needs the mouse, and the key that opened it cannot know.**
        //
        // Reported from the window: pressing E for the inventory left the
        // cursor grabbed, so the screen was there and unclickable. The key
        // could not release it — a mod's dialog opens after a round trip to the
        // server, so at the moment the key is pressed there is nothing open
        // yet. Handled here, where the answer is known, on the transition
        // rather than every frame so a player can still take the cursor back
        // with Escape while a dialog is up.
        // **Singleplayer pauses; a hosted game does not.** `embedded` is only
        // `Some` when this client owns the server it is talking to — on
        // somebody else's server there are other people in it, and one of them
        // opening a menu must not stop the world for everybody.
        if let Some(server) = self.embedded.as_ref() {
            let wanted = app.menu_open();
            if server.paused() != wanted {
                server.set_paused(wanted);
            }
            // **And the client stops with it.** Both sides of a paused world
            // have to hold still: a client that went on predicting would come
            // back from the menu holding a body the server never moved, and be
            // corrected out of it over the next second and a half. Reported
            // from the window as being pulled about after unpausing.
            app.set_world_paused(wanted);
        }

        // **Quitting a world is not quitting the game.** It was, because
        // there was nowhere else to go; now it goes back to the front screen,
        // where the player can open another world or press Quit again.
        if app.take_quit_request() {
            return self.leave_world();
        }

        let dialog_open = !app.dialogs().is_empty();
        if dialog_open != self.released_for_dialog {
            self.released_for_dialog = dialog_open;
            // Asked on the transition rather than every frame, so a player who
            // took the cursor back with Escape while a dialog is up keeps it —
            // and answered by the one rule, so a dialog closing over an open
            // menu does not snatch it away again.
            let wanted = wants_cursor(app.menu_open(), app.chat_open(), dialog_open);
            self.grabbed = hand_over(
                &surface.window,
                wanted,
                app,
                &mut self.held,
                &mut self.digging,
            );
        }

        let now = std::time::Instant::now();
        // Clamped: a frame after a long stall — a breakpoint, a window drag —
        // would otherwise teleport the camera by however long the machine was
        // busy.
        let dt = (now - self.last_frame).as_secs_f32().min(0.1);
        self.last_frame = now;

        // Every phase is timed, and the breakdown of the frame that turns out
        // to be the worst is what the HUD reports. See `app::Phases`: a set of
        // independent per-phase maxima does not add up to the worst frame, so
        // it cannot say what the worst frame was doing.
        let mut phases = Phases::default();

        // **The image is acquired BEFORE any work is done for it.**
        //
        // It used to be asked for last, which is the conventional order — hold
        // the swapchain image for as short a time as possible. That order costs
        // nothing when every frame presents, and this one did not: measured from
        // the window, `211 fps · 103 presented`, so half the frames pumped the
        // network, spent a full `REMESH_TIME_BUDGET` meshing and advanced the
        // world, then found no image and threw all of it away. During a streaming
        // burst — 24 chunks queued, `acquire 42.6 ms` of a 43.0 ms frame — that
        // is the client spending its budget twice over on frames nobody sees,
        // while the frame that DOES present reports `remesh 0.0` because the
        // discarded attempts already drained the queue.
        //
        // Acquiring first also paces the loop: with `Fifo` this blocks until the
        // display is ready, so the work below happens once per presented frame
        // rather than as fast as the CPU can spin. `acquire` is therefore the
        // vsync wait now, and a large number there is the display doing its job
        // rather than a hitch — read `present`, `world` and `remesh` for work.
        let phase = std::time::Instant::now();
        let frame = match surface.surface.get_current_texture() {
            // Suboptimal still hands over a usable texture. Reconfiguring is
            // recommended, not required, and doing it mid-frame would drop a
            // frame every time the window was being dragged.
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,

            // The surface no longer matches the window. Reconfiguring and
            // skipping this frame is the whole remedy — and skipping it now
            // costs nothing, which is the point of asking first.
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                configure_surface(
                    &surface.surface,
                    &surface.gpu,
                    surface.size,
                    self.config.vsync,
                );
                phases.acquire = elapsed_ms(phase);
                app.log_frame(&phases, false);
                return true;
            }

            // Minimised, or the compositor is busy. Not an error, and not worth
            // a log line every frame while a window sits in the dock — but it IS
            // worth a row in the frame log, because a frame that produced no
            // picture is exactly what the log exists to count.
            wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => {
                phases.acquire = elapsed_ms(phase);
                app.log_frame(&phases, false);
                return true;
            }

            wgpu::CurrentSurfaceTexture::Validation => {
                self.error = Some("the surface rejected a frame request".into());
                return false;
            }
        };
        phases.acquire = elapsed_ms(phase);

        let phase = std::time::Instant::now();
        if !app.pump_network() {
            return false;
        }
        phases.network = elapsed_ms(phase);

        // **Played here rather than where they arrive**, because a sound's
        // place is relative to where the camera is NOW. Draining this on the
        // network task would spatialise every sound against wherever the
        // player happened to be standing when the packet landed.
        //
        // Not timed as its own phase: starting a sound is handing a buffer to
        // kira's thread, which is the point of kira having one.
        app.play_heard();
        let _ = app.play_footsteps();
        // Jumping and landing, watched from the body rather than from the key:
        // a jump pressed against a ceiling makes no noise and a fall off a
        // ledge lands without anybody pressing anything.
        app.play_movement_cues();
        // After the HUD raised them last frame; before the next draw does.
        app.flush_dialog_events();

        let phase = std::time::Instant::now();
        app.remesh();
        phases.remesh = elapsed_ms(phase);

        // `advance` reports the input itself, once per simulation tick rather
        // than once per frame. Reporting per frame sent the same tick number
        // repeatedly on a fast machine and skipped ticks on a slow one, and the
        // server's input queue is keyed by tick.
        let phase = std::time::Instant::now();
        let input = self.held.as_input(self.pending_teleport.take());
        app.advance(input, dt);

        // After `advance`, so the dig aims at where the player ended up this
        // frame rather than where they started. Re-sent every frame the button
        // is held: re-aiming at the same cell keeps its progress, so this is
        // free, and it means a dig follows the crosshair rather than sticking
        // to whatever was under it when the button went down.
        if self.digging {
            app.dig();
        }
        phases.advance = elapsed_ms(phase);

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let camera = *app.camera();

        let phase = std::time::Instant::now();
        app.renderer().render(&view, &camera, surface.size);
        phases.world = elapsed_ms(phase);

        let phase = std::time::Instant::now();
        draw_hud(surface, &view);
        phases.hud = elapsed_ms(phase);

        let phase = std::time::Instant::now();
        frame.present();
        phases.present = elapsed_ms(phase);

        // Taken again because `draw_hud` needs the whole window and the world
        // is one of its fields. Cheap, and the alternative is threading the
        // interface's needs through a borrow it does not want.
        let Stage::Playing(app) = &mut surface.stage else {
            return false;
        };
        // Only here, at the one place a frame becomes a picture. Now that the
        // image is acquired first, the count should track the frame rate closely
        // — a gap means frames are still being built and dropped, which is the
        // thing this was added to catch.
        app.note_presented();
        app.log_frame(&phases, true);

        // Paired with the `dt` measured at the top of the NEXT frame, which is
        // what actually measures this one.
        app.record_phases(phases);
        true
    }
}

/// Milliseconds since an instant, for one phase of a frame.
fn elapsed_ms(since: std::time::Instant) -> f32 {
    since.elapsed().as_secs_f32() * 1000.0
}

/// Lists every sound a server's mods registered, under the mod that asked for it.
///
/// **The attribution criterion, the sound half** — its twin is the binding list
/// above, and [`client::audio::by_mod`] is the one place that decides who owns
/// what. Collapsed by default: a player opens this screen to change a control
/// or a volume, and consults the list only when they want to know what a
/// server has taught the client to say.
///
/// A sound whose file the server never had is called out rather than hidden.
/// The client plays nothing for it, and silence with no explanation is the
/// hardest kind of audio fault to report.
fn draw_sound_attribution(app: &App, ui: &mut egui::Ui) {
    let groups = client::audio::by_mod(app.sounds());
    ui.separator();
    let heading = if groups.is_empty() {
        "sounds — none; this server's mods make no noise".to_owned()
    } else {
        format!("sounds ({})", app.sounds().len())
    };
    egui::CollapsingHeader::new(heading)
        .id_salt("sound-attribution")
        .show(ui, |ui| {
            for (mod_id, sounds) in &groups {
                ui.heading(*mod_id);
                for sound in sounds {
                    ui.horizontal(|ui| {
                        ui.add_sized([300.0, 18.0], egui::Label::new(&sound.id).truncate());
                        if sound.file.is_none() {
                            ui.label(
                                egui::RichText::new("file missing").color(egui::Color32::LIGHT_RED),
                            );
                        }
                    });
                }
                ui.separator();
            }
        });
}

/// Draws what the pushed HUD scripts asked for, and the engine's crosshair.
///
/// # The engine's HUD is three things, and this draws one of them
///
/// Criterion 1 of Task 14: delete every mod and what is left is a crosshair,
/// chat, and the settings screen. The hotbar, the dig readout, anything a game
/// wants — those are a mod's, drawn from `core::hud` commands, and they go when
/// the mod goes. That is why the crosshair is here in the window rather than in
/// `game/core_ui`: it must survive with zero mods loaded.
///
/// # Virtual pixels to real ones
///
/// A script draws on a canvas [`tiamot_core::hud::VIRTUAL_HEIGHT`] tall and as
/// wide as this window's aspect ratio makes it. Everything scales by the height
/// alone, so a HUD is the same apparent size on every monitor and anchors take
/// care of the width. Points rather than physical pixels, because egui works in
/// points and DPI is its problem, not the script's.
fn draw_hud_scripts(app: &mut App, ctx: &egui::Context, icons: client::icons::Icons<'_>) {
    app.run_hud_scripts();

    // **Off means off, crosshair included** — but never chat, which is drawn
    // elsewhere and is not the HUD's to hide (see `core::hud::Builtin`).
    if !app.hud_visible() {
        return;
    }

    // `content_rect`, not `viewport_rect`: a HUD anchored to the bottom edge
    // must not sit under an OS status bar or a display notch. A crosshair in
    // the middle would not care; a hotbar 16 up from the bottom would.
    let screen = ctx.content_rect();
    let scale = screen.height() / f32::from(tiamot_core::hud::VIRTUAL_HEIGHT);
    let virtual_width = if scale > 0.0 {
        screen.width() / scale
    } else {
        0.0
    };
    let painter = ctx.layer_painter(egui::LayerId::new(
        egui::Order::Foreground,
        egui::Id::new("hud_scripts"),
    ));

    let hides_crosshair = app
        .hud_frame(|frame| {
            for command in frame.commands() {
                paint_hud_command(&painter, command, virtual_width, scale, icons);
            }
            frame.hides(tiamot_core::hud::Builtin::Crosshair)
        })
        .unwrap_or(false);

    if !hides_crosshair {
        paint_crosshair(&painter, screen.center(), scale);
    }
}

/// The engine's crosshair: two strokes, and nothing a mod has to provide.
fn paint_crosshair(painter: &egui::Painter, centre: egui::Pos2, scale: f32) {
    // Sized in virtual pixels like everything else, so it does not become a
    // speck on a tall monitor.
    let arm = 10.0 * scale;
    let gap = 3.0 * scale;
    // White with a dark edge underneath, because a crosshair on a white world
    // is invisible — which is exactly the world this engine's reference mods
    // build.
    for (colour, width) in [
        (egui::Color32::from_black_alpha(160), 3.0 * scale),
        (egui::Color32::WHITE, 1.0 * scale),
    ] {
        let stroke = egui::Stroke::new(width, colour);
        for (from, to) in [
            (
                egui::pos2(centre.x - arm, centre.y),
                egui::pos2(centre.x - gap, centre.y),
            ),
            (
                egui::pos2(centre.x + gap, centre.y),
                egui::pos2(centre.x + arm, centre.y),
            ),
            (
                egui::pos2(centre.x, centre.y - arm),
                egui::pos2(centre.x, centre.y - gap),
            ),
            (
                egui::pos2(centre.x, centre.y + gap),
                egui::pos2(centre.x, centre.y + arm),
            ),
        ] {
            painter.line_segment([from, to], stroke);
        }
    }
}

/// Paints one draw command.
///
/// **Nothing here decides anything.** Every clamp, limit and refusal already
/// happened in `core::hud` where it could be tested without a window; this walks
/// a list and paints it.
fn paint_hud_command(
    painter: &egui::Painter,
    command: &tiamot_core::hud::Command,
    virtual_width: f32,
    scale: f32,
    icons: client::icons::Icons<'_>,
) {
    use tiamot_core::hud::Command;

    let place = |anchor: tiamot_core::hud::Anchor, x: i16, y: i16| {
        let (vx, vy) = anchor.resolve(virtual_width, x, y);
        egui::pos2(vx * scale, vy * scale)
    };
    let rgba = |colour: tiamot_core::ui::Colour| {
        egui::Color32::from_rgba_unmultiplied(colour[0], colour[1], colour[2], colour[3])
    };

    match command {
        Command::Text {
            anchor,
            x,
            y,
            text,
            size,
            colour,
        } => {
            painter.text(
                place(*anchor, *x, *y),
                egui::Align2::LEFT_TOP,
                text,
                egui::FontId::proportional(f32::from(*size) * scale),
                rgba(*colour),
            );
        }
        Command::Rect {
            anchor,
            x,
            y,
            w,
            h,
            colour,
        } => {
            let min = place(*anchor, *x, *y);
            let size = egui::vec2(f32::from(*w) * scale, f32::from(*h) * scale);
            painter.rect_filled(egui::Rect::from_min_size(min, size), 0.0, rgba(*colour));
        }
        Command::Bar {
            anchor,
            x,
            y,
            w,
            h,
            fill,
            colour,
            background,
        } => {
            let min = place(*anchor, *x, *y);
            let size = egui::vec2(f32::from(*w) * scale, f32::from(*h) * scale);
            painter.rect_filled(egui::Rect::from_min_size(min, size), 0.0, rgba(*background));
            let filled = egui::vec2(size.x * fill.fraction(), size.y);
            painter.rect_filled(egui::Rect::from_min_size(min, filled), 0.0, rgba(*colour));
        }
        Command::Icon {
            anchor,
            x,
            y,
            size,
            material,
            shape,
        } => {
            let min = place(*anchor, *x, *y);
            let extent = egui::vec2(f32::from(*size) * scale, f32::from(*size) * scale);
            let rect = egui::Rect::from_min_size(min, extent);
            icons.paint_stack(painter, rect, material.0, *shape);
            painter.rect_stroke(
                rect,
                2.0,
                egui::Stroke::new(scale, egui::Color32::from_black_alpha(120)),
                egui::StrokeKind::Inside,
            );
        }
        Command::Image {
            anchor, x, y, w, h, ..
        } => {
            // **Visible rather than silent.** Content images are not bridged
            // into egui yet — the atlas is the world renderer's texture and
            // tier-1 `Widget::Image` does not draw one either. A script that
            // asked for a picture gets a placeholder it can SEE, because an
            // image that draws nothing is indistinguishable from a script that
            // never ran.
            let min = place(*anchor, *x, *y);
            let size = egui::vec2(f32::from(*w) * scale, f32::from(*h) * scale);
            let rect = egui::Rect::from_min_size(min, size);
            painter.rect_filled(rect, 0.0, egui::Color32::from_rgb(200, 0, 200));
            painter.rect_stroke(
                rect,
                0.0,
                egui::Stroke::new(scale, egui::Color32::BLACK),
                egui::StrokeKind::Inside,
            );
        }
    }
}

/// Draws the chat history, and the input line when it is open.
///
/// **Engine-native, and that is a decision.** Task 14 puts chat in the engine
/// because moderation and RCON depend on it: an operator must be able to read
/// and stop what is said without every server having installed the same mod.
/// It therefore works with zero mods loaded, which is what criterion 1's
/// "minimal engine HUD" means.
///
/// Deliberately not a `core::ui` dialog. A dialog belongs to a mod and can be
/// closed by one; chat cannot be, so it is drawn by the client directly.
fn draw_chat(app: &mut App, ctx: &egui::Context) {
    let lines: Vec<String> = app.chat().map(ToOwned::to_owned).collect();
    let open = app.chat_open();
    if lines.is_empty() && !open {
        return;
    }

    let focus = app.take_chat_focus();
    let mut send = false;
    let mut close = false;
    egui::Area::new(egui::Id::new("chat"))
        .anchor(egui::Align2::LEFT_BOTTOM, egui::vec2(8.0, -8.0))
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(egui::Color32::from_black_alpha(140))
                .inner_margin(6.0)
                .show(ui, |ui| {
                    // Newest at the bottom, which is where a reader's eye is.
                    // Only the last few unless the box is open, so chat does
                    // not cover the world nobody is talking about.
                    let shown = if open { 12 } else { 5 };
                    let skip = lines.len().saturating_sub(shown);
                    egui::ScrollArea::vertical()
                        .max_height(220.0)
                        .stick_to_bottom(true)
                        .show(ui, |ui| {
                            for line in lines.iter().skip(skip) {
                                ui.label(egui::RichText::new(line).color(egui::Color32::WHITE));
                            }
                        });
                    if !open {
                        return;
                    }
                    let response = ui.add(
                        egui::TextEdit::singleline(app.chat_draft_mut())
                            .desired_width(420.0)
                            .hint_text("say something"),
                    );
                    // **Once, on the frame it opens — never every frame.**
                    //
                    // Reported from the window: chat did not work. It took the
                    // keys and never sent, because egui reports a single-line
                    // field's Enter as `lost_focus`, and a field handed focus
                    // back on every frame never loses it. Enter was landing on
                    // a box that immediately took focus again, so the line sat
                    // there being typed into for ever.
                    if focus {
                        response.request_focus();
                    }
                    if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
                        send = true;
                    }
                    if ui.input(|i| i.key_pressed(egui::Key::Escape)) {
                        close = true;
                    }
                });
        });

    if send {
        app.send_chat();
    } else if close {
        app.set_chat_open(false);
    }
}

/// Draws the controls screen, and applies whatever the player clicked.
///
/// **Every binding says which mod asked for it** — that is Task 13's
/// attribution criterion, and `Actions::by_source` is the one place that
/// decides. The engine's own controls are a group like any other rather than an
/// unlabelled remainder.
///
/// The whole of the model behind this is in `client::input` and is tested
/// there without a window, which is what the task asks for: the screen only
/// reads a list and reports clicks.
/// The pause menu: the interface's front door.
///
/// # Why this exists at all
///
/// Escape used to release the cursor and nothing else, so the only way into the
/// settings was one undocumented function key. Reported from the window as the
/// controls screen being hard to reach and janky when it got there.
///
/// Everything here is a page or a switch, and the two switches are the ones a
/// player reaches for most: whether the HUD is drawn and whether the debug
/// overlay is. **Chat is not among them** — moderation depends on a player
/// being able to read what is said, so it cannot be switched off from a menu
/// any more than a mod can hide it.
fn draw_menu(app: &mut App, ctx: &egui::Context) {
    let mut resume = false;
    let mut controls = false;
    let mut quit = false;
    let hosting = app.hosting().map(str::to_owned);
    let mut hud = app.hud_visible();
    let mut overlay = app.debug_overlay();
    let live = app.ui_scale();
    let mut settled = None;
    let draft = app.ui_scale_draft();

    // **The same sheet every other screen gets**, and it decides the shape so
    // this cannot. See `client::panel::sheet`.
    resume |= client::panel::sheet(ctx, "Paused", Some("Resume"), |ui| {
        {
            ui.vertical_centered_justified(|ui| {
                ui.add_space(6.0);
                // **Where to tell people to connect.** Only when the world is
                // actually open: a line that always showed an address would
                // have people typing one at a server that refuses them.
                if let Some(address) = &hosting {
                    ui.label(format!("Open to your network at  {address}"));
                    ui.label(
                        "They join from Play → Server. The first connection pins this \
                         machine's certificate.",
                    );
                    ui.add_space(6.0);
                }
                if ui.button("Settings").clicked() {
                    controls = true;
                }
                // **"Leave", not "Quit".** It goes back to the front screen,
                // where the game is still running and another world is one
                // click away — calling that quitting would make a player who
                // wanted to switch worlds close the game instead.
                if ui.button("Leave world").clicked() {
                    quit = true;
                }
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(6.0);
            });

            // **Interface scale, on release rather than live.** It used to
            // apply while the drag was running, on the grounds that a scale you
            // cannot see is a scale you have to guess at. That is true and it
            // is the lesser problem: the scale rescales the slider, so the
            // control moved under the pointer and the value chased it. Reported
            // from the window as jumping around. See `client::widget::settle`.
            settled = client::widget::on_release(
                ui,
                "interface scale",
                client::config::UI_SCALE_RANGE,
                client::config::UI_SCALE_STEP,
                live,
                draft,
            );
            ui.checkbox(&mut hud, "Show HUD");
            ui.checkbox(&mut overlay, "Debug overlay");
            ui.add_space(6.0);
        }
    });

    if let Some(scale) = settled {
        app.set_ui_scale(scale);
    }
    app.set_hud_visible(hud);
    app.set_debug_overlay(overlay);
    if controls {
        app.open_settings();
    }
    if resume {
        app.set_menu_open(false);
    }
    if quit {
        app.request_quit();
    }
}

fn draw_settings(app: &mut App, ctx: &egui::Context) {
    // Collected before the panel runs, because drawing borrows the registry
    // and the buttons need `&mut App` to act.
    #[derive(Clone)]
    struct Row {
        id: String,
        description: String,
        binding: String,
        custom: bool,
        conflicted: bool,
    }
    let conflicts = app.bindings().conflicts(app.actions());
    let conflicting: std::collections::BTreeSet<&str> = conflicts
        .iter()
        .flat_map(|(_, ids)| ids.iter().map(String::as_str))
        .collect();
    let groups: Vec<(String, Vec<Row>)> = app
        .actions()
        .by_source()
        .into_iter()
        .map(|(source, actions)| {
            let rows = actions
                .into_iter()
                .map(|action| Row {
                    binding: app
                        .bindings()
                        .get(app.actions(), &action.id)
                        .map_or_else(|| "—".to_owned(), |input| input.to_string()),
                    custom: app.bindings().is_custom(&action.id),
                    conflicted: conflicting.contains(action.id.as_str()),
                    id: action.id.clone(),
                    description: action.description.clone(),
                })
                .collect();
            (source.label().to_owned(), rows)
        })
        .collect();
    let waiting = app.rebinding().map(ToOwned::to_owned);

    let mut rebind: Option<String> = None;
    let mut reset: Option<String> = None;
    let mut reset_all = false;
    let mut close = false;
    let mut volumes_changed = false;

    // **The same sheet every other screen gets**, which now decides the shape
    // rather than being asked for one. This page is a scrolling list of
    // bindings with volume sliders under it, and a `fixed_size` egui grows past
    // is how it ended up running off the top and the bottom of the screen with
    // no way to reach either end.
    close |= client::panel::sheet(ctx, "Controls and audio", Some("Back"), |ui| {
        {
            if let Some(id) = &waiting {
                ui.label(
                    egui::RichText::new(format!("Press a key for {id} — Escape to cancel"))
                        .color(egui::Color32::LIGHT_YELLOW),
                );
                ui.separator();
            }
            if !conflicts.is_empty() {
                for (input, ids) in &conflicts {
                    ui.label(
                        egui::RichText::new(format!("{input} is bound to {}", ids.join(", ")))
                            .color(egui::Color32::LIGHT_RED),
                    );
                }
                ui.separator();
            }
            {
                for (source, rows) in &groups {
                    // **The attribution.** A player can see which mod wants
                    // every binding they are being offered.
                    ui.heading(source);
                    for row in rows {
                        ui.horizontal(|ui| {
                            let label = if row.description.is_empty() {
                                row.id.clone()
                            } else {
                                row.description.clone()
                            };
                            ui.add_sized([300.0, 18.0], egui::Label::new(label).truncate());
                            let text = egui::RichText::new(&row.binding);
                            let text = if row.conflicted {
                                text.color(egui::Color32::LIGHT_RED)
                            } else {
                                text
                            };
                            if ui
                                .add_sized([120.0, 18.0], egui::Button::new(text))
                                .clicked()
                            {
                                rebind = Some(row.id.clone());
                            }
                            // Only where there is something to undo, so the row
                            // says at a glance which bindings are the player's.
                            if row.custom && ui.small_button("reset").clicked() {
                                reset = Some(row.id.clone());
                            }
                        });
                    }
                    ui.separator();
                }
            }
            ui.separator();
            ui.heading("volume");
            // **Live, not on close.** A slider you cannot hear while dragging
            // is a slider you have to guess at, so every change goes straight
            // to the mixer and the file is written when the screen closes.
            let mut volumes = app.mixer_mut().volumes().clone();
            let mut changed = ui
                .add(egui::Slider::new(&mut volumes.master, 0.0..=1.0).text("master"))
                .changed();
            for bus in client::audio::Bus::ALL {
                let level = volumes.buses.entry(bus.name().to_owned()).or_insert(0.8);
                changed |= ui
                    .add(egui::Slider::new(level, 0.0..=1.0).text(bus.name()))
                    .changed();
            }
            if changed {
                app.mixer_mut().set_volumes(volumes);
                volumes_changed = true;
            }
            if !app.audio_available() {
                ui.label(
                    egui::RichText::new("no audio device found; the game is running silently")
                        .color(egui::Color32::LIGHT_YELLOW),
                );
            }

            draw_sound_attribution(app, ui);

            ui.separator();
            ui.heading("display");
            // **The debug overlay ships, and lives here.** Charter rule 18
            // makes frame pacing the metric, and every pacing question so far
            // was answered by somebody reading these numbers off their own
            // screen. A player on hardware nobody here will ever own is the
            // person best placed to measure it, so the instrument is in the
            // menu rather than behind a build flag or an undocumented key.
            let mut overlay = app.debug_overlay();
            if ui
                .checkbox(
                    &mut overlay,
                    "Debug overlay (frame timings, memory, adapter)",
                )
                .changed()
            {
                app.set_debug_overlay(overlay);
            }

            ui.separator();
            // No "Close" here: the top bar's Back is the way out of every
            // screen, and a second one further down is a second thing to learn.
            if ui.button("Reset all").clicked() {
                reset_all = true;
            }
        }
    });

    if let Some(id) = rebind {
        app.begin_rebind(&id);
    }
    if let Some(id) = reset {
        app.reset_binding(&id);
    }
    if reset_all {
        app.reset_all_bindings();
    }
    if close {
        app.toggle_settings();
    }
    if volumes_changed {
        app.mark_volumes_dirty();
    }
}

/// Hands egui a view of the world's texture atlas, once per atlas.
///
/// **Not a second copy.** The atlas is uploaded by the renderer and this
/// registers a view of that same texture, so an inventory slot and the wall
/// built from it cannot disagree about what a material looks like.
///
/// Nearest filtering: a 16-pixel tile blown up to a 48-point slot should look
/// like the blocks do, not like a smear.
fn register_atlas(surface: &mut Surface) {
    let Stage::Playing(app) = &mut surface.stage else {
        return;
    };
    if !app.take_atlas_change() {
        return;
    }
    // Freed before the replacement is registered: a session that reloads its
    // material table would otherwise leak a bind group per reload.
    if let Some(old) = surface.atlas_texture.take() {
        surface.egui_renderer.free_texture(&old);
    }
    let device = surface.gpu.device.clone();
    let Stage::Playing(app) = &mut surface.stage else {
        return;
    };
    surface.atlas_texture = Some(surface.egui_renderer.register_native_texture(
        &device,
        app.renderer().atlas_view(),
        wgpu::FilterMode::Nearest,
    ));
}

/// Draws the front screen, and reports what the player asked for.
///
/// The same egui plumbing the HUD uses, minus everything about a world: no
/// atlas to register, no scale to apply from an `App` that does not exist, and
/// nothing to save when it is over.
///
/// Returns the mod selection too, when it changed, because the screen edits a
/// copy and the window is what starts worlds from it.
fn draw_front(
    surface: &mut Surface,
    config: &mut Config,
    view: &wgpu::TextureView,
) -> (client::front::Action, Option<client::launcher::Catalogue>) {
    // **Before the input is taken, not inside the frame.** `egui_winit` turns
    // a click into points using the zoom factor as it stands when the input is
    // read, and `paint_egui` turns points back into pixels using it as it
    // stands after — so setting it in between makes a frame whose clicks and
    // whose pixels disagree, on every frame the scale changes. Set here, all
    // three agree.
    surface.egui.set_zoom_factor(config.ui_scale);
    let raw = surface.egui_state.take_egui_input(&surface.window);
    let mut action = client::front::Action::None;
    let mut dirty = false;
    let mut catalogue = None;
    let output = surface.egui.run_ui(raw, |root| {
        if let Stage::Front(front) = &mut surface.stage {
            let context = root.ctx().clone();
            action = front.draw(&context, config);
            dirty = front.take_settings_dirty();
            if front.take_catalogue_dirty() {
                catalogue = Some(front.catalogue.clone());
            }
        }
    });
    // **Written when it changes, not when the screen closes.** There is no
    // "close" on a front screen — a player presses Play, and a setting that
    // only reached the file on the way out would be lost by the one route
    // everybody takes.
    if dirty && let Err(err) = config.save(std::path::Path::new(CONFIG_FILE)) {
        tracing::warn!(%err, "could not save the settings");
    }
    surface
        .egui_state
        .handle_platform_output(&surface.window, output.platform_output);
    paint_egui(surface, output.shapes, output.textures_delta, view);
    (action, catalogue)
}

/// Draws the HUD over the frame that has just been rendered.
///
/// A second render pass that loads rather than clears, so it composites onto
/// the world instead of replacing it.
fn draw_hud(surface: &mut Surface, view: &wgpu::TextureView) {
    register_atlas(surface);
    // Before the input, for the reason spelled out in `draw_front`.
    if let Stage::Playing(app) = &surface.stage {
        surface.egui.set_zoom_factor(app.ui_scale());
    }
    let raw = surface.egui_state.take_egui_input(&surface.window);
    let Stage::Playing(app) = &mut surface.stage else {
        return;
    };
    // Empty when the overlay is off. The warnings and the joining notice below
    // are NOT part of it — those tell a player something is wrong, and a player
    // who turned off a frame-timing readout did not ask to stop being told.
    let lines = if app.debug_overlay() {
        app.hud()
    } else {
        Vec::new()
    };
    let warnings: Vec<String> = app.warnings().to_vec();
    let joined = app.joined();

    let settings_open = app.settings_open();
    let menu_open = app.menu_open();
    // Cloned out because the closure below borrows `app` mutably, and
    // the interface needs to read the atlas layout while it does.
    let tiles = app.tiles().clone();
    // Cloned out for the same reason the tiles are: the closure below borrows
    // `app` mutably and the interface has to know which materials are items
    // while it does. A set of a handful of ids.
    let items = app.items().clone();
    let atlas_texture = surface.atlas_texture;
    // **Points, not physical pixels.** `client::panel` sizes a sheet and egui
    // lays it out, and both work in points — measuring the window instead made
    // a mod's dialog a quarter larger than the engine's own screens at the
    // default interface scale, which is the same units mistake as the click
    // offset above and was hiding behind the fact that a dialog has no edge to
    // line up with.
    let size = {
        let content = surface.egui.content_rect();
        (content.width(), content.height())
    };
    let output = surface.egui.run_ui(raw, |root| {
        // **One scale for the whole interface**, set above rather than by every
        // panel: egui works in points, and the zoom factor is what a point is
        // worth. A mod's HUD scales with it — see `draw_hud_scripts`, which
        // measures its canvas in the same points.
        let context = root.ctx().clone();
        if menu_open {
            draw_menu(app, &context);
        }
        if settings_open {
            draw_settings(app, &context);
        }
        // **Server dialogs, drawn from data.** Nothing here executes anything a
        // server sent: `client::dialog` walks the tree and the rectangles
        // `core::ui` computed for it. See that module for why the layout is
        // not egui's.
        let raised = surface.dialogs.draw(
            &context,
            app.dialogs(),
            app.views(),
            client::icons::Icons::new(atlas_texture, Some(&tiles)).with_items(&items),
            size,
        );
        // **The interface makes its own noise, locally.** A click that waited
        // for the server to agree it had happened would arrive after the button
        // had already visibly moved. `engine:ui_click` is bound like any other
        // cue, so a mod set that binds nothing is silent here and that is fine.
        if !raised.is_empty() {
            app.play_cue("engine:ui_click", client::audio::Bus::Ui);
        }
        app.raise_dialog_events(raised);
        draw_chat(app, &context);
        // **Last, so a script's HUD sits over the world and under a dialog.**
        // A dialog is a thing a player is interacting with; a HUD is a thing
        // they are reading past.
        draw_hud_scripts(
            app,
            &context,
            client::icons::Icons::new(atlas_texture, Some(&tiles)).with_items(&items),
        );
        if lines.is_empty() && warnings.is_empty() && joined {
            return;
        }
        egui::Area::new(egui::Id::new("hud"))
            .fixed_pos(egui::pos2(8.0, 8.0))
            .interactable(false)
            .show(&context, |ui| {
                // A dark backing, because white text on a white world is
                // invisible — which is exactly the world this task renders.
                egui::Frame::new()
                    .fill(egui::Color32::from_black_alpha(160))
                    .inner_margin(6.0)
                    .show(ui, |ui| {
                        for line in &lines {
                            ui.label(egui::RichText::new(line).color(egui::Color32::WHITE));
                        }
                        if !joined {
                            ui.label(
                                egui::RichText::new("joining…").color(egui::Color32::LIGHT_YELLOW),
                            );
                        }
                        for warning in &warnings {
                            ui.label(
                                egui::RichText::new(warning).color(egui::Color32::LIGHT_YELLOW),
                            );
                        }
                    });
            });
    });

    surface
        .egui_state
        .handle_platform_output(&surface.window, output.platform_output);

    // Volumes and the debug-overlay toggle live in `client.toml` beside the
    // other settings. Saved on the same "the App raises a flag, the window
    // knows the path" split as the bindings below. One flag for both: they are
    // the same file, and a second flag would be a second chance to forget one.
    if app.take_volumes_dirty() {
        let mut config = app.config().clone();
        config.volumes = app.mixer_mut().volumes().clone();
        if let Err(err) = config.save(std::path::Path::new(CONFIG_FILE)) {
            tracing::warn!(%err, "could not save the volume settings");
        }
    }

    // **The window saves, because the window is what knows the path.** The
    // `App` raises a flag when a binding changes and this writes it out at most
    // once a frame — a rebind is a click, so there is nothing to batch, and a
    // failed write is reported rather than retried because the likeliest cause
    // is a read-only directory that will not fix itself.
    if app.take_bindings_dirty()
        && let Err(err) = app.bindings().save(std::path::Path::new(BINDINGS_FILE))
    {
        tracing::warn!(%err, "could not save the key bindings");
    }

    paint_egui(surface, output.shapes, output.textures_delta, view);
}

/// How many physical pixels one interface point is worth.
///
/// # The bug this is
///
/// **`egui_winit` divides a click by `zoom × window scale`; this used to
/// multiply a widget back up by the window scale alone.** With the default
/// interface scale of 1.25 that put every widget a quarter of its distance from
/// the top-left corner away from where egui thought it was — so a button had to
/// be clicked below and to the right of itself, by more the further down the
/// screen it sat. Reported from the front screen, where the buttons are far
/// enough down to make it obvious; it was just as wrong in every dialog and on
/// the settings screen, where the targets are bigger and the miss was survivable.
///
/// There is exactly one right answer and it is the one egui itself computes:
/// the zoom factor times the window's own scale. Anything that converts between
/// points and pixels must use THIS, and nothing may use `scale_factor` alone.
fn interface_scale(ctx: &egui::Context, window: &Window) -> f32 {
    egui_winit::pixels_per_point(ctx, window)
}

/// Uploads and draws whatever egui produced, over what is already there.
///
/// Shared by the front screen and the HUD: the two decide different things and
/// then hand the result to the same three calls, and a second copy of this is a
/// second place for a screen descriptor to go stale.
fn paint_egui(
    surface: &mut Surface,
    shapes: Vec<egui::epaint::ClippedShape>,
    textures: egui::TexturesDelta,
    view: &wgpu::TextureView,
) {
    let gpu = surface.gpu.clone();
    let pixels_per_point = interface_scale(&surface.egui, &surface.window);
    let triangles = surface.egui.tessellate(shapes, pixels_per_point);

    for (id, delta) in &textures.set {
        surface
            .egui_renderer
            .update_texture(&gpu.device, &gpu.queue, *id, delta);
    }

    let descriptor = egui_wgpu::ScreenDescriptor {
        size_in_pixels: [surface.size.0, surface.size.1],
        pixels_per_point,
    };
    let mut encoder = gpu
        .device
        .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("hud") });
    surface.egui_renderer.update_buffers(
        &gpu.device,
        &gpu.queue,
        &mut encoder,
        &triangles,
        &descriptor,
    );

    {
        let pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("hud"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    // LOAD, not Clear. The world is already in this buffer.
                    load: wgpu::LoadOp::Load,
                    store: wgpu::StoreOp::Store,
                },
            })],
            // No depth: the HUD is on top of everything by construction, and
            // testing it against the world's depth buffer would let a nearby
            // block occlude the frame counter.
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        surface
            .egui_renderer
            .render(&mut pass.forget_lifetime(), &triangles, &descriptor);
    }

    gpu.queue.submit(Some(encoder.finish()));

    for id in &textures.free {
        surface.egui_renderer.free_texture(id);
    }
}

/// Configures the swap chain, and reports the present mode it actually asked for.
///
/// # Why the mode is named rather than left to `Auto`
///
/// It used to ask for `AutoVsync`, which resolves to **FifoRelaxed** where that
/// exists and only falls back to `Fifo`. FifoRelaxed is vsync that is allowed to
/// present immediately when a frame arrives late — which for a renderer drawing
/// far faster than the display is most of the time, so it paces nothing. That is
/// the shape of the anomaly the HUD kept reporting: "vsync on" beside 1,200 fps,
/// which strict vsync cannot produce, and worst frames of 19–25 ms sitting almost
/// entirely in `acquire` — a queue saturated by frames nobody asked for.
///
/// `Fifo` is the one mode every backend must support, so naming it costs no
/// portability. And the name is returned so the HUD can report what is in force
/// instead of what was requested: the two disagreeing is exactly the thing that
/// took a week of frame-pacing guesses to notice.
fn configure_surface(
    surface: &wgpu::Surface<'static>,
    gpu: &Gpu,
    size: (u32, u32),
    vsync: bool,
) -> &'static str {
    let present_mode = if vsync {
        wgpu::PresentMode::Fifo
    } else {
        wgpu::PresentMode::AutoNoVsync
    };
    surface.configure(
        &gpu.device,
        &wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: gpu.surface_format(),
            width: size.0,
            height: size.1,
            present_mode,
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        },
    );

    // Named for the HUD. `AutoNoVsync` is still an Auto — it picks between
    // Immediate and Mailbox on availability — so it is reported as the request it
    // is rather than as a mode that may not be the one in force.
    if vsync { "on (Fifo)" } else { "OFF (auto)" }
}

/// The hotbar slot an action id selects, if it is a hotbar action.
///
/// Parsed from the id rather than matched arm by arm, because the nine of them
/// differ only by a digit and nine arms is nine chances to write the wrong one.
/// `engine:hotbar_1` is slot 0, the way `Digit1` was.
fn hotbar_slot(id: &str) -> Option<usize> {
    let slot: usize = id.strip_prefix("engine:hotbar_")?.parse().ok()?;
    slot.checked_sub(1)
}

/// Whether the camera should have the mouse right now.
///
/// **One rule, in one place.** Four things can want the cursor back — the pause
/// menu, the settings page, the chat line, a mod's dialog — and the decision
/// used to be written out at each of the places that could open one. Four
/// copies of a condition is four chances to get it wrong, and it WAS wrong: see
/// [`grab`] for the negation that kept mouse-look running under every one of
/// them.
const fn wants_cursor(menu: bool, chat: bool, dialog: bool) -> bool {
    !menu && !chat && !dialog
}

/// Hands the mouse to the interface, or takes it back for the camera.
///
/// **Releasing the cursor stops what the player was doing with it.** Every
/// screen that wants the pointer — chat, a mod's dialog, the pause menu, the
/// settings page — is a screen the player is no longer digging through, and
/// each of them used to release the cursor and leave the dig running. Reported
/// from the window: opening chat mid-dig took the next sub-node out of the next
/// block before it stopped, because the only things that ever called
/// [`App::stop_digging`] were the mouse release and losing focus.
///
/// So it hangs off the same transition [`wants_cursor`] already decides, in one
/// place, rather than being remembered at each of the four sites that open a
/// screen. Taking the cursor BACK does nothing: a player returning to the world
/// is not holding anything down, because the interface had the button.
fn hand_over(
    window: &Window,
    wanted: bool,
    app: &mut App,
    held: &mut Held,
    digging: &mut bool,
) -> bool {
    if !wanted {
        *held = Held::default();
        if *digging {
            *digging = false;
            app.stop_digging();
        }
    }
    grab(window, wanted)
}

/// Grabs or releases the cursor, reporting whether it is now grabbed.
///
/// **The return is the STATE, not whether the call worked**, which is the whole
/// of a bug worth naming. Every release site wrote `grabbed = !grab(w, false)`
/// — reading the result as "did it succeed" — so releasing the cursor set
/// `grabbed` to true. The cursor came back and mouse-look never stopped, so the
/// pause screen, the chat line and every mod dialog were all opened with the
/// camera still swinging under them. Reported from the window as "I hit escape
/// and I am still looking around".
///
/// Confined first, locked second: Wayland supports one and X11 the other, and
/// a client that insisted on either would refuse to grab on half of Linux.
fn grab(window: &Window, grab: bool) -> bool {
    if !grab {
        let _ = window.set_cursor_grab(CursorGrabMode::None);
        window.set_cursor_visible(true);
        return false;
    }

    let grabbed = window
        .set_cursor_grab(CursorGrabMode::Locked)
        .or_else(|_| window.set_cursor_grab(CursorGrabMode::Confined))
        .is_ok();
    window.set_cursor_visible(!grabbed);
    grabbed
}

#[cfg(test)]
mod tests {

    #[test]
    fn a_new_world_never_opens_a_forgotten_one() {
        // **Reported from the window**: forget a world called "New world",
        // make another called "New world", and the new one arrives with all
        // the old one's building in it.
        //
        // Forgetting takes a world out of the LIST and deliberately leaves its
        // files alone, so the name is free and the directory is not. Checking
        // the list would have found nothing wrong; the disk is what has to be
        // asked.
        let root = std::env::temp_dir().join("tiamot-world-names");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch");

        let first = super::unused_world_directory(&root, "New world");
        assert_eq!(first, std::path::Path::new("worlds").join("new-world"));

        // The world now exists on disk, as it would after being played once.
        std::fs::create_dir_all(root.join(&first)).expect("world dir");

        let second = super::unused_world_directory(&root, "New world");
        assert_ne!(
            second, first,
            "a second world of the same name was pointed at the first one's save"
        );
        assert!(
            !root.join(&second).exists(),
            "and the one it picked is free"
        );

        // And again, so the rule is "the next free one" rather than "one more".
        std::fs::create_dir_all(root.join(&second)).expect("world dir");
        let third = super::unused_world_directory(&root, "New world");
        assert!(third != first && third != second, "{third:?}");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_world_name_that_is_not_a_path_still_becomes_one() {
        // **Not a duplicate of the `world_directory` test below.** That one is
        // about the slug; this is about the whole path the uniquifying step
        // builds out of it, which is the part that could newly escape.
        let root = std::env::temp_dir().join("tiamot-world-names-safe");
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("scratch");
        for name in ["../../etc", "a/b", "   ", "..", ""] {
            let path = super::unused_world_directory(&root, name);
            assert!(
                path.starts_with("worlds"),
                "`{name}` produced {path:?}, which is outside the worlds directory"
            );
            assert_eq!(
                path.components().count(),
                2,
                "`{name}` produced {path:?}, which is more than one directory deep"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }
    use super::{hotbar_slot, world_directory};

    /// A click lands where the widget is drawn, whatever the interface scale.
    ///
    /// # What this pins
    ///
    /// `egui_winit` turns a physical click into points by dividing by
    /// `zoom × window scale`, and the renderer turns points back into pixels by
    /// multiplying by whatever it is handed. **Those two numbers have to be the
    /// same one**, and for a long time the renderer used the window scale alone:
    /// at the default interface scale of 1.25 every widget had to be clicked a
    /// quarter of its distance from the top-left corner below and to the right
    /// of itself.
    ///
    /// The number egui itself computes is `Context::pixels_per_point`, and this
    /// asserts it is the product — so a future change that reaches for
    /// `scale_factor` again has something to fail against. A window cannot be
    /// opened in a test, so this drives the context directly with the same
    /// `native_pixels_per_point` a window would report.
    #[test]
    fn a_click_lands_where_the_widget_is_drawn() {
        for (native, zoom) in [(1.0, 1.25), (1.0, 1.0), (2.0, 1.25), (1.5, 0.75)] {
            let ctx = egui::Context::default();
            ctx.set_zoom_factor(zoom);
            let mut input = egui::RawInput::default();
            input
                .viewports
                .entry(input.viewport_id)
                .or_default()
                .native_pixels_per_point = Some(native);
            let _ = ctx.run_ui(input, |_| {});

            let expected = native * zoom;
            assert!(
                (ctx.pixels_per_point() - expected).abs() < 1e-5,
                "at native {native} and zoom {zoom}, egui works in {} pixels per point and the \
                 renderer would have used {expected}",
                ctx.pixels_per_point()
            );
            // And the thing the bug actually did: the window scale on its own
            // is NOT the answer whenever the player has scaled the interface.
            if (zoom - 1.0).abs() > 1e-5 {
                assert!(
                    (ctx.pixels_per_point() - native).abs() > 1e-5,
                    "the window scale alone happened to be right, so this proves nothing"
                );
            }
        }
    }

    #[test]
    fn only_a_world_with_nothing_over_it_gets_the_mouse() {
        use super::wants_cursor;

        // The camera has the mouse when nothing is asking for it, and not
        // otherwise. **The bug this replaced was worse than a wrong condition:**
        // `grab` returns whether the cursor is NOW grabbed, and every release
        // site wrote `grabbed = !grab(w, false)` — reading it as "did that
        // work" — so releasing the cursor set the flag to true and mouse-look
        // carried on under the pause screen, the chat line and every dialog.
        assert!(wants_cursor(false, false, false), "nothing is in the way");
        assert!(!wants_cursor(true, false, false), "the pause menu is open");
        assert!(!wants_cursor(false, true, false), "chat is open");
        assert!(!wants_cursor(false, false, true), "a mod's dialog is open");
        // And more than one at once is still no.
        assert!(!wants_cursor(true, true, true));
        // The case the old scattered version got wrong from the other side: a
        // dialog closing must not take the cursor back into an open menu.
        assert!(!wants_cursor(true, false, false));
    }

    #[test]
    fn the_interface_scale_moves_in_steps_a_player_can_land_on() {
        // Reported from the window as "waaay too sensitive". A continuous
        // slider over the old 0.5..=3.0 moved the whole interface on a pixel of
        // travel — and the interface includes the slider, so it slid out from
        // under the pointer.
        let range = client::config::UI_SCALE_RANGE;
        let span = f64::from(*range.end() - *range.start());
        let steps = span / client::config::UI_SCALE_STEP;
        assert!(
            (8.0..=40.0).contains(&steps),
            "{steps} positions across the slider is not a scale anybody can aim at"
        );
        assert!(
            *range.start() >= 0.75 && *range.end() <= 1.25,
            "a range this wide is what made a small drag a big change; past about a quarter \
             either way the interface stops fitting the screen rather than getting clearer"
        );
        // 1.0 has to be reachable exactly, or a player cannot get back to it.
        // Counted rather than rounded: `round` is on the determinism ban list
        // and a step count is integer arithmetic anyway.
        let mut at = f64::from(*range.start());
        let mut lands_on_one = false;
        for _ in 0..64 {
            if (at - 1.0).abs() < 1e-9 {
                lands_on_one = true;
            }
            at += client::config::UI_SCALE_STEP;
        }
        assert!(
            lands_on_one,
            "the steps do not land on 1.0, so there is no way back to no scaling"
        );
    }

    #[test]
    fn a_world_name_never_becomes_a_path_that_leaves_the_worlds_directory() {
        // **A player may call a world anything.** That string is a title, not a
        // path, and a title with a `..` or a `/` in it must not be able to say
        // where the files go.
        assert_eq!(world_directory("My World"), "my-world");
        assert!(!world_directory("../../etc").contains('/'));
        assert!(!world_directory("../../etc").contains(".."));
        assert_eq!(
            world_directory("../.."),
            "world",
            "a name of nothing still opens"
        );
        assert_eq!(world_directory(""), "world");
        // Ordinary names survive intact, dashes and underscores included.
        assert_eq!(world_directory("test_world-2"), "test_world-2");
    }

    #[test]
    fn a_hotbar_action_names_the_slot_it_selects() {
        // `engine:hotbar_1` is slot 0, the way `Digit1` was before the window
        // stopped knowing about keys.
        assert_eq!(hotbar_slot("engine:hotbar_1"), Some(0));
        assert_eq!(hotbar_slot("engine:hotbar_9"), Some(8));
        // Not a hotbar action, and — the case that matters — a malformed one.
        // Parsed rather than matched arm by arm, so the parse has to refuse
        // what it cannot answer instead of selecting a slot nobody has.
        assert_eq!(hotbar_slot("engine:jump"), None);
        assert_eq!(hotbar_slot("engine:hotbar_"), None);
        assert_eq!(hotbar_slot("engine:hotbar_x"), None);
        // Slot zero has no meaning: the hotbar is one-based in the id and
        // zero-based in the array, and `checked_sub` is what stops
        // `hotbar_0` wrapping to `usize::MAX`.
        assert_eq!(hotbar_slot("engine:hotbar_0"), None);
    }
}
