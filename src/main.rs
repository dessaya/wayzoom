//! wayzoom — a view-only screen magnifier for Wayland.
//!
//! It maps a fullscreen `wlr-layer-shell` overlay with a colored border, grabs a
//! single frozen frame of the current output via `wlr-screencopy`, and lets you
//! pan/zoom that frame with the mouse. Because the overlay grabs input, you can't
//! interact with apps underneath until you press Esc — the border is the reminder.
//!
//! Why frozen: same-output live capture is impossible with wlr-screencopy (the
//! overlay would capture itself → feedback). See the project plan for details.
//!
//! Scaling is offloaded to the compositor via `wp_viewporter`: the frozen frame is
//! uploaded once, and each redraw merely moves the viewport source rectangle, so
//! the compositor does the pan/scale/filtering on the GPU. The colored border
//! lives on a static child subsurface (it must not be scaled with the content).

mod capture;
mod crop;

use capture::{CaptureFormat, SourceImage};

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm, delegate_subcompositor,
    output::{OutputHandler, OutputState},
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{
        slot::{Buffer, SlotPool},
        Shm, ShmHandler,
    },
    subcompositor::SubcompositorState,
};
use wayland_client::{
    delegate_noop,
    globals::registry_queue_init,
    protocol::{
        wl_compositor::WlCompositor, wl_keyboard, wl_output, wl_pointer, wl_region::WlRegion,
        wl_seat, wl_shm, wl_subsurface::WlSubsurface, wl_surface,
    },
    Connection, QueueHandle,
};
use wayland_protocols::wp::viewporter::client::{
    wp_viewport::WpViewport, wp_viewporter::WpViewporter,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

const BORDER_PX: u32 = 3;
const BORDER_COLOR: u32 = 0xFF_FF3B30;

const ZOOM_STEP: f32 = 1.15;
const ZOOM_MAX: f32 = 8.0;
/// exponential ease-out time constant for zoom (ms)
const ZOOM_TAU_MS: f32 = 120.0;

/// Parse CLI args. Returns whether to draw the border. Exits on `--help`/unknown.
fn parse_args() -> bool {
    let mut args = std::env::args();
    let name = args.next().expect("missing argv[0]");

    let mut border = true;
    for arg in args {
        match arg.as_str() {
            "--no-border" => border = false,
            "-h" | "--help" => {
                println!("Usage: {name} [--no-border]\n\nA view-only screen magnifier.\n\nOptions:\n  --no-border   Don't draw the reminder border around the overlay.\n  -h, --help    Show this help.");
                std::process::exit(0);
            }
            other => {
                eprintln!("{name}: unknown argument '{other}' (try --help)");
                std::process::exit(2);
            }
        }
    }
    border
}

fn main() {
    let border = parse_args();

    let conn = Connection::connect_to_env().expect("failed to connect to Wayland");
    let (globals, mut event_queue) = registry_queue_init(&conn).expect("registry init failed");
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor unavailable");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("wlr-layer-shell unavailable");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm unavailable");
    let subcompositor = SubcompositorState::bind(compositor.wl_compositor().clone(), &globals, &qh)
        .expect("wl_subcompositor unavailable");
    let viewporter: WpViewporter = globals
        .bind(&qh, 1..=1, ())
        .expect("wp_viewporter unavailable");
    let screencopy_mgr: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .expect("wlr-screencopy unavailable");

    // Fullscreen overlay on the current output (None lets the compositor pick it).
    let surface = compositor.create_surface(&qh);
    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("wayzoom"), None);
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.set_size(0, 0);
    // Initial commit with no buffer; the compositor replies with a configure.
    layer.commit();

    let viewport = viewporter.get_viewport(layer.wl_surface(), &qh, ());
    let compositor = compositor.wl_compositor().clone();
    let pool = SlotPool::new(1024, &shm).expect("failed to create shm pool");

    let mut state = AppState {
        wayland: WaylandState {
            registry: RegistryState::new(&globals),
            seat: SeatState::new(&globals, &qh),
            output: OutputState::new(&globals, &qh),
            shm,
            compositor,
            subcompositor,
            layer,
            viewport,
            screencopy_mgr,
            pool,
            keyboard: None,
            pointer: None,
            size: None,
            cursor: None,
        },
        render: RenderState {
            last_frame_time: None,
            dirty: false,
            frame_pending: false,
        },
        capture: CaptureState {
            started: false,
            frame: None,
            format: None,
            pool: None,
            buffer: None,
            y_invert: false,
            source: None,
        },

        border,
        exit: false,
        zoom: 1.0,
        zoom_target: 1.0,

        frame: None,
        border_subsurface: None,
        border_surface: None,
        border_buffer: None,
    };

    loop {
        event_queue
            .blocking_dispatch(&mut state)
            .expect("dispatch failed");
        if state.exit {
            break;
        }
    }
}

pub struct WaylandState {
    registry: RegistryState,
    seat: SeatState,
    output: OutputState,
    pub shm: Shm,
    compositor: WlCompositor,
    subcompositor: SubcompositorState,
    layer: LayerSurface,
    viewport: WpViewport,
    pub screencopy_mgr: ZwlrScreencopyManagerV1,
    pool: SlotPool,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,
    size: Option<(u32, u32)>,
    cursor: Option<(f64, f64)>,
}

pub struct RenderState {
    last_frame_time: Option<u32>,
    dirty: bool,
    frame_pending: bool,
}

pub struct CaptureState {
    started: bool,
    frame: Option<ZwlrScreencopyFrameV1>,
    pub format: Option<CaptureFormat>,
    pub pool: Option<SlotPool>,
    pub buffer: Option<Buffer>,
    pub y_invert: bool,
    pub source: Option<SourceImage>,
}

pub struct FrameState {
    w: u32,
    h: u32,
}

pub struct AppState {
    wayland: WaylandState,
    render: RenderState,
    capture: CaptureState,

    border: bool,
    pub exit: bool,
    zoom: f32,
    zoom_target: f32,

    // The frozen frame, uploaded once and kept attached to the layer surface.
    frame: Option<FrameState>,

    // Static border, kept alive for the lifetime of the overlay.
    border_subsurface: Option<WlSubsurface>,
    border_surface: Option<wl_surface::WlSurface>,
    border_buffer: Option<Buffer>,
}

impl AppState {
    /// Map the surface with a fully transparent buffer so screencopy captures the
    /// real desktop beneath us (not our own overlay).
    fn commit_transparent(&mut self, w: u32, h: u32) {
        let stride = w as i32 * 4;
        let (buffer, canvas) = self
            .wayland
            .pool
            .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
            .expect("create transparent buffer");
        canvas.fill(0); // ARGB 0x00000000 — fully transparent
        let surface = self.wayland.layer.wl_surface();
        buffer
            .attach_to(surface)
            .expect("attach transparent buffer");
        self.wayland.layer.commit();
    }

    /// Called once the frozen frame is ready: upload it as a single buffer, switch
    /// the surface to a viewport, attach the border, and present the first view.
    pub fn begin_magnify(&mut self, qh: &QueueHandle<Self>) {
        let Some((w, h)) = self.wayland.size else {
            return;
        };

        let Some(src) = self.capture.source.take() else {
            return;
        };

        // Upload the frozen frame once. Force alpha opaque (source may be XRGB).
        let stride = src.width as i32 * 4;
        let (buffer, canvas) = self
            .wayland
            .pool
            .create_buffer(
                src.width as i32,
                src.height as i32,
                stride,
                wl_shm::Format::Argb8888,
            )
            .expect("create frame buffer");
        canvas.copy_from_slice(&src.data);
        for px in canvas.chunks_exact_mut(4) {
            px[3] = 0xFF;
        }
        buffer
            .attach_to(self.wayland.layer.wl_surface())
            .expect("attach frame buffer");
        self.frame = Some(FrameState {
            w: src.width,
            h: src.height,
        });

        // The viewport maps a source crop of the frame to the full logical output.
        self.wayland.viewport.set_destination(w as i32, h as i32);

        if self.border {
            self.setup_border(qh, w, h);
        }

        if self.wayland.cursor.is_none() {
            self.wayland.cursor = Some((w as f64 / 2.0, h as f64 / 2.0));
        }
        self.render.frame_pending = false;
        self.render.dirty = true;
        self.draw(qh);
    }

    /// Build the static border as a child subsurface on top of the magnified view.
    /// Its input region is empty so the pointer still reaches the parent.
    fn setup_border(&mut self, qh: &QueueHandle<Self>, w: u32, h: u32) {
        let (lw, lh) = (w as i32, h as i32);
        let (subsurface, surface) = self
            .wayland
            .subcompositor
            .create_subsurface(self.wayland.layer.wl_surface().clone(), qh);

        let (buffer, canvas) = self
            .wayland
            .pool
            .create_buffer(lw, lh, lw * 4, wl_shm::Format::Argb8888)
            .expect("create border buffer");
        canvas.fill(0); // transparent center
        draw_border_ring(canvas, w, h, BORDER_PX, BORDER_COLOR);

        // Empty input region: pointer events fall through to the parent surface.
        let region = self.wayland.compositor.create_region(qh, ());
        surface.set_input_region(Some(&region));

        subsurface.set_position(0, 0);
        subsurface.place_above(self.wayland.layer.wl_surface());
        buffer.attach_to(&surface).expect("attach border buffer");
        surface.damage_buffer(0, 0, lw, lh);
        surface.commit(); // sync subsurface: applied on the next parent commit
        region.destroy();

        self.border_subsurface = Some(subsurface);
        self.border_surface = Some(surface);
        self.border_buffer = Some(buffer);
    }

    fn request_redraw(&mut self, qh: &QueueHandle<Self>) {
        self.render.dirty = true;
        if self.frame.is_some() && !self.render.frame_pending {
            self.render.last_frame_time = None; // restart the animation clock from idle
            self.draw(qh);
        }
    }

    /// Advance the displayed zoom toward the target with a time-based ease-out.
    /// Returns `true` while still animating (so the frame loop keeps going).
    fn step_zoom(&mut self, time: u32) -> bool {
        let dt = match self.render.last_frame_time {
            // Cap dt so an idle gap can't make the first step jump to the target.
            Some(prev) => (time.wrapping_sub(prev) as f32).min(64.0),
            None => 16.0,
        };
        self.render.last_frame_time = Some(time);

        let diff = self.zoom_target - self.zoom;
        if diff.abs() < 0.003 {
            self.zoom = self.zoom_target;
            return false;
        }
        self.zoom += diff * (1.0 - (-dt / ZOOM_TAU_MS).exp());
        true
    }

    /// Present a frame: move the viewport source rectangle and commit. No pixel
    /// work — the compositor re-samples the already-attached buffer.
    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let Some((w, h)) = self.wayland.size else {
            return;
        };
        let Some(cursor) = self.wayland.cursor else {
            return;
        };
        let Some(frame) = self.frame.as_ref() else {
            return;
        };
        let rect = crop::crop_source_rect(frame.w, frame.h, w, h, cursor, self.zoom);
        self.wayland
            .viewport
            .set_source(rect.x, rect.y, rect.w, rect.h);

        let surface = self.wayland.layer.wl_surface();
        surface.damage_buffer(0, 0, frame.w as i32, frame.h as i32);
        surface.frame(qh, surface.clone());
        self.wayland.layer.commit();
        self.render.dirty = false;
        self.render.frame_pending = true;
    }
}

/// Overwrite the outer `border_px` ring of an `Argb8888` canvas with `color`.
fn draw_border_ring(canvas: &mut [u8], w: u32, h: u32, border_px: u32, color: u32) {
    if border_px == 0 {
        return;
    }
    let bw = border_px.min(w / 2).min(h / 2);
    let bytes = color.to_le_bytes();
    for y in 0..h {
        let edge_row = y < bw || y >= h - bw;
        for x in 0..w {
            if edge_row || x < bw || x >= w - bw {
                let i = (y as usize * w as usize + x as usize) * 4;
                canvas[i..i + 4].copy_from_slice(&bytes);
            }
        }
    }
}

impl CompositorHandler for AppState {
    fn scale_factor_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: i32,
    ) {
        // The viewport destination fixes the on-screen size regardless of scale.
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        time: u32,
    ) {
        self.render.frame_pending = false;
        let animating = self.step_zoom(time);
        if animating || self.render.dirty {
            self.draw(qh);
        }
    }

    fn surface_enter(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        output: &wl_output::WlOutput,
    ) {
        // First time our (transparent) overlay lands on an output: capture it.
        if !self.capture.started {
            self.capture.started = true;
            let frame = self
                .wayland
                .screencopy_mgr
                .capture_output(0, output, qh, ());
            self.capture.frame = Some(frame);
        }
    }

    fn surface_leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: &wl_output::WlOutput,
    ) {
    }
}

impl LayerShellHandler for AppState {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        self.exit = true;
    }

    fn configure(
        &mut self,
        _: &Connection,
        _qh: &QueueHandle<Self>,
        _: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        let (w, h) = (configure.new_size.0, configure.new_size.1);
        if w != 0 && h != 0 {
            let first_configure = self.wayland.size.is_none();
            self.wayland.size = Some((w, h));
            if first_configure {
                self.commit_transparent(w, h);
            }
        }
    }
}

impl SeatHandler for AppState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.wayland.seat
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.wayland.keyboard.is_none() {
            self.wayland.keyboard = self.wayland.seat.get_keyboard(qh, &seat, None).ok();
        }
        if capability == Capability::Pointer && self.wayland.pointer.is_none() {
            self.wayland.pointer = self.wayland.seat.get_pointer(qh, &seat).ok();
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard {
            if let Some(kb) = self.wayland.keyboard.take() {
                kb.release();
            }
        }
        if capability == Capability::Pointer {
            if let Some(ptr) = self.wayland.pointer.take() {
                ptr.release();
            }
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for AppState {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
    }

    fn leave(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: &wl_surface::WlSurface,
        _: u32,
    ) {
    }

    fn press_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        event: KeyEvent,
    ) {
        if event.keysym == Keysym::Escape {
            self.exit = true;
        }
    }

    fn release_key(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: KeyEvent,
    ) {
    }

    fn update_modifiers(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        _: u32,
        _: Modifiers,
        _: u32,
    ) {
    }
}

impl PointerHandler for AppState {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        _: &wl_pointer::WlPointer,
        events: &[PointerEvent],
    ) {
        use PointerEventKind::*;
        let mut changed = false;
        for event in events {
            if &event.surface != self.wayland.layer.wl_surface() {
                continue;
            }
            match event.kind {
                Enter { .. } | Motion { .. } => {
                    self.wayland.cursor = Some(event.position);
                    changed = true;
                }
                Axis { vertical, .. } => {
                    // Wheel up is negative; up should zoom in.
                    let notches = if vertical.discrete != 0 {
                        vertical.discrete as f32
                    } else {
                        (vertical.absolute / 15.0) as f32
                    };
                    if notches != 0.0 {
                        self.zoom_target =
                            (self.zoom_target * ZOOM_STEP.powf(-notches)).clamp(1.0, ZOOM_MAX);
                        changed = true;
                    }
                }
                _ => {}
            }
        }
        if changed {
            self.request_redraw(qh);
        }
    }
}

impl OutputHandler for AppState {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.wayland.output
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for AppState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.wayland.shm
    }
}

impl ProvidesRegistryState for AppState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.wayland.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(AppState);
delegate_output!(AppState);
delegate_shm!(AppState);
delegate_seat!(AppState);
delegate_keyboard!(AppState);
delegate_pointer!(AppState);
delegate_layer!(AppState);
delegate_subcompositor!(AppState);
delegate_registry!(AppState);

delegate_noop!(AppState: WpViewporter);
delegate_noop!(AppState: WpViewport);
delegate_noop!(AppState: WlRegion);
