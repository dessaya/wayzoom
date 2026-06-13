//! `wlr-screencopy` driver: a one-shot capture of a single output into a frozen
//! [`SourceImage`]. We bind the manager, request `capture_output`, wait for the
//! `buffer_done` handshake (screencopy v3), copy into an shm buffer, and on
//! `ready` normalize the pixels (handling `y_invert`) into an owned image.

use smithay_client_toolkit::shm::slot::SlotPool;
use wayland_client::protocol::wl_shm;
use wayland_client::{Connection, Dispatch, QueueHandle, WEnum};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{self, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::{self, ZwlrScreencopyManagerV1},
};

use crate::AppState;

/// A frozen captured frame. Always 4 bytes/pixel, top-down, row-major.
///
/// Byte order matches `wl_shm` `Argb8888` on little-endian: `[B, G, R, A]`.
pub struct SourceImage {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

/// What the `buffer` event advertised for an shm-backed copy.
#[derive(Clone, Copy)]
pub struct CaptureFormat {
    pub format: wl_shm::Format,
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

fn is_supported(format: wl_shm::Format) -> bool {
    matches!(format, wl_shm::Format::Argb8888 | wl_shm::Format::Xrgb8888)
}

// The manager itself emits no events.
impl Dispatch<ZwlrScreencopyManagerV1, ()> for AppState {
    fn event(
        _: &mut Self,
        _: &ZwlrScreencopyManagerV1,
        _: zwlr_screencopy_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<ZwlrScreencopyFrameV1, ()> for AppState {
    fn event(
        state: &mut Self,
        frame: &ZwlrScreencopyFrameV1,
        event: zwlr_screencopy_frame_v1::Event,
        _: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use zwlr_screencopy_frame_v1::Event;
        match event {
            Event::Buffer {
                format: WEnum::Value(format),
                width,
                height,
                stride,
            } if is_supported(format) => {
                state.cap_format = Some(CaptureFormat {
                    format,
                    width,
                    height,
                    stride,
                });
            }
            Event::LinuxDmabuf { .. } | Event::Damage { .. } => {
                // We only use the shm path; dmabuf is ignored.
            }
            Event::BufferDone => {
                // All formats advertised; allocate an shm buffer and request the copy.
                let Some(fmt) = state.cap_format else {
                    eprintln!("wayzoom: compositor offered no usable shm capture format");
                    state.exit = true;
                    return;
                };
                let pool_size = (fmt.stride * fmt.height) as usize;
                let mut pool = match SlotPool::new(pool_size.max(1), &state.wayland.shm) {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("wayzoom: failed to create capture pool: {e}");
                        state.exit = true;
                        return;
                    }
                };
                let buffer = match pool.create_buffer(
                    fmt.width as i32,
                    fmt.height as i32,
                    fmt.stride as i32,
                    fmt.format,
                ) {
                    Ok((buffer, _canvas)) => buffer,
                    Err(e) => {
                        eprintln!("wayzoom: failed to create capture buffer: {e}");
                        state.exit = true;
                        return;
                    }
                };
                frame.copy(buffer.wl_buffer());
                state.capture_pool = Some(pool);
                state.capture_buffer = Some(buffer);
            }
            Event::Flags {
                flags: WEnum::Value(flags),
            } => {
                state.y_invert = flags.contains(zwlr_screencopy_frame_v1::Flags::YInvert);
            }
            Event::Ready { .. } => {
                let Some(fmt) = state.cap_format else { return };
                let mut pool = state.capture_pool.take();
                let buffer = state.capture_buffer.take();
                if let (Some(pool), Some(buffer)) = (pool.as_mut(), buffer.as_ref()) {
                    if let Some(canvas) = buffer.canvas(pool) {
                        let src = normalize(canvas, fmt, state.y_invert);
                        state.source = Some(src);
                        frame.destroy();
                        state.begin_magnify(qh);
                        return;
                    }
                }
                eprintln!("wayzoom: capture ready but buffer canvas was unavailable");
                state.exit = true;
            }
            Event::Failed => {
                eprintln!("wayzoom: screencopy failed");
                state.exit = true;
            }
            _ => {}
        }
    }
}

/// Copy the captured pixels into a tightly-packed, top-down [`SourceImage`],
/// undoing row padding and `y_invert`.
fn normalize(canvas: &[u8], fmt: CaptureFormat, y_invert: bool) -> SourceImage {
    let w = fmt.width as usize;
    let h = fmt.height as usize;
    let stride = fmt.stride as usize;
    let row_bytes = w * 4;
    let mut data = vec![0u8; row_bytes * h];
    for row in 0..h {
        let src_row = if y_invert { h - 1 - row } else { row };
        let s = src_row * stride;
        let d = row * row_bytes;
        data[d..d + row_bytes].copy_from_slice(&canvas[s..s + row_bytes]);
    }
    SourceImage {
        data,
        width: fmt.width,
        height: fmt.height,
    }
}
