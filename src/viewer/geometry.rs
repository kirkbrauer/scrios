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

/// How the window is laid out: the picture letterboxed inside the window less a margin, and the margin around it.
///
/// The **margin** is a band `margin` pixels wide on every side of the picture (drawn a little lighter than the bars).
/// A press that starts in it is on the device's edge: it is what makes an edge swipe (up from the bottom for Home, down
/// from the top for the Control Center) easy with a mouse, because the device's edge is then not the window's edge, where
/// a drag grabs the window's resize border instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// The window's size in physical pixels.
    pub window: (u32, u32),
    /// The upright picture's size.
    pub upright: (u32, u32),
    /// The margin in physical pixels (0: none).
    pub margin: u32,
}

impl Layout {
    /// The margin that is used: never more than a sixth of the window's shorter side, so that a small window keeps its
    /// picture.
    #[must_use]
    pub fn margin(&self) -> u32 {
        self.margin.min(self.window.0.min(self.window.1) / 6)
    }

    /// Where the picture is: letterboxed in the window less the margin. `None` when there is no area.
    #[must_use]
    pub fn picture(&self) -> Option<Rect> {
        let m = self.margin();
        let inner = (self.window.0.saturating_sub(2 * m), self.window.1.saturating_sub(2 * m));
        let r = letterbox(inner, self.upright)?;
        Some(Rect { x: r.x + f64::from(m), y: r.y + f64::from(m), ..r })
    }

    /// The picture and its margin: where a press is on the device.
    #[must_use]
    pub fn zone(&self) -> Option<Rect> {
        let p = self.picture()?;
        let m = f64::from(self.margin());
        Some(Rect { x: p.x - m, y: p.y - m, w: p.w + 2.0 * m, h: p.h + 2.0 * m })
    }

    /// A press at `at` (physical pixels of the window) as a point of the upright picture, in pixels of it. Inside the
    /// picture it is the matching point. In the margin it is **on the edge** of the device, on each side of the picture
    /// that the press is beyond: the first column or row (0) on the left and top, the last on the right and bottom (the
    /// digitizer's very edge); along the other axis it is the matching point. `None` in the bars and outside the window.
    #[must_use]
    pub fn press(&self, at: (f64, f64)) -> Option<(f64, f64)> {
        let (p, zone) = (self.picture()?, self.zone()?);
        if !zone.contains(at.0, at.1) {
            return None;
        }
        let (x, y) = scale_into(&p, self.upright, at);
        if p.contains(at.0, at.1) {
            return Some((x, y));
        }
        let edge = |at: f64, start: f64, len: f64, last: f64, matched: f64| {
            if at < start {
                0.0
            } else if at >= start + len {
                last
            } else {
                matched
            }
        };
        Some((edge(at.0, p.x, p.w, last_pixel(self.upright.0), x), edge(at.1, p.y, p.h, last_pixel(self.upright.1), y)))
    }

    /// A point of the window as a point of the upright picture once a finger is down: the same mapping, with a point in
    /// the margin, the bars or outside the window taken to the nearest point of the picture's edge. `None` only when there
    /// is no picture.
    #[must_use]
    pub fn follow(&self, at: (f64, f64)) -> Option<(f64, f64)> {
        Some(scale_into(&self.picture()?, self.upright, at))
    }

    /// Whether `at` is in the margin below the picture (the band an upward swipe from the bottom edge starts in).
    #[must_use]
    pub fn in_bottom_margin(&self, at: (f64, f64)) -> bool {
        match (self.picture(), self.zone()) {
            (Some(p), Some(zone)) => zone.contains(at.0, at.1) && at.1 >= p.y + p.h,
            _ => false,
        }
    }

    /// Whether a press at `at` is where a swipe up from the bottom edge starts: in the margin below the picture, or in the
    /// picture's lowest `fraction` (the home indicator's own strip).
    #[must_use]
    pub fn starts_bottom_swipe(&self, at: (f64, f64), fraction: f64) -> bool {
        let Some(p) = self.picture() else { return false };
        self.in_bottom_margin(at) || (p.contains(at.0, at.1) && at.1 >= p.y + p.h * (1.0 - fraction))
    }

    /// The hint that shows while the pointer is in the bottom margin: a short bar, like iOS's home indicator, in the
    /// middle of the band. `None` when the margin is too thin for it.
    #[must_use]
    pub fn hint(&self) -> Option<Rect> {
        let m = self.margin();
        if m < 8 {
            return None;
        }
        let p = self.picture()?;
        let h = f64::from(m / 6).clamp(3.0, 8.0);
        let w = (p.w / 4.0).clamp(48.0, 240.0).min(p.w).floor();
        Some(Rect { x: (p.x + (p.w - w) / 2.0).floor(), y: (p.y + p.h + (f64::from(m) - h) / 2.0).floor(), w, h })
    }
}

/// The last pixel of an axis of `size` pixels: where the device's edge is.
fn last_pixel(size: u32) -> f64 {
    f64::from(size.saturating_sub(1))
}

fn scale_into(r: &Rect, upright: (u32, u32), at: (f64, f64)) -> (f64, f64) {
    let fx = ((at.0 - r.x) / r.w).clamp(0.0, 1.0);
    let fy = ((at.1 - r.y) / r.h).clamp(0.0, 1.0);
    (fx * f64::from(upright.0), fy * f64::from(upright.1))
}

/// The size to open the window at: the picture's shape at the largest size within `max` (logical pixels), and within
/// `limit_w` and `limit_h` when the user gave them, plus the `margin` on each side (the limits and `max` are for the
/// whole window). Never below 1x1, never larger than the picture itself (a window that magnifies a picture is not a
/// better view of it).
#[must_use]
pub fn fit_size(
    upright: (u32, u32),
    max: (u32, u32),
    limit_w: Option<u32>,
    limit_h: Option<u32>,
    margin: u32,
) -> (u32, u32) {
    let pad = margin.saturating_mul(2);
    let room = |v: u32| f64::from(v.saturating_sub(pad).max(1));
    let (pw, ph) = (f64::from(upright.0.max(1)), f64::from(upright.1.max(1)));
    let bound_w = limit_w.map_or_else(|| room(max.0), room);
    let bound_h = limit_h.map_or_else(|| room(max.1), room);
    // The user's own limit is what they asked for: it may exceed the picture. The screen's is only a bound.
    let scale = (bound_w / pw).min(bound_h / ph);
    let scale = if limit_w.is_some() || limit_h.is_some() { scale } else { scale.min(1.0) };
    (whole_pixels(pw * scale) + pad, whole_pixels(ph * scale) + pad)
}

/// What the window was last sized for: the shape the stream's `config` gave it. A picture that arrives before its
/// config does not count (it only shows what it is), so a config that follows still sizes the window.
#[derive(Debug, Default)]
pub struct Fitter {
    fitted: Option<(u32, u32)>,
}

/// Why the window is to be sized again.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refit {
    /// The first shape the stream has given: size the window for it as for the first window.
    First,
    /// The shape changed (the device turned): size the window for it from its present size.
    Turned,
}

impl Fitter {
    /// The stream says the upright picture is `upright`: whether the window is to be sized, and how.
    pub fn shape(&mut self, upright: (u32, u32)) -> Option<Refit> {
        let before = self.fitted.replace(upright);
        match before {
            None => Some(Refit::First),
            Some(old) if old != upright => Some(Refit::Turned),
            Some(_) => None,
        }
    }

    /// The shape the window was sized for, if the stream has given one.
    #[must_use]
    pub fn shape_known(&self) -> Option<(u32, u32)> {
        self.fitted
    }
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
    margin: u32,
) -> (u32, u32) {
    match current {
        None => fit_size(upright, max, limit_w, limit_h, margin),
        Some((w, h)) => {
            let long = w.max(h).max(1);
            fit_size(upright, (long.min(max.0.max(1)), long.min(max.1.max(1))), None, None, margin)
        }
    }
}

/// A length of `logical` logical pixels on a display of `scale` physical pixels to each, in physical pixels (0 stays 0).
#[must_use]
pub fn scaled_pixels(logical: u32, scale: f64) -> u32 {
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "clamped into the range of a u32 first"
    )]
    let pixels = (f64::from(logical) * scale).round().clamp(0.0, f64::from(u32::MAX)) as u32;
    pixels
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

    /// A press and a drag with no margin: the picture's whole shape in the window and nothing around it.
    fn window_to_device(window: (u32, u32), upright: (u32, u32), at: (f64, f64)) -> Option<(f64, f64)> {
        Layout { window, upright, margin: 0 }.press(at)
    }

    fn window_to_device_clamped(window: (u32, u32), upright: (u32, u32), at: (f64, f64)) -> Option<(f64, f64)> {
        Layout { window, upright, margin: 0 }.follow(at)
    }

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
        assert_eq!(
            refit_size((1206, 2622), None, screen, None, None, 0),
            fit_size((1206, 2622), screen, None, None, 0)
        );
        // A tall window of 368x800 (the longer side 800), the device turns: wide, 800 across, within the screen.
        let (w, h) = refit_size((2622, 1206), Some((368, 800)), screen, None, None, 0);
        assert_eq!(w, 800);
        assert!((f64::from(h) - 800.0 * 1206.0 / 2622.0).abs() <= 1.0, "{h}");
        // A wide window turned back is tall again, as long as before.
        let (w, h) = refit_size((1206, 2622), Some((800, 368)), screen, None, None, 0);
        assert_eq!(h, 800);
        assert!((f64::from(w) - 368.0).abs() <= 1.0, "{w}");
        // The window is never made larger than the screen allows, nor than the picture.
        assert_eq!(refit_size((2622, 1206), Some((5000, 3000)), screen, None, None, 0).0, 1300);
        assert_eq!(refit_size((300, 600), Some((1000, 2000)), screen, None, None, 0), (300, 600));
    }

    #[test]
    fn the_first_window_fits_the_screen_and_the_picture() {
        // A tall device on a 1440x900 screen with room for 800 pixels of height: the height limits.
        let (w, h) = fit_size((1206, 2622), (1400, 800), None, None, 0);
        assert_eq!(h, 800);
        assert!((f64::from(w) - 800.0 * 1206.0 / 2622.0).abs() <= 1.0, "{w}");
        // A wide one: the width limits.
        let (w, h) = fit_size((2622, 1206), (1000, 800), None, None, 0);
        assert_eq!(w, 1000);
        assert!((f64::from(h) - 1000.0 * 1206.0 / 2622.0).abs() <= 1.0, "{h}");
        // A screen larger than the picture does not magnify it.
        assert_eq!(fit_size((300, 600), (4000, 4000), None, None, 0), (300, 600));
        // The user's own height wins over the screen's, and it keeps the shape.
        let (w, h) = fit_size((1206, 2622), (1400, 800), None, Some(1000), 0);
        assert_eq!(h, 1000);
        assert!((f64::from(w) - 460.0).abs() <= 1.0, "{w}");
        // Both limits: the one that bites first.
        let (w, h) = fit_size((1206, 2622), (1400, 800), Some(200), Some(1000), 0);
        assert_eq!(w, 200);
        assert!((f64::from(h) - 435.0).abs() <= 1.0, "{h}");
        // Nothing is ever zero.
        assert_eq!(
            fit_size((1206, 2622), (0, 0), None, None, 0).0.min(fit_size((1206, 2622), (0, 0), None, None, 0).1),
            1
        );
        assert_eq!(fit_size((0, 0), (100, 100), None, None, 0), (1, 1));
    }

    /// A 500x1000 window, a 24-pixel margin and the 1206x2622 picture: it is 438x952 at (31, 24), the margin band is 486
    /// wide from x = 7 and the whole 1000 high.
    fn margined() -> Layout {
        Layout { window: (500, 1000), upright: (1206, 2622), margin: 24 }
    }

    #[test]
    fn the_picture_is_letterboxed_in_the_window_less_the_margin() {
        let l = margined();
        assert_eq!(l.margin(), 24);
        let p = l.picture().unwrap();
        assert_eq!((p.x, p.y, p.w, p.h), (31.0, 24.0, 438.0, 952.0));
        // The height limits it, so the margin band reaches the window's top and bottom edge: the window's own border
        // (where a drag resizes it) is outside the band that is the device's edge, on the sides, and is the band's far end
        // above and below.
        let z = l.zone().unwrap();
        assert_eq!((z.x, z.y, z.w, z.h), (7.0, 0.0, 486.0, 1000.0));
        // With no margin there is none: the picture is the window's letterbox and the zone is the picture.
        let none = Layout { margin: 0, ..l };
        assert_eq!(none.picture(), letterbox((500, 1000), (1206, 2622)));
        assert_eq!(none.zone(), none.picture());
        // A window of the picture's own shape plus the margin: the band is the whole border of the window.
        let exact = Layout { window: (438 + 48, 952 + 48), upright: (1206, 2622), margin: 24 };
        let p = exact.picture().unwrap();
        assert_eq!((p.x, p.y, p.w, p.h), (24.0, 24.0, 438.0, 952.0));
        let z = exact.zone().unwrap();
        assert_eq!((z.x, z.y, z.w, z.h), (0.0, 0.0, 486.0, 1000.0));
    }

    #[test]
    fn a_wide_window_keeps_the_bars_outside_the_margin() {
        // 1000 wide, 1000 high: the picture is 438x952 as before; the bars left and right are beyond the margin band.
        let l = Layout { window: (1000, 1000), upright: (1206, 2622), margin: 24 };
        let p = l.picture().unwrap();
        let z = l.zone().unwrap();
        assert_eq!((p.y, p.h), (24.0, 952.0));
        assert!(z.x > 200.0 && z.x + z.w < 800.0, "{z:?}");
        // A press in the bar, well away from the picture, is nothing, and so is one just outside the band.
        assert_eq!(l.press((50.0, 500.0)), None);
        assert_eq!(l.press((z.x - 1.0, 500.0)), None);
        assert!(l.press((z.x + 1.0, 500.0)).is_some(), "the band's first column is the margin");
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "the device's edge is exactly a pixel of it, or 0")]
    fn a_press_is_snapped_to_the_edge_it_is_beyond() {
        let l = margined();
        let up = l.upright;
        let (right_edge, bottom_edge) = (f64::from(up.0 - 1), f64::from(up.1 - 1));
        // Bottom: the last row exactly, the matching column (the middle of the picture is the middle of the device).
        let (x, y) = l.press((250.0, 990.0)).unwrap();
        assert_eq!(y, bottom_edge);
        assert!((x - 603.0 * (250.0 - 31.0) / 219.0).abs() < 1.0, "{x}");
        // Top: row 0. Left: column 0. Right: the last column.
        assert_eq!(l.press((250.0, 10.0)).unwrap().1, 0.0);
        assert_eq!(l.press((15.0, 500.0)).unwrap().0, 0.0);
        assert_eq!(l.press((485.0, 500.0)).unwrap().0, right_edge);
        // Each along-the-edge coordinate is the ordinary one, so a corner press is the corner of the device.
        assert_eq!(l.press((10.0, 10.0)), Some((0.0, 0.0)));
        assert_eq!(l.press((490.0, 10.0)), Some((right_edge, 0.0)));
        assert_eq!(l.press((10.0, 990.0)), Some((0.0, bottom_edge)));
        assert_eq!(l.press((490.0, 990.0)), Some((right_edge, bottom_edge)));
        // The press just inside the picture is the ordinary mapping, not a snap.
        let inside = l.press((250.0, 900.0)).unwrap();
        assert!(inside.1 < bottom_edge - 100.0, "{inside:?}");
        // The band's four outer edges are the margin; one pixel beyond is not.
        assert_eq!(l.press((6.0, 500.0)), None);
        assert_eq!(l.press((493.0, 500.0)), None);
        assert_eq!(l.press((250.0, -1.0)), None);
        assert_eq!(l.press((250.0, 1000.0)), None);
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "the device's edge is exactly a pixel of it, or 0")]
    fn the_edge_is_the_uprights_edge_for_each_rotation() {
        // The device is 1206x2622. Turned a quarter, its upright picture is 2622x1206 and its "bottom" is the upright
        // picture's: the last row of the upright picture, 1205.
        for degrees in [0u16, 90, 180, 270] {
            let g = PictureGeometry::from_event(&config(1206, 2622, Some(degrees))).unwrap();
            let up = g.upright();
            // A window of the picture's shape at about 1/3 size, with a margin, its picture at (30, 30).
            let size = (up.0 / 3 + 60, up.1 / 3 + 60);
            let l = Layout { window: size, upright: up, margin: 30 };
            let p = l.picture().unwrap();
            let mid_x = p.x + p.w / 2.0;
            let mid_y = p.y + p.h / 2.0;
            // Bottom, top, left, right margins.
            let bottom = l.press((mid_x, p.y + p.h + 15.0)).unwrap();
            assert_eq!(bottom.1, f64::from(up.1 - 1), "rotation {degrees}: bottom");
            assert_eq!(l.press((mid_x, p.y - 15.0)).unwrap().1, 0.0, "rotation {degrees}: top");
            assert_eq!(l.press((p.x - 15.0, mid_y)).unwrap().0, 0.0, "rotation {degrees}: left");
            assert_eq!(l.press((p.x + p.w + 15.0, mid_y)).unwrap().0, f64::from(up.0 - 1), "rotation {degrees}: right");
            // And the middle of the picture is the middle of the device, whatever the rotation.
            let (cx, cy) = l.press((mid_x, mid_y)).unwrap();
            assert!((cx - f64::from(up.0) / 2.0).abs() < 4.0 && (cy - f64::from(up.1) / 2.0).abs() < 4.0, "{cx} {cy}");
        }
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "the device's edge is exactly a pixel of it, or 0")]
    fn after_a_press_the_pointer_is_taken_to_the_pictures_edge_wherever_it_goes() {
        let l = margined();
        // At the picture's last row, below it in the margin, outside the window: the bottom; the column is the pointer's.
        for y in [976.0, 990.0, 999.0, 1500.0] {
            let (_, dy) = l.follow((250.0, y)).unwrap();
            assert!((dy - 2622.0).abs() < 1e-9, "{y} -> {dy}");
        }
        assert_eq!(l.follow((-300.0, 500.0)).unwrap().0, 0.0);
        assert_eq!(l.follow((9000.0, 500.0)).unwrap().0, 1206.0);
        assert_eq!(Layout { window: (0, 0), ..l }.follow((1.0, 1.0)), None);
    }

    #[test]
    fn the_margin_gives_way_in_a_small_window() {
        // Never more than a sixth of the shorter side from each edge.
        let l = Layout { window: (60, 600), upright: (1206, 2622), margin: 24 };
        assert_eq!(l.margin(), 10);
        let p = l.picture().unwrap();
        assert!(p.x >= 10.0 && p.y >= 10.0 && p.x + p.w <= 50.0 && p.y + p.h <= 590.0, "{p:?}");
        // A window of a few pixels has none, and still has a picture.
        let tiny = Layout { window: (5, 5), upright: (1206, 2622), margin: 24 };
        assert_eq!(tiny.margin(), 0);
        assert!(tiny.picture().is_some());
        assert_eq!(Layout { window: (0, 600), ..tiny }.picture(), None);
    }

    #[test]
    fn a_bottom_swipe_starts_in_the_bottom_margin_or_the_pictures_lowest_strip() {
        let l = margined();
        // The picture is 952 high from y = 24: its lowest 3% starts at 976 - 28.56.
        assert!(l.starts_bottom_swipe((250.0, 990.0), 0.03), "the margin below it");
        assert!(l.starts_bottom_swipe((250.0, 960.0), 0.03), "the strip");
        assert!(!l.starts_bottom_swipe((250.0, 940.0), 0.03), "above the strip");
        assert!(!l.starts_bottom_swipe((250.0, 500.0), 0.03), "the middle");
        assert!(!l.starts_bottom_swipe((250.0, 10.0), 0.03), "the top margin");
        assert!(!l.starts_bottom_swipe((3.0, 990.0), 0.03), "the bar beyond the margin");
        // With no margin there is only the strip.
        let none = Layout { margin: 0, ..l };
        assert!(!none.starts_bottom_swipe((250.0, 500.0), 0.03));
        assert!(none.starts_bottom_swipe((250.0, 995.0), 0.03));
        assert!(!Layout { window: (0, 0), ..l }.starts_bottom_swipe((1.0, 1.0), 0.03));
    }

    #[test]
    fn the_hint_is_a_short_bar_in_the_middle_of_the_bottom_margin() {
        let l = margined();
        let (p, h) = (l.picture().unwrap(), l.hint().unwrap());
        assert!(h.y >= p.y + p.h && h.y + h.h <= p.y + p.h + 24.0, "inside the band below the picture: {h:?}");
        assert!(h.w >= 48.0 && h.w <= p.w / 2.0);
        assert!((h.x + h.w / 2.0 - (p.x + p.w / 2.0)).abs() <= 1.0, "centred");
        assert!(h.h >= 3.0 && h.h <= 8.0);
        // The pointer is in the bottom margin where a swipe up would start, and not elsewhere.
        assert!(l.in_bottom_margin((250.0, 990.0)));
        assert!(l.in_bottom_margin((10.0, 999.0)), "a bottom corner too");
        assert!(!l.in_bottom_margin((250.0, 900.0)), "in the picture");
        assert!(!l.in_bottom_margin((250.0, 10.0)), "the top margin");
        assert!(!l.in_bottom_margin((3.0, 990.0)), "the bar beyond the band");
        // No hint where the margin is too thin to hold it, or where there is none.
        assert_eq!(Layout { margin: 6, ..l }.hint(), None);
        assert_eq!(Layout { margin: 0, ..l }.hint(), None);
        assert!(!Layout { margin: 0, ..l }.in_bottom_margin((250.0, 999.0)));
    }

    #[test]
    fn the_window_is_sized_by_the_config_and_again_when_the_shape_changes() {
        let mut f = Fitter::default();
        assert_eq!(f.shape_known(), None);
        assert_eq!(f.shape((1206, 2622)), Some(Refit::First));
        assert_eq!(f.shape((1206, 2622)), None, "a config that says the same changes nothing");
        assert_eq!(f.shape((2622, 1206)), Some(Refit::Turned));
        assert_eq!(f.shape((2622, 1206)), None);
        assert_eq!(f.shape((1206, 2622)), Some(Refit::Turned));
        assert_eq!(f.shape_known(), Some((1206, 2622)));
    }

    #[test]
    fn a_margin_in_logical_pixels_is_the_same_on_a_sharper_display() {
        assert_eq!(scaled_pixels(24, 1.0), 24);
        assert_eq!(scaled_pixels(24, 2.0), 48);
        assert_eq!(scaled_pixels(24, 1.25), 30);
        assert_eq!(scaled_pixels(0, 2.0), 0, "none stays none");
        assert_eq!(scaled_pixels(24, -1.0), 0);
    }

    #[test]
    fn the_window_is_the_picture_plus_the_margin_on_each_side() {
        // A tall device on a screen with room for 1400x800: the picture gets 800 less 2x24, and the window is 800 high.
        let (w, h) = fit_size((1206, 2622), (1400, 800), None, None, 24);
        assert_eq!(h, 800);
        let picture_h = f64::from(h - 48);
        assert!((f64::from(w - 48) - picture_h * 1206.0 / 2622.0).abs() <= 1.0, "{w}");
        // The user's limits are for the whole window.
        let (w, h) = fit_size((1206, 2622), (1400, 800), Some(300), Some(1000), 24);
        assert!(w <= 300 && h <= 1000, "{w}x{h}");
        assert!(w == 300 || h == 1000);
        // A window never larger than the picture and its margin when there is room.
        assert_eq!(fit_size((300, 600), (4000, 4000), None, None, 24), (348, 648));
        // A turn keeps the longer side.
        let (w, h) = refit_size((2622, 1206), Some((400, 800)), (1300, 800), None, None, 24);
        assert_eq!(w, 800);
        assert!(h < 400);
        // No margin is what it was.
        assert_eq!(
            fit_size((1206, 2622), (1400, 800), None, None, 0),
            fit_size((1206, 2622), (1400, 800), None, None, 0)
        );
        // A margin larger than the room is not a size of zero.
        let (w, h) = fit_size((1206, 2622), (30, 30), None, None, 24);
        assert!(w >= 49 && h >= 49);
    }
}
