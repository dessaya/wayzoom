//! Pure CPU magnify blit.
//!
//! Given a frozen captured frame ([`SourceImage`]), crop a region around a focus
//! point and nearest-neighbor scale it to fill the destination buffer, then draw
//! a colored border ring. Kept free of any Wayland types so it can be unit-tested
//! in isolation.

/// A frozen captured frame. Always 4 bytes/pixel, top-down, row-major.
///
/// Byte order matches `wl_shm` `Argb8888` on little-endian: `[B, G, R, A]`.
pub struct SourceImage {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
}

impl SourceImage {
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }
}

/// Render the magnified view into `dst` (`dst_w * dst_h * 4` bytes, `Argb8888`).
///
/// Source pixels are bilinearly interpolated, so magnified content (especially
/// text) looks smooth rather than blocky.
///
/// * `zoom` is clamped to `>= 1.0`; at `1.0` the whole source maps to the whole
///   destination (no panning).
/// * `center` is the focus point in *source pixel* coordinates; the crop is
///   centered there and clamped to stay inside the source.
#[allow(clippy::too_many_arguments)]
pub fn render(
    src: &SourceImage,
    dst: &mut [u8],
    dst_w: u32,
    dst_h: u32,
    zoom: f32,
    center: (f32, f32),
    border_px: u32,
    border_color: u32,
) {
    let zoom = zoom.max(1.0);
    let src_w = src.width as f32;
    let src_h = src.height as f32;

    // Size of the source crop that gets scaled up to fill the destination.
    let crop_w = (src_w / zoom).max(1.0);
    let crop_h = (src_h / zoom).max(1.0);

    // Top-left of the crop, clamped so it never leaves the source image.
    let x0 = (center.0 - crop_w / 2.0).clamp(0.0, (src_w - crop_w).max(0.0));
    let y0 = (center.1 - crop_h / 2.0).clamp(0.0, (src_h - crop_h).max(0.0));

    let sx_step = crop_w / dst_w as f32;
    let sy_step = crop_h / dst_h as f32;

    for dy in 0..dst_h {
        let syf = y0 + dy as f32 * sy_step;
        let dst_row = dy as usize * dst_w as usize * 4;
        for dx in 0..dst_w {
            let sxf = x0 + dx as f32 * sx_step;
            let di = dst_row + dx as usize * 4;
            sample_bilinear(src, sxf, syf, &mut dst[di..di + 4]);
        }
    }

    draw_border(dst, dst_w, dst_h, border_px, border_color);
}

/// Bilinearly sample the source at (`x`, `y`) source-pixel coordinates, writing
/// `[B, G, R, A]` into `out` with a forced-opaque alpha.
fn sample_bilinear(src: &SourceImage, x: f32, y: f32, out: &mut [u8]) {
    let x = x.clamp(0.0, (src.width - 1) as f32);
    let y = y.clamp(0.0, (src.height - 1) as f32);
    let x0 = x.floor() as u32;
    let y0 = y.floor() as u32;
    let x1 = (x0 + 1).min(src.width - 1);
    let y1 = (y0 + 1).min(src.height - 1);
    let fx = x - x0 as f32;
    let fy = y - y0 as f32;

    let stride = src.stride();
    let at = |px: u32, py: u32, c: usize| src.data[py as usize * stride + px as usize * 4 + c] as f32;

    for (c, o) in out.iter_mut().enumerate().take(3) {
        let top = at(x0, y0, c) * (1.0 - fx) + at(x1, y0, c) * fx;
        let bot = at(x0, y1, c) * (1.0 - fx) + at(x1, y1, c) * fx;
        *o = (top * (1.0 - fy) + bot * fy).round() as u8;
    }
    out[3] = 0xFF; // force opaque (source may be XRGB)
}

/// Overwrite the outer `border_px` ring of `dst` with `color` (0xAARRGGBB).
fn draw_border(dst: &mut [u8], w: u32, h: u32, border_px: u32, color: u32) {
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
                dst[i..i + 4].copy_from_slice(&bytes);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 2x2 source with four distinct pixels; values encode (x,y) in the R channel.
    fn src_2x2() -> SourceImage {
        // [B, G, R, A] per pixel.
        let data = vec![
            10, 20, 30, 0, // (0,0)
            11, 21, 31, 0, // (1,0)
            12, 22, 32, 0, // (0,1)
            13, 23, 33, 0, // (1,1)
        ];
        SourceImage { data, width: 2, height: 2 }
    }

    #[test]
    fn zoom_one_is_identity_with_forced_alpha() {
        let src = src_2x2();
        let mut dst = vec![0u8; 2 * 2 * 4];
        render(&src, &mut dst, 2, 2, 1.0, (1.0, 1.0), 0, 0);
        let expected = vec![
            10, 20, 30, 0xFF, //
            11, 21, 31, 0xFF, //
            12, 22, 32, 0xFF, //
            13, 23, 33, 0xFF, //
        ];
        assert_eq!(dst, expected);
    }

    #[test]
    fn full_border_fills_everything() {
        let src = src_2x2();
        let mut dst = vec![0u8; 2 * 2 * 4];
        // border_px huge -> clamped to w/2,h/2 = 1, which for 2x2 covers all pixels.
        render(&src, &mut dst, 2, 2, 1.0, (1.0, 1.0), 100, 0xFF_FF3B30);
        let px = 0xFF_FF3B30u32.to_le_bytes();
        for chunk in dst.chunks_exact(4) {
            assert_eq!(chunk, px);
        }
    }

    #[test]
    fn crop_clamps_at_corner_without_panic() {
        let src = src_2x2();
        let mut dst = vec![0u8; 4 * 4 * 4];
        // High zoom + center far outside bounds: must clamp, sample top-left pixel.
        render(&src, &mut dst, 4, 4, 8.0, (-100.0, -100.0), 0, 0);
        // Crop is clamped into the top-left corner, so every pixel is a bilinear
        // blend of the four source pixels (R channels 30..=33).
        for chunk in dst.chunks_exact(4) {
            assert!((30..=33).contains(&chunk[2]), "R={} out of range", chunk[2]);
        }
    }

    #[test]
    fn bilinear_blends_between_pixels() {
        // 2x1 source: R channel 0 on the left, 200 on the right.
        let data = vec![
            0, 0, 0, 0, // (0,0) R=0
            0, 0, 200, 0, // (1,0) R=200
        ];
        let src = SourceImage { data, width: 2, height: 1 };
        let mut dst = vec![0u8; 4 * 4];
        // zoom 1, dst width 4: sample x positions 0.0, 0.5, 1.0, 1.5.
        render(&src, &mut dst, 4, 1, 1.0, (0.0, 0.0), 0, 0);
        let r = |x: usize| dst[x * 4 + 2];
        assert_eq!(r(0), 0); // exact left pixel
        assert_eq!(r(1), 100); // halfway -> mean(0, 200)
        assert_eq!(r(2), 200); // exact right pixel
        assert_eq!(r(3), 200); // clamped past the edge
    }
}
