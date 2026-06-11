//! wayzoom — a view-only screen magnifier for Wayland (developed against niri).
//!
//! It maps a fullscreen `wlr-layer-shell` overlay with a colored border, grabs a
//! single frozen frame of the current output via `wlr-screencopy`, and lets you
//! pan/zoom that frame with the mouse. Because the overlay grabs input, you can't
//! interact with apps underneath until you press Esc — the border is the reminder.
//!
//! Why frozen: same-output live capture is impossible with wlr-screencopy (the
//! overlay would capture itself → feedback). See the project plan for details.

mod capture;
mod render;

use capture::CaptureFormat;
use render::SourceImage;

use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_pointer,
    delegate_registry, delegate_seat, delegate_shm,
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
};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
    Connection, QueueHandle,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::ZwlrScreencopyFrameV1,
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

const BORDER_PX: u32 = 3;
const BORDER_COLOR: u32 = 0xFF_FF3B30; // opaque red-orange
const STEP: f32 = 1.15; // zoom multiplier per wheel notch
const MAX_ZOOM: f32 = 8.0;

fn main() {
    let conn = Connection::connect_to_env().expect("failed to connect to Wayland");
    let (globals, mut event_queue) = registry_queue_init(&conn).expect("registry init failed");
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh).expect("wl_compositor unavailable");
    let layer_shell = LayerShell::bind(&globals, &qh).expect("wlr-layer-shell unavailable");
    let shm = Shm::bind(&globals, &qh).expect("wl_shm unavailable");
    let screencopy_mgr: ZwlrScreencopyManagerV1 = globals
        .bind(&qh, 1..=3, ())
        .expect("wlr-screencopy unavailable");

    // Fullscreen overlay on the current output (None lets niri pick it).
    let surface = compositor.create_surface(&qh);
    let layer =
        layer_shell.create_layer_surface(&qh, surface, Layer::Overlay, Some("wayzoom"), None);
    layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
    layer.set_exclusive_zone(-1);
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.set_size(0, 0);
    // Initial commit with no buffer; the compositor replies with a configure.
    layer.commit();

    let pool = SlotPool::new(1024, &shm).expect("failed to create shm pool");

    let mut state = AppState {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        layer,
        screencopy_mgr,
        pool,

        keyboard: None,
        pointer: None,

        exit: false,
        first_configure: true,
        logical_w: 0,
        logical_h: 0,
        scale: 1,
        dst_w: 0,
        dst_h: 0,
        cursor: (0.0, 0.0),
        zoom: 1.0,
        dirty: false,
        frame_pending: false,

        capture_started: false,
        capture_frame: None,
        cap_format: None,
        capture_pool: None,
        capture_buffer: None,
        y_invert: false,
        source: None,
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

pub struct AppState {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    pub shm: Shm,
    layer: LayerSurface,
    pub screencopy_mgr: ZwlrScreencopyManagerV1,
    pool: SlotPool,

    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer: Option<wl_pointer::WlPointer>,

    pub exit: bool,
    first_configure: bool,
    logical_w: u32,
    logical_h: u32,
    scale: i32,
    dst_w: u32,
    dst_h: u32,
    cursor: (f64, f64),
    zoom: f32,
    dirty: bool,
    frame_pending: bool,

    capture_started: bool,
    capture_frame: Option<ZwlrScreencopyFrameV1>,
    pub cap_format: Option<CaptureFormat>,
    pub capture_pool: Option<SlotPool>,
    pub capture_buffer: Option<Buffer>,
    pub y_invert: bool,
    pub source: Option<SourceImage>,
}

impl AppState {
    /// Map the surface with a fully transparent buffer so screencopy captures the
    /// real desktop beneath us (not our own overlay).
    fn commit_transparent(&mut self) {
        let w = self.logical_w.max(1);
        let h = self.logical_h.max(1);
        let stride = w as i32 * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(w as i32, h as i32, stride, wl_shm::Format::Argb8888)
            .expect("create transparent buffer");
        canvas.fill(0); // ARGB 0x00000000 — fully transparent
        let surface = self.layer.wl_surface();
        buffer
            .attach_to(surface)
            .expect("attach transparent buffer");
        self.layer.commit();
    }

    /// Called once the frozen frame is ready: derive the output scale, switch the
    /// surface to render at physical resolution, and draw the first magnified view.
    pub fn begin_magnify(&mut self, qh: &QueueHandle<Self>) {
        let Some(src) = self.source.as_ref() else {
            return;
        };
        let scale = (src.width as f32 / self.logical_w.max(1) as f32)
            .round()
            .max(1.0) as i32;
        self.scale = scale;
        self.dst_w = self.logical_w * scale as u32;
        self.dst_h = self.logical_h * scale as u32;
        self.layer.wl_surface().set_buffer_scale(scale);
        if self.cursor == (0.0, 0.0) {
            self.cursor = (self.logical_w as f64 / 2.0, self.logical_h as f64 / 2.0);
        }
        self.frame_pending = false;
        self.dirty = true;
        self.draw(qh);
    }

    fn request_redraw(&mut self, qh: &QueueHandle<Self>) {
        self.dirty = true;
        if self.source.is_some() && !self.frame_pending {
            self.draw(qh);
        }
    }

    fn draw(&mut self, qh: &QueueHandle<Self>) {
        let Some(src) = self.source.as_ref() else {
            return;
        };
        let (dst_w, dst_h) = (self.dst_w, self.dst_h);
        if dst_w == 0 || dst_h == 0 {
            return;
        }
        let stride = dst_w as i32 * 4;
        let (buffer, canvas) = self
            .pool
            .create_buffer(dst_w as i32, dst_h as i32, stride, wl_shm::Format::Argb8888)
            .expect("create display buffer");

        // Map the cursor (surface-local logical coords) to source pixel coords.
        let cx = (self.cursor.0 as f32 / self.logical_w.max(1) as f32) * src.width as f32;
        let cy = (self.cursor.1 as f32 / self.logical_h.max(1) as f32) * src.height as f32;
        render::render(
            src,
            canvas,
            dst_w,
            dst_h,
            self.zoom,
            (cx, cy),
            BORDER_PX,
            BORDER_COLOR,
        );

        let surface = self.layer.wl_surface();
        surface.damage_buffer(0, 0, dst_w as i32, dst_h as i32);
        surface.frame(qh, surface.clone());
        buffer.attach_to(surface).expect("attach display buffer");
        self.layer.commit();
        self.dirty = false;
        self.frame_pending = true;
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
        // Scale is derived from the captured frame dimensions instead.
    }

    fn transform_changed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_surface::WlSurface,
        _: wl_output::Transform,
    ) {
    }

    fn frame(&mut self, _: &Connection, qh: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {
        self.frame_pending = false;
        if self.dirty && self.source.is_some() {
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
        if !self.capture_started {
            self.capture_started = true;
            let frame = self.screencopy_mgr.capture_output(0, output, qh, ());
            self.capture_frame = Some(frame);
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
        if configure.new_size.0 != 0 && configure.new_size.1 != 0 {
            self.logical_w = configure.new_size.0;
            self.logical_h = configure.new_size.1;
        }
        if self.first_configure {
            self.first_configure = false;
            if self.logical_w == 0 || self.logical_h == 0 {
                eprintln!("wayzoom: compositor gave a zero-size configure");
                self.exit = true;
                return;
            }
            self.commit_transparent();
        }
    }
}

impl SeatHandler for AppState {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: wl_seat::WlSeat,
        capability: Capability,
    ) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
        if capability == Capability::Pointer && self.pointer.is_none() {
            self.pointer = self.seat_state.get_pointer(qh, &seat).ok();
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
            if let Some(kb) = self.keyboard.take() {
                kb.release();
            }
        }
        if capability == Capability::Pointer {
            if let Some(ptr) = self.pointer.take() {
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
            if &event.surface != self.layer.wl_surface() {
                continue;
            }
            match event.kind {
                Enter { .. } | Motion { .. } => {
                    self.cursor = event.position;
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
                        self.zoom = (self.zoom * STEP.powf(-notches)).clamp(1.0, MAX_ZOOM);
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
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl ShmHandler for AppState {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for AppState {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
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
delegate_registry!(AppState);
