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
use client::render::{COLOUR_FORMAT, Gpu, Renderer};
use tiamot_core::identity::Identity;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{CursorGrabMode, Window, WindowId};

/// The config file, relative to the working directory.
const CONFIG_FILE: &str = "client.toml";

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

    // An embedded server is started BEFORE the window, so a world that fails to
    // open is a clear error on the terminal rather than a window that appears
    // and then vanishes.
    let (address, embedded) = match config.server {
        ServerChoice::Remote(address) => (address, None),
        ServerChoice::Embedded => {
            let handle = tiamot_server::ServerHandle::start(&tiamot_server::Settings {
                // Loopback only. An embedded server is this player's world, and
                // binding it to every interface would silently publish it.
                bind_addr: "127.0.0.1:0".parse()?,
                world_path: config.world_path.clone(),
                max_players: 1,
                allowlist: tiamot_core::identity::Allowlist::open(),
                view_distance: config.view(),
                mods_path: Some(std::path::PathBuf::from("game")),
                seed: None,
                rcon: None,
                materials: Vec::new(),
            })?;
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

    let event_loop = EventLoop::new()?;
    // Poll rather than Wait: this is a game, and a frame is due whether or not
    // an input event arrived.
    event_loop.set_control_flow(ControlFlow::Poll);

    let mut client = Client {
        config,
        connection: Some(connection),
        embedded,
        window: None,
        held: Held::default(),
        bindings: Some(bindings),
        last_frame: std::time::Instant::now(),
        pending_teleport: None,
        grabbed: false,
        digging: false,
        error: None,
    };
    event_loop.run_app(&mut client)?;

    client.error.map_or(Ok(()), Err)
}

/// The window, the surface, and everything drawn into it.
struct Surface {
    window: Arc<Window>,
    surface: wgpu::Surface<'static>,
    app: App,
    egui: egui::Context,
    egui_state: egui_winit::State,
    egui_renderer: egui_wgpu::Renderer,
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
            teleport,
        };
        // Mouse movement is a delta, so it is consumed rather than held: a
        // frame that read it twice would turn the camera twice as fast at low
        // frame rates, which is the worst time for it.
        self.look = (0.0, 0.0);
        input
    }
}

struct Client {
    config: Config,
    /// Taken when the window is created.
    connection: Option<Connection>,
    /// Kept alive for as long as the client runs. Dropping it stops the world.
    embedded: Option<tiamot_server::ServerHandle>,
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
        let response = surface.egui_state.on_window_event(&surface.window, &event);
        if response.consumed {
            return;
        }

        match event {
            WindowEvent::CloseRequested => event_loop.exit(),

            WindowEvent::Resized(size) => {
                surface.size = (size.width.max(1), size.height.max(1));
                configure_surface(
                    &surface.surface,
                    surface.app.renderer().gpu(),
                    surface.size,
                    self.config.vsync,
                );
            }

            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = state == ElementState::Pressed;
                // **Click to look FIRST, and before any action lookup.** Until
                // the cursor is grabbed a click is the player asking for
                // mouse-look, not asking to dig a hole under an unaimed
                // crosshair — and that is a property of the window rather than
                // of whatever `engine:dig` happens to be bound to.
                if button == MouseButton::Left && pressed && !self.grabbed {
                    self.grabbed = grab(&surface.window, true);
                    return;
                }
                // The same rule for the mouse: a capture owns the button.
                if surface.app.rebinding().is_some() {
                    if pressed {
                        surface.app.capture(Control::Mouse(button));
                    }
                    return;
                }
                let Some(action) = surface.app.action_for(Control::Mouse(button)) else {
                    return;
                };
                match action.as_str() {
                    // Held, not clicked: a dig takes a second or two of ticks
                    // and the server counts them. Releasing cancels, which is
                    // why the state is tracked rather than acted on once.
                    "engine:dig" => {
                        self.digging = pressed;
                        if !pressed {
                            surface.app.stop_digging();
                        }
                    }
                    // A single action, unlike digging. Repeating while held
                    // would build a wall out of one click.
                    "engine:place" if pressed && self.grabbed => surface.app.place(),
                    _ => {}
                }
            }

            // The hotbar, on the wheel as well as the number keys.
            WindowEvent::MouseWheel { delta, .. } => {
                let forward = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, y) => y < 0.0,
                    winit::event::MouseScrollDelta::PixelDelta(position) => position.y < 0.0,
                };
                surface.app.select_next(forward);
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
                if surface.app.rebinding().is_some() {
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
                        surface.app.cancel_rebind();
                        return;
                    }
                    // Otherwise this key is the answer. Taken here rather than
                    // acted on, or rebinding would also fire whatever the key
                    // currently does — at best a jump, at worst the very thing
                    // being rebound away from.
                    surface.app.capture(Control::Key(code));
                    return;
                }
                let Some(action) = surface.app.action_for(Control::Key(code)) else {
                    return;
                };
                match action.as_str() {
                    "engine:move_forward" => self.held.forward = pressed,
                    "engine:move_back" => self.held.back = pressed,
                    "engine:move_left" => self.held.left = pressed,
                    "engine:move_right" => self.held.right = pressed,
                    "engine:jump" => self.held.up = pressed,
                    "engine:sneak" | "engine:sneak_alt" => self.held.down = pressed,
                    "engine:sprint" => self.held.sprint = pressed,
                    "engine:settings" if pressed => {
                        surface.app.toggle_settings();
                        // The cursor has to come back to click anything.
                        self.grabbed = if surface.app.settings_open() {
                            !grab(&surface.window, false)
                        } else {
                            grab(&surface.window, true)
                        };
                    }
                    "engine:menu" if pressed => {
                        self.grabbed = !grab(&surface.window, false);
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
                    "engine:next_tool" if pressed => surface.app.next_tool(),
                    "engine:lighting_mode" | "engine:lighting_mode_alt" if pressed => {
                        surface.app.cycle_lighting_mode();
                    }
                    // Its own control rather than part of the lighting mode:
                    // the cascades are the largest thing the client allocates
                    // and the right setting depends entirely on the card.
                    "engine:shadow_quality" if pressed => surface.app.cycle_shadow_quality(),
                    "engine:third_person" | "engine:third_person_alt" if pressed => {
                        surface.app.toggle_third_person();
                    }
                    "engine:chunk_borders" if pressed => {
                        let on = surface.app.toggle_chunk_borders();
                        tracing::info!(on, "chunk borders");
                    }
                    // Temporary, for tracking sources: a source and a full flow
                    // block look identical, so from inside a pond there is no
                    // telling which block is feeding it.
                    "engine:fluid_sources" if pressed => {
                        let on = surface.app.toggle_fluid_sources();
                        tracing::info!(on, "fluid source outlines");
                    }
                    // A twentieth of a day a press, so a full circuit is twenty
                    // presses and dawn is findable.
                    "engine:time_back" | "engine:time_back_alt" if pressed => {
                        surface.app.nudge_time(-0.05);
                    }
                    "engine:time_forward" | "engine:time_forward_alt" if pressed => {
                        surface.app.nudge_time(0.05);
                    }
                    "engine:time_resync" | "engine:time_resync_alt" if pressed => {
                        surface.app.resync_time();
                    }
                    // Singleplayer only, and through the embedded server's own
                    // handle rather than over the wire — a client cannot edit a
                    // world it is a guest in, and this does not make it one.
                    "engine:material_row" if pressed => {
                        if let Some(server) = self.embedded.as_ref() {
                            for (pos, material) in surface.app.debug_material_row() {
                                server.seed_block(pos, material);
                            }
                        }
                    }
                    id => {
                        if let Some(slot) = hotbar_slot(id).filter(|_| pressed) {
                            surface.app.select_slot(slot);
                        } else {
                            // Anything else is a mod's, and the mod is told
                            // BOTH edges so it can implement a held control.
                            // `send_action` drops engine ids, so an unhandled
                            // arm above cannot leak onto the wire.
                            surface.app.send_action(id, pressed);
                        }
                    }
                }
            }

            WindowEvent::RedrawRequested if !self.frame() => event_loop.exit(),

            _ => {}
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
        if let Some(surface) = self.window.take() {
            surface.app.shutdown();
        }
        if let Some(handle) = self.embedded.take() {
            handle.stop();
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
            COLOUR_FORMAT,
            egui_wgpu::RendererOptions {
                // No MSAA and no depth: the HUD is flat text drawn last, and
                // testing it against the world's depth buffer would let a
                // nearby block occlude the frame counter.
                msaa_samples: 1,
                depth_stencil_format: None,
                ..Default::default()
            },
        );
        let mut renderer = Renderer::new(gpu, self.config.render_mode, size.0, size.1)?;
        renderer.set_lighting_mode(self.config.lighting_mode);
        renderer.set_shadow_quality(self.config.shadow_quality);

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

        let connection = self
            .connection
            .take()
            .ok_or("the connection was already taken; the window was created twice")?;

        Ok(Surface {
            window,
            surface,
            app: {
                let mut app = App::with_bindings(
                    self.config.clone(),
                    connection,
                    renderer,
                    self.bindings.take().unwrap_or_default(),
                );
                // So the HUD reports the mode in force rather than the flag that
                // asked for one. See `configure_surface`.
                app.set_present_mode(present_mode);
                // The environment belongs to the binary, not to `App`: a library
                // reading it would be process-global state a caller cannot
                // control, which is a poor thing for tests and a worse one for an
                // embedded server.
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
            },
            egui,
            egui_state,
            egui_renderer,
            size,
        })
    }

    /// Draws one frame. Returns whether to keep going.
    fn frame(&mut self) -> bool {
        let Some(surface) = self.window.as_mut() else {
            return false;
        };

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
                    surface.app.renderer().gpu(),
                    surface.size,
                    self.config.vsync,
                );
                phases.acquire = elapsed_ms(phase);
                surface.app.log_frame(&phases, false);
                return true;
            }

            // Minimised, or the compositor is busy. Not an error, and not worth
            // a log line every frame while a window sits in the dock — but it IS
            // worth a row in the frame log, because a frame that produced no
            // picture is exactly what the log exists to count.
            wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => {
                phases.acquire = elapsed_ms(phase);
                surface.app.log_frame(&phases, false);
                return true;
            }

            wgpu::CurrentSurfaceTexture::Validation => {
                self.error = Some("the surface rejected a frame request".into());
                return false;
            }
        };
        phases.acquire = elapsed_ms(phase);

        let phase = std::time::Instant::now();
        if !surface.app.pump_network() {
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
        surface.app.play_heard();
        surface.app.play_footsteps();

        let phase = std::time::Instant::now();
        surface.app.remesh();
        phases.remesh = elapsed_ms(phase);

        // `advance` reports the input itself, once per simulation tick rather
        // than once per frame. Reporting per frame sent the same tick number
        // repeatedly on a fast machine and skipped ticks on a slow one, and the
        // server's input queue is keyed by tick.
        let phase = std::time::Instant::now();
        let input = self.held.as_input(self.pending_teleport.take());
        surface.app.advance(input, dt);

        // After `advance`, so the dig aims at where the player ended up this
        // frame rather than where they started. Re-sent every frame the button
        // is held: re-aiming at the same cell keeps its progress, so this is
        // free, and it means a dig follows the crosshair rather than sticking
        // to whatever was under it when the button went down.
        if self.digging {
            surface.app.dig();
        }
        phases.advance = elapsed_ms(phase);

        let view = frame
            .texture
            .create_view(&wgpu::TextureViewDescriptor::default());
        let camera = *surface.app.camera();

        let phase = std::time::Instant::now();
        surface.app.renderer().render(&view, &camera, surface.size);
        phases.world = elapsed_ms(phase);

        let phase = std::time::Instant::now();
        draw_hud(surface, &view);
        phases.hud = elapsed_ms(phase);

        let phase = std::time::Instant::now();
        frame.present();
        phases.present = elapsed_ms(phase);
        // Only here, at the one place a frame becomes a picture. Now that the
        // image is acquired first, the count should track the frame rate closely
        // — a gap means frames are still being built and dropped, which is the
        // thing this was added to catch.
        surface.app.note_presented();
        surface.app.log_frame(&phases, true);

        // Paired with the `dt` measured at the top of the NEXT frame, which is
        // what actually measures this one.
        surface.app.record_phases(phases);
        true
    }
}

/// Milliseconds since an instant, for one phase of a frame.
fn elapsed_ms(since: std::time::Instant) -> f32 {
    since.elapsed().as_secs_f32() * 1000.0
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

    egui::Window::new("Controls")
        .collapsible(false)
        .default_width(520.0)
        .show(ctx, |ui| {
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
            egui::ScrollArea::vertical()
                .max_height(420.0)
                .show(ui, |ui| {
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
                });
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

            ui.separator();
            ui.horizontal(|ui| {
                if ui.button("Reset all").clicked() {
                    reset_all = true;
                }
                if ui.button("Close").clicked() {
                    close = true;
                }
            });
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

/// Draws the HUD over the frame that has just been rendered.
///
/// A second render pass that loads rather than clears, so it composites onto
/// the world instead of replacing it.
fn draw_hud(surface: &mut Surface, view: &wgpu::TextureView) {
    let raw = surface.egui_state.take_egui_input(&surface.window);
    let lines = surface.app.hud();
    let warnings: Vec<String> = surface.app.warnings().to_vec();
    let joined = surface.app.joined();

    let settings_open = surface.app.settings_open();
    let output = surface.egui.run_ui(raw, |root| {
        let context = root.ctx().clone();
        if settings_open {
            draw_settings(&mut surface.app, &context);
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

    // Volumes live in `client.toml` beside the other settings. Saved on the
    // same "the App raises a flag, the window knows the path" split as the
    // bindings below.
    if surface.app.take_volumes_dirty() {
        let mut config = surface.app.config().clone();
        config.volumes = surface.app.mixer_mut().volumes().clone();
        if let Err(err) = config.save(std::path::Path::new(CONFIG_FILE)) {
            tracing::warn!(%err, "could not save the volume settings");
        }
    }

    // **The window saves, because the window is what knows the path.** The
    // `App` raises a flag when a binding changes and this writes it out at most
    // once a frame — a rebind is a click, so there is nothing to batch, and a
    // failed write is reported rather than retried because the likeliest cause
    // is a read-only directory that will not fix itself.
    if surface.app.take_bindings_dirty()
        && let Err(err) = surface
            .app
            .bindings()
            .save(std::path::Path::new(BINDINGS_FILE))
    {
        tracing::warn!(%err, "could not save the key bindings");
    }

    let gpu = surface.app.renderer().gpu();
    let pixels_per_point = surface.window.scale_factor() as f32;
    let triangles = surface.egui.tessellate(output.shapes, pixels_per_point);

    for (id, delta) in &output.textures_delta.set {
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

    for id in &output.textures_delta.free {
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
            format: COLOUR_FORMAT,
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

/// Grabs or releases the cursor, reporting whether it is now grabbed.
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
    use super::hotbar_slot;

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
