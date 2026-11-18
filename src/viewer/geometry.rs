//! Where the picture is in the window, and where a point of the window is on the device.
//!
//! Everything here is plain arithmetic, without a window or a GPU, so that it can be tested.
//!
//! The words, as `docs/protocol.md` uses them:
//!
//! * the **framebuffer** is what the device encodes: `width`x`height` of the `config` event;
//! * the **upright picture** is the framebuffer turned clockwise by the config's `rotation`, which is what a person
//!   looking at the device sees: `width`x`height` for 0 and 180, `height`x`width` for 90 and 270;
//! * touches are sent in pixels of the upright picture, so the mapping from the window to the device does not
//!   depend on the rotation but for the upright size. The rotation only matters to the renderer ([`Rotation::sample`]
//!   says which framebuffer point an upright point is).

use crate::frame::Event;

/// How far the framebuffer is turned clockwise to be upright.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Rotation {
    #[default]
    R0,
    R90,
    R180,
    R270,
}

impl Rotation {
    /// From degrees: 0, 90, 180 or 270 (a full turn more or less is the same). Anything else is not a rotation of the
    /// display.
    #[must_use]
    pub fn from_degrees(degrees: u16) -> Option<Rotation> {
        match degrees % 360 {
            0 => Some(Rotation::R0),
            90 => Some(Rotation::R90),
            180 => Some(Rotation::R180),
            270 => Some(Rotation::R270),
            _ => None,
        }
    }

    #[must_use]
    pub fn degrees(self) -> u16 {
        match self {
            Rotation::R0 => 0,
            Rotation::R90 => 90,
            Rotation::R180 => 180,
            Rotation::R270 => 270,
        }
    }

    /// Quarter turns clockwise: 0 to 3 (what the shader is given).
    #[must_use]
    pub fn quarter_turns(self) -> u32 {
        u32::from(self.degrees() / 90)
    }

    /// Whether width and height trade places between the framebuffer and the upright picture.
    #[must_use]
    pub fn swaps_axes(self) -> bool {
        matches!(self, Rotation::R90 | Rotation::R270)
    }

    /// The point of the framebuffer that shows at the point `(u, v)` of the upright picture, both as fractions of
    /// the width and height from the top left (`v` grows downwards). The shader does the same; the tests check
    /// both against the corners.
    ///
    /// Turning the framebuffer clockwise by a quarter takes its top left corner to the top right corner of the
    /// upright picture: the upright `(1, 0)` is the framebuffer's `(0, 0)`.
    #[must_use]
    pub fn sample(self, u: f64, v: f64) -> (f64, f64) {
        match self {
            Rotation::R0 => (u, v),
            Rotation::R90 => (v, 1.0 - u),
            Rotation::R180 => (1.0 - u, 1.0 - v),
            Rotation::R270 => (1.0 - v, u),
        }
    }
}

/// The size and orientation of the picture, from a `config` event.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PictureGeometry {
    /// The framebuffer's visible size.
    pub width: u32,
    pub height: u32,
    pub rotation: Rotation,
}

impl PictureGeometry {
    /// What a `config` event says (`None` for any other event, or a size of zero). A rotation the event does not
    /// give, or one that is not a quarter turn, is upright.
    #[must_use]
    pub fn from_event(event: &Event) -> Option<PictureGeometry> {
        let Event::Config { width, height, rotation, .. } = event else {
            return None;
        };
        if *width == 0 || *height == 0 {
            return None;
        }
        Some(PictureGeometry {
            width: *width,
            height: *height,
            rotation: rotation.and_then(Rotation::from_degrees).unwrap_or_default(),
        })
    }

    /// The upright picture's size: the framebuffer's, with the sides exchanged for a quarter turn.
    #[must_use]
    pub fn upright(&self) -> (u32, u32) {
        if self.rotation.swaps_axes() { (self.height, self.width) } else { (self.width, self.height) }
    }
}

/// A rectangle of the window, in physical pixels from its top left.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    #[must_use]
    pub fn contains(&self, x: f64, y: f64) -> bool {
        x >= self.x && x < self.x + self.w && y >= self.y && y < self.y + self.h
    }
}

/// The part of a window of `window` pixels that shows a picture of `upright` pixels: the largest rectangle of the
/// picture's shape that fits, centred, whole pixels. The rest of the window is the bars. `None` when the window or the
/// picture has no area.
#[must_use]
pub fn letterbox(window: (u32, u32), upright: (u32, u32)) -> Option<Rect> {
    let (ww, wh) = (f64::from(window.0), f64::from(window.1));
    let (pw, ph) = (f64::from(upright.0), f64::from(upright.1));
    if ww < 1.0 || wh < 1.0 || pw < 1.0 || ph < 1.0 {
        return None;
    }
    // Wider than the window allows: it is the width that limits it; otherwise the height.
    let (w, h) =
        if ww * ph <= wh * pw { (ww, (ww * ph / pw).round().max(1.0)) } else { ((wh * pw / ph).round().max(1.0), wh) };
    Some(Rect { x: ((ww - w) / 2.0).floor(), y: ((wh - h) / 2.0).floor(), w, h })
}

/// The point of the window `(x, y)` (physical pixels) on the upright picture, in pixels of it, or `None` in the bars.
#[must_use]
pub fn window_to_device(window: (u32, u32), upright: (u32, u32), at: (f64, f64)) -> Option<(f64, f64)> {
    let r = letterbox(window, upright)?;
    if !r.contains(at.0, at.1) {
        return None;
    }
    Some(scale_into(&r, upright, at))
}

/// The same, with a point in the bars (or outside the window) taken to the nearest point of the picture's edge: what a
/// drag that leaves the picture is, until the finger lifts. `None` only when there is no picture to point at.
#[must_use]
pub fn window_to_device_clamped(window: (u32, u32), upright: (u32, u32), at: (f64, f64)) -> Option<(f64, f64)> {
    let r = letterbox(window, upright)?;
    Some(scale_into(&r, upright, at))
}

fn scale_into(r: &Rect, upright: (u32, u32), at: (f64, f64)) -> (f64, f64) {
    let fx = ((at.0 - r.x) / r.w).clamp(0.0, 1.0);
    let fy = ((at.1 - r.y) / r.h).clamp(0.0, 1.0);
    (fx * f64::from(upright.0), fy * f64::from(upright.1))
}

/// The size to open the window at: the picture's shape at the largest size within `max` (logical pixels), and within
/// `limit_w` and `limit_h` when the user gave them. Never below 1x1, never larger than the picture itself (a window
/// that magnifies a picture is not a better view of it).
#[must_use]
pub fn fit_size(upright: (u32, u32), max: (u32, u32), limit_w: Option<u32>, limit_h: Option<u32>) -> (u32, u32) {
    let (pw, ph) = (f64::from(upright.0.max(1)), f64::from(upright.1.max(1)));
    let mut bound_w = f64::from(max.0.max(1));
    let mut bound_h = f64::from(max.1.max(1));
    if let Some(w) = limit_w {
        bound_w = f64::from(w.max(1));
    }
    if let Some(h) = limit_h {
        bound_h = f64::from(h.max(1));
    }
    // The user's own limit is what they asked for: it may exceed the picture. The screen's is only a bound.
    let scale = (bound_w / pw).min(bound_h / ph);
    let scale = if limit_w.is_some() || limit_h.is_some() { scale } else { scale.min(1.0) };
    (whole_pixels(pw * scale), whole_pixels(ph * scale))
}

/// The size the window takes when the picture's shape changes (the first `config`, a turn of the device): the new
/// shape within a square as long as the window's longer side (so that a window the user made large stays large), and
/// within `max`. Without a window yet, the size of the first window ([`fit_size`] with the user's limits).
#[must_use]
pub fn refit_size(
    upright: (u32, u32),
    current: Option<(u32, u32)>,
    max: (u32, u32),
    limit_w: Option<u32>,
    limit_h: Option<u32>,
) -> (u32, u32) {
    match current {
        None => fit_size(upright, max, limit_w, limit_h),
        Some((w, h)) => {
            let long = w.max(h).max(1);
            fit_size(upright, (long.min(max.0.max(1)), long.min(max.1.max(1))), None, None)
        }
    }
}

/// A length in pixels as a whole number of them: rounded, and at least 1.
#[must_use]
pub fn whole_pixels(v: f64) -> u32 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped into the range of a u32 first"
    )]
    let pixels = v.round().clamp(1.0, f64::from(u32::MAX)) as u32;
    pixels
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(width: u32, height: u32, rotation: Option<u16>) -> Event {
        Event::Config {
            codec: "avc1.640033".into(),
            width,
            height,
            coded_width: width + 10,
            coded_height: height + 34,
            fps: 60,
            clock_hz: 24000,
            clock_source: "default".into(),
            ssrc: 1,
            device_ssrc: 2,
            rotation,
            point_scale: Some(3.0),
            current_orientation: None,
            native_orientation: None,
        }
    }

    fn near(a: (f64, f64), b: (f64, f64)) {
        assert!((a.0 - b.0).abs() < 1e-6 && (a.1 - b.1).abs() < 1e-6, "{a:?} != {b:?}");
    }

    #[test]
    fn rotations_come_from_degrees_and_back() {
        for (d, r) in [(0, Rotation::R0), (90, Rotation::R90), (180, Rotation::R180), (270, Rotation::R270)] {
            assert_eq!(Rotation::from_degrees(d), Some(r));
            assert_eq!(r.degrees(), d);
            assert_eq!(u16::try_from(r.quarter_turns() * 90).unwrap(), d);
        }
        assert_eq!(Rotation::from_degrees(360), Some(Rotation::R0));
        assert_eq!(Rotation::from_degrees(450), Some(Rotation::R90));
        assert_eq!(Rotation::from_degrees(45), None);
        assert!(!Rotation::R0.swaps_axes() && !Rotation::R180.swaps_axes());
        assert!(Rotation::R90.swaps_axes() && Rotation::R270.swaps_axes());
    }

    #[test]
    fn the_config_gives_the_geometry_and_the_upright_size() {
        // The iPhone's framebuffer: 1206x2622. Turned a quarter, the picture is wide.
        let up = |r| PictureGeometry::from_event(&config(1206, 2622, r)).unwrap();
        assert_eq!(up(None).upright(), (1206, 2622));
        assert_eq!(up(Some(0)).upright(), (1206, 2622));
        assert_eq!(up(Some(90)).upright(), (2622, 1206));
        assert_eq!(up(Some(180)).upright(), (1206, 2622));
        assert_eq!(up(Some(270)).upright(), (2622, 1206));
        // A rotation that is not a quarter turn is not believed.
        assert_eq!(up(Some(45)).rotation, Rotation::R0);
        // Not a config, or no area: no geometry.
        assert_eq!(PictureGeometry::from_event(&Event::Stopped), None);
        assert_eq!(PictureGeometry::from_event(&config(0, 10, None)), None);
        assert_eq!(PictureGeometry::from_event(&config(10, 0, None)), None);
    }

    #[test]
    fn a_quarter_turn_takes_the_framebuffers_top_left_to_the_upright_top_right() {
        // The upright corners and the framebuffer corners they show, for each rotation.
        let corners = [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)];
        let shows = |r: Rotation| corners.map(|(u, v)| r.sample(u, v));
        assert_eq!(shows(Rotation::R0), [(0.0, 0.0), (1.0, 0.0), (1.0, 1.0), (0.0, 1.0)]);
        // 90: the upright top left shows the framebuffer's bottom left; the upright top right its top left.
        assert_eq!(shows(Rotation::R90), [(0.0, 1.0), (0.0, 0.0), (1.0, 0.0), (1.0, 1.0)]);
        assert_eq!(shows(Rotation::R180), [(1.0, 1.0), (0.0, 1.0), (0.0, 0.0), (1.0, 0.0)]);
        assert_eq!(shows(Rotation::R270), [(1.0, 0.0), (1.0, 1.0), (0.0, 1.0), (0.0, 0.0)]);
        // Four quarter turns come back to where they started.
        let (u, v) = (0.3, 0.8);
        let mut p = (u, v);
        for _ in 0..4 {
            p = Rotation::R90.sample(p.0, p.1);
        }
        near(p, (u, v));
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "the rectangle is whole pixels: exact")]
    fn the_letterbox_is_the_largest_centred_rectangle_of_the_pictures_shape() {
        // A tall picture in a wide window: bars left and right.
        let r = letterbox((1000, 500), (1206, 2622)).unwrap();
        assert_eq!((r.y, r.h), (0.0, 500.0));
        assert!((r.w - 230.0).abs() <= 1.0, "{r:?}");
        assert_eq!(r.x, ((1000.0 - r.w) / 2.0).floor());
        // The same picture in a window of its own shape: no bars at all.
        let r = letterbox((603, 1311), (1206, 2622)).unwrap();
        assert_eq!((r.x, r.y, r.w, r.h), (0.0, 0.0, 603.0, 1311.0));
        // A wide picture in a tall window: bars above and below.
        let r = letterbox((400, 1000), (2622, 1206)).unwrap();
        assert_eq!((r.x, r.w), (0.0, 400.0));
        assert!((r.h - 184.0).abs() <= 1.0, "{r:?}");
        assert!(r.y > 300.0);
        // No area, no picture.
        assert_eq!(letterbox((0, 500), (10, 10)), None);
        assert_eq!(letterbox((500, 0), (10, 10)), None);
        assert_eq!(letterbox((500, 500), (0, 10)), None);
        assert_eq!(letterbox((500, 500), (10, 0)), None);
        // A window of one pixel still has a picture of one pixel.
        let r = letterbox((1, 1), (1206, 2622)).unwrap();
        assert_eq!((r.w, r.h), (1.0, 1.0));
    }

    #[test]
    fn the_picture_never_leaves_the_window() {
        for window in [(1, 1), (7, 900), (900, 7), (333, 333), (1920, 1080), (1206, 2622), (3000, 17)] {
            for upright in [(1206, 2622), (2622, 1206), (1, 1), (4, 3000)] {
                let r = letterbox(window, upright).unwrap();
                assert!(r.x >= 0.0 && r.y >= 0.0, "{window:?} {upright:?} {r:?}");
                assert!(
                    r.x + r.w <= f64::from(window.0) && r.y + r.h <= f64::from(window.1),
                    "{window:?} {upright:?} {r:?}"
                );
            }
        }
    }

    #[test]
    fn a_point_of_the_window_is_a_point_of_the_device() {
        // The window is wider than the picture's shape allows: bars of equal width left and right (699 pixels each).
        let window = (2001, 1311);
        let upright = (1206, 2622);
        let r = letterbox(window, upright).unwrap();
        assert_eq!((r.y, r.h), (0.0, 1311.0));
        // The centre of the window is the centre of the picture.
        near(window_to_device(window, upright, (1000.5, 655.5)).unwrap(), (603.0, 1311.0));
        // The picture's corners are the first and the last pixel.
        near(window_to_device(window, upright, (r.x, 0.0)).unwrap(), (0.0, 0.0));
        let (x, y) = window_to_device(window, upright, (r.x + r.w - 0.001, 1310.999)).unwrap();
        assert!(x > 1205.9 && x <= 1206.0 && y > 2621.9 && y <= 2622.0, "{x} {y}");
        // The bars point at nothing.
        assert_eq!(window_to_device(window, upright, (r.x - 1.0, 600.0)), None);
        assert_eq!(window_to_device(window, upright, (r.x + r.w, 600.0)), None);
        assert_eq!(window_to_device(window, upright, (-5.0, 600.0)), None);
        assert_eq!(window_to_device(window, upright, (1000.5, 1311.0)), None);
        assert_eq!(window_to_device(window, upright, (1000.5, -1.0)), None);
    }

    #[test]
    fn a_drag_that_leaves_the_picture_sticks_to_its_edge() {
        let window = (2001, 1311);
        let upright = (1206, 2622);
        near(window_to_device_clamped(window, upright, (-100.0, 655.5)).unwrap(), (0.0, 1311.0));
        near(window_to_device_clamped(window, upright, (5000.0, 655.5)).unwrap(), (1206.0, 1311.0));
        near(window_to_device_clamped(window, upright, (1000.5, -50.0)).unwrap(), (603.0, 0.0));
        near(window_to_device_clamped(window, upright, (1000.5, 9999.0)).unwrap(), (603.0, 2622.0));
        near(window_to_device_clamped(window, upright, (-1.0, -1.0)).unwrap(), (0.0, 0.0));
        assert_eq!(window_to_device_clamped((0, 0), upright, (1.0, 1.0)), None);
    }

    /// The whole path, for each rotation: the device is 1206x2622 and the window is the upright picture's shape at
    /// half size, so that a window pixel is two device pixels. A point on the window is the same point of the upright
    /// picture whatever the rotation; only the upright size differs.
    #[test]
    fn each_rotation_maps_the_window_onto_its_upright_picture() {
        for (degrees, upright, window) in [
            (0, (1206, 2622), (603, 1311)),
            (90, (2622, 1206), (1311, 603)),
            (180, (1206, 2622), (603, 1311)),
            (270, (2622, 1206), (1311, 603)),
        ] {
            let g = PictureGeometry::from_event(&config(1206, 2622, Some(degrees))).unwrap();
            assert_eq!(g.upright(), upright, "rotation {degrees}");
            let (ww, wh) = (f64::from(window.0), f64::from(window.1));
            // The window's four corners and its centre (the right and bottom edges are the last pixel's far side).
            near(window_to_device(window, g.upright(), (0.0, 0.0)).unwrap(), (0.0, 0.0));
            near(
                window_to_device(window, g.upright(), (ww / 2.0, wh / 2.0)).unwrap(),
                (f64::from(upright.0) / 2.0, f64::from(upright.1) / 2.0),
            );
            near(
                window_to_device(window, g.upright(), (ww - 1.0, wh - 1.0)).unwrap(),
                (f64::from(upright.0) - 2.0, f64::from(upright.1) - 2.0),
            );
            // A pixel is two device pixels.
            near(window_to_device(window, g.upright(), (100.0, 40.0)).unwrap(), (200.0, 80.0));
        }
    }

    /// With a window of another shape, the picture is letterboxed and the same mapping holds inside the picture.
    #[test]
    fn letterboxing_and_rotation_together() {
        // A landscape picture (a quarter turn) in a portrait window: bars above and below.
        let g = PictureGeometry::from_event(&config(1206, 2622, Some(90))).unwrap();
        let window = (500, 1000);
        let r = letterbox(window, g.upright()).unwrap();
        assert_eq!((r.x, r.w), (0.0, 500.0));
        // The middle of the window is the middle of the picture, and the bars are not the picture.
        near(window_to_device(window, g.upright(), (250.0, 500.0)).unwrap(), (1311.0, 603.0));
        assert_eq!(window_to_device(window, g.upright(), (250.0, r.y - 1.0)), None);
        assert_eq!(window_to_device(window, g.upright(), (250.0, r.y + r.h)), None);
        // The picture's top edge is device y 0, its bottom edge device y 1206 (the upright height).
        near(window_to_device(window, g.upright(), (0.0, r.y)).unwrap(), (0.0, 0.0));
        let (_, y) = window_to_device(window, g.upright(), (0.0, r.y + r.h - 0.001)).unwrap();
        assert!(y > 1205.0 && y <= 1206.0, "{y}");
    }

    #[test]
    fn a_turn_of_the_device_turns_the_window_and_keeps_its_size_within_the_screen() {
        let screen = (1300, 800);
        // The first window has no current size: it is the first fit.
        assert_eq!(refit_size((1206, 2622), None, screen, None, None), fit_size((1206, 2622), screen, None, None));
        // A tall window of 368x800 (the longer side 800), the device turns: wide, 800 across, within the screen.
        let (w, h) = refit_size((2622, 1206), Some((368, 800)), screen, None, None);
        assert_eq!(w, 800);
        assert!((f64::from(h) - 800.0 * 1206.0 / 2622.0).abs() <= 1.0, "{h}");
        // A wide window turned back is tall again, as long as before.
        let (w, h) = refit_size((1206, 2622), Some((800, 368)), screen, None, None);
        assert_eq!(h, 800);
        assert!((f64::from(w) - 368.0).abs() <= 1.0, "{w}");
        // The window is never made larger than the screen allows, nor than the picture.
        assert_eq!(refit_size((2622, 1206), Some((5000, 3000)), screen, None, None).0, 1300);
        assert_eq!(refit_size((300, 600), Some((1000, 2000)), screen, None, None), (300, 600));
    }

    #[test]
    fn the_first_window_fits_the_screen_and_the_picture() {
        // A tall device on a 1440x900 screen with room for 800 pixels of height: the height limits.
        let (w, h) = fit_size((1206, 2622), (1400, 800), None, None);
        assert_eq!(h, 800);
        assert!((f64::from(w) - 800.0 * 1206.0 / 2622.0).abs() <= 1.0, "{w}");
        // A wide one: the width limits.
        let (w, h) = fit_size((2622, 1206), (1000, 800), None, None);
        assert_eq!(w, 1000);
        assert!((f64::from(h) - 1000.0 * 1206.0 / 2622.0).abs() <= 1.0, "{h}");
        // A screen larger than the picture does not magnify it.
        assert_eq!(fit_size((300, 600), (4000, 4000), None, None), (300, 600));
        // The user's own height wins over the screen's, and it keeps the shape.
        let (w, h) = fit_size((1206, 2622), (1400, 800), None, Some(1000));
        assert_eq!(h, 1000);
        assert!((f64::from(w) - 460.0).abs() <= 1.0, "{w}");
        // Both limits: the one that bites first.
        let (w, h) = fit_size((1206, 2622), (1400, 800), Some(200), Some(1000));
        assert_eq!(w, 200);
        assert!((f64::from(h) - 435.0).abs() <= 1.0, "{h}");
        // Nothing is ever zero.
        assert_eq!(fit_size((1206, 2622), (0, 0), None, None).0.min(fit_size((1206, 2622), (0, 0), None, None).1), 1);
        assert_eq!(fit_size((0, 0), (100, 100), None, None), (1, 1));
    }
}
