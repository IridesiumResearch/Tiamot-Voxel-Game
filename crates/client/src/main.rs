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
use client::net::Connection;
use client::render::{COLOUR_FORMAT, Gpu, Renderer};
use tiamot_core::identity::Identity;
use winit::application::ApplicationHandler;
use winit::event::{DeviceEvent, DeviceId, ElementState, MouseButton, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{KeyCode, PhysicalKey};
use winit::window::{CursorGrabMode, Window, WindowId};

/// The config file, relative to the working directory.
const CONFIG_FILE: &str = "client.toml";

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
                match button {
                    // Click to look FIRST. Until the cursor is grabbed a click
                    // is the player asking for mouse-look, not asking to dig a
                    // hole in whatever happens to be under an unaimed
                    // crosshair.
                    MouseButton::Left if pressed && !self.grabbed => {
                        self.grabbed = grab(&surface.window, true);
                    }
                    // Held, not clicked: a dig takes a second or two of ticks
                    // and the server counts them. Releasing cancels, which is
                    // why the state has to be tracked rather than acted on once.
                    MouseButton::Left => {
                        self.digging = pressed;
                        if !pressed {
                            surface.app.stop_digging();
                        }
                    }
                    // A single action, unlike digging. Repeating while held
                    // would build a wall out of one click.
                    MouseButton::Right if pressed && self.grabbed => surface.app.place(),
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
                if let PhysicalKey::Code(code) = event.physical_key {
                    match code {
                        KeyCode::KeyW => self.held.forward = pressed,
                        KeyCode::KeyS => self.held.back = pressed,
                        KeyCode::KeyA => self.held.left = pressed,
                        KeyCode::KeyD => self.held.right = pressed,
                        KeyCode::Space => self.held.up = pressed,
                        KeyCode::ShiftLeft | KeyCode::ControlLeft => self.held.down = pressed,
                        KeyCode::ShiftRight => self.held.sprint = pressed,
                        KeyCode::Escape if pressed => {
                            self.grabbed = !grab(&surface.window, false);
                        }
                        // The floating-origin check from Task 08's acceptance
                        // criteria. F8 goes out, F7 comes home. The world
                        // travels with the camera, so a working floating origin
                        // shows an identical picture from coordinates fifty
                        // thousand blocks away — the HUD's position is what
                        // moves, and the frame is what must not.
                        //
                        // Bound on a letter as well as a function key: F7 and
                        // F8 sit under Fn-lock or a vendor media overlay on a
                        // lot of Windows laptops, and the failure is silent —
                        // the key simply never arrives, which reads as "the
                        // teleport is broken" rather than "the key was eaten".
                        KeyCode::F8 | KeyCode::KeyT if pressed => {
                            self.pending_teleport = Some(Teleport::Far);
                        }
                        KeyCode::F7 | KeyCode::KeyH if pressed => {
                            self.pending_teleport = Some(Teleport::Home);
                        }
                        // Cycles the tool. A cycle rather than a fixed key
                        // per tool, because the engine does not know how many
                        // there are — charter rule 1 puts that entirely in the
                        // mods, and a server could register twenty.
                        KeyCode::KeyR if pressed => surface.app.next_tool(),
                        // The hotbar's number keys. `Digit1` is slot 0.
                        KeyCode::Digit1
                        | KeyCode::Digit2
                        | KeyCode::Digit3
                        | KeyCode::Digit4
                        | KeyCode::Digit5
                        | KeyCode::Digit6
                        | KeyCode::Digit7
                        | KeyCode::Digit8
                        | KeyCode::Digit9
                            if pressed =>
                        {
                            let slot = code as usize - KeyCode::Digit1 as usize;
                            surface.app.select_slot(slot);
                        }
                        _ => {}
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
        configure_surface(&surface, &gpu, size, self.config.vsync);

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
        let renderer = Renderer::new(gpu, self.config.render_mode, size.0, size.1)?;

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
            app: App::new(self.config.clone(), connection, renderer),
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

        let phase = std::time::Instant::now();
        if !surface.app.pump_network() {
            return false;
        }
        phases.network = elapsed_ms(phase);

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

        // Acquire and present are timed separately from the drawing between
        // them because they measure something different in kind: both block on
        // the swapchain, so time here is the GPU or the compositor holding the
        // frame rather than work this process is doing. Optimising client code
        // against a hitch that lives in these two would be chasing the wrong
        // machine entirely.
        let phase = std::time::Instant::now();
        let frame = match surface.surface.get_current_texture() {
            // Suboptimal still hands over a usable texture. Reconfiguring is
            // recommended, not required, and doing it mid-frame would drop a
            // frame every time the window was being dragged.
            wgpu::CurrentSurfaceTexture::Success(frame)
            | wgpu::CurrentSurfaceTexture::Suboptimal(frame) => frame,

            // The surface no longer matches the window. Reconfiguring and
            // skipping this frame is the whole remedy.
            wgpu::CurrentSurfaceTexture::Outdated | wgpu::CurrentSurfaceTexture::Lost => {
                configure_surface(
                    &surface.surface,
                    surface.app.renderer().gpu(),
                    surface.size,
                    self.config.vsync,
                );
                return true;
            }

            // Minimised, or the compositor is busy. Not an error, and not
            // worth a log line every frame while a window sits in the dock.
            wgpu::CurrentSurfaceTexture::Occluded | wgpu::CurrentSurfaceTexture::Timeout => {
                return true;
            }

            wgpu::CurrentSurfaceTexture::Validation => {
                self.error = Some("the surface rejected a frame request".into());
                return false;
            }
        };

        // One acquisition, two passes into the same view: the world clears and
        // draws, the HUD loads and composites over it. Acquiring twice is not a
        // shortcut that happens to work — the second call fails, and on a
        // backend where it does not, the two passes render into different
        // buffers and the HUD is never seen.
        phases.acquire = elapsed_ms(phase);

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

/// Draws the HUD over the frame that has just been rendered.
///
/// A second render pass that loads rather than clears, so it composites onto
/// the world instead of replacing it.
fn draw_hud(surface: &mut Surface, view: &wgpu::TextureView) {
    let raw = surface.egui_state.take_egui_input(&surface.window);
    let lines = surface.app.hud();
    let warnings: Vec<String> = surface.app.warnings().to_vec();
    let joined = surface.app.joined();

    let output = surface.egui.run_ui(raw, |root| {
        let context = root.ctx().clone();
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

/// Configures the swap chain.
fn configure_surface(surface: &wgpu::Surface<'static>, gpu: &Gpu, size: (u32, u32), vsync: bool) {
    surface.configure(
        &gpu.device,
        &wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: COLOUR_FORMAT,
            width: size.0,
            height: size.1,
            // FIFO is vsync and is the only mode every backend must support.
            // Immediate is not guaranteed, so a client that required it would
            // fail to start on hardware that simply cannot tear.
            present_mode: if vsync {
                wgpu::PresentMode::AutoVsync
            } else {
                wgpu::PresentMode::AutoNoVsync
            },
            alpha_mode: wgpu::CompositeAlphaMode::Auto,
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        },
    );
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
