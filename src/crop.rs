//! Crop-rectangle geometry.
//!
//! With `wp_viewporter` the compositor does the scaling/filtering on the GPU; all
//! we compute per frame is which sub-rectangle of the frozen source maps to the
//! whole output. This is the only non-trivial math left, so it lives here as a
//! pure, unit-tested function.

/// A source rectangle in source-pixel coordinates (fractional for sub-pixel pans).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// Compute the source crop centered on the cursor for a given zoom.
///
/// * `cursor` is in surface-local *logical* coordinates (the full output).
/// * `zoom` is clamped to `>= 1.0`; at `1.0` the whole source is returned.
/// * The rect is clamped so it never leaves the source image.
pub fn crop_source_rect(
    src_w: u32,
    src_h: u32,
    logical_w: u32,
    logical_h: u32,
    cursor: (f64, f64),
    zoom: f32,
) -> Rect {
    let zoom = (zoom as f64).max(1.0);
    let sw = src_w as f64;
    let sh = src_h as f64;

    let w = (sw / zoom).max(1.0);
    let h = (sh / zoom).max(1.0);

    // Cursor (logical) -> source pixels.
    let cx = cursor.0 / logical_w.max(1) as f64 * sw;
    let cy = cursor.1 / logical_h.max(1) as f64 * sh;

    let x = (cx - w / 2.0).clamp(0.0, (sw - w).max(0.0));
    let y = (cy - h / 2.0).clamp(0.0, (sh - h).max(0.0));

    Rect { x, y, w, h }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zoom_one_covers_whole_source() {
        let r = crop_source_rect(1920, 1080, 1920, 1080, (500.0, 500.0), 1.0);
        assert_eq!(r, Rect { x: 0.0, y: 0.0, w: 1920.0, h: 1080.0 });
    }

    #[test]
    fn zoom_two_is_half_size_centered_on_cursor() {
        // Cursor at the center of a 1000x1000 output == center of source.
        let r = crop_source_rect(1000, 1000, 1000, 1000, (500.0, 500.0), 2.0);
        assert_eq!(r, Rect { x: 250.0, y: 250.0, w: 500.0, h: 500.0 });
    }

    #[test]
    fn clamps_at_top_left_corner() {
        let r = crop_source_rect(1000, 1000, 1000, 1000, (0.0, 0.0), 2.0);
        assert_eq!(r, Rect { x: 0.0, y: 0.0, w: 500.0, h: 500.0 });
    }

    #[test]
    fn clamps_at_bottom_right_corner() {
        let r = crop_source_rect(1000, 1000, 1000, 1000, (1000.0, 1000.0), 2.0);
        assert_eq!(r, Rect { x: 500.0, y: 500.0, w: 500.0, h: 500.0 });
    }

    #[test]
    fn maps_logical_cursor_to_source_scale() {
        // Output is logical 1000 wide but the source is 2000px (scale 2).
        // Cursor at logical x=500 (center) -> source x=1000 (center).
        let r = crop_source_rect(2000, 2000, 1000, 1000, (500.0, 500.0), 2.0);
        assert_eq!(r, Rect { x: 500.0, y: 500.0, w: 1000.0, h: 1000.0 });
    }
}
