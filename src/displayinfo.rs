//! The primary display's size and orientation, from the `CoreDevice` `deviceinfo` service.
//!
//! The service answers the read-only feature `com.apple.coredevice.feature.getdisplayinfo` with a
//! `displays` array; each display has `displayId`, `name`, `primary`, `external`, `nativeSize`
//! (`[width, height]`, already in the current orientation), `pointScale`, `currentOrientation` and
//! `nativeOrientation` (`rot0`, `rot90`, `rot180`, `rot270`). This module only reads that reply.

use std::future::Future;
use std::pin::Pin;

/// One query for the device's displays, in flight.
pub type DisplayQuery = Pin<Box<dyn Future<Output = Result<Vec<DisplayInfo>, String>>>>;

/// Where display info comes from (the device; a fake in tests).
pub trait DisplayInfoSource {
    /// Starts one query. Queries run one at a time on the device's side.
    fn query(&self) -> DisplayQuery;
}

/// One display of the device.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayInfo {
    pub display_id: Option<i64>,
    pub name: String,
    pub primary: bool,
    pub external: bool,
    /// Pixels, in the current orientation.
    pub native_size: (u32, u32),
    pub point_scale: Option<f64>,
    pub current_orientation: Option<String>,
    pub native_orientation: Option<String>,
}

impl DisplayInfo {
    /// How far the framebuffer must be turned clockwise to be upright: the degrees of the current
    /// orientation (`rot90` means 90). Checked on an iPad by the integrator; the native orientation
    /// is not part of it (it is reported as it is).
    #[must_use]
    pub fn rotation(&self) -> Option<u16> {
        orientation_degrees(self.current_orientation.as_deref()?)
    }
}

/// `rot0`, `rot90`, `rot180`, `rot270` (or just the number) as degrees.
#[must_use]
pub fn orientation_degrees(s: &str) -> Option<u16> {
    let n: u16 = s.trim().strip_prefix("rot").unwrap_or(s.trim()).parse().ok()?;
    (n.is_multiple_of(90) && n < 360).then_some(n)
}

/// The display the screen is on: the primary one that is not external, else any primary one,
/// else the first.
#[must_use]
pub fn primary(displays: &[DisplayInfo]) -> Option<&DisplayInfo> {
    displays
        .iter()
        .find(|d| d.primary && !d.external)
        .or_else(|| displays.iter().find(|d| d.primary))
        .or_else(|| displays.first())
}

// A plist integer is read as a double; one above 2^53 is no display size and loses a few low bits.
#[expect(clippy::cast_precision_loss, reason = "see above: display sizes and ids are small")]
fn as_f64(v: &plist::Value) -> Option<f64> {
    match v {
        plist::Value::Real(r) => Some(*r),
        plist::Value::Integer(i) => i.as_signed().map(|n| n as f64).or_else(|| i.as_unsigned().map(|n| n as f64)),
        plist::Value::String(s) => s.trim().parse().ok(),
        // CoreDevice wraps some values in a single-key dictionary ({"int": 3}).
        plist::Value::Dictionary(d) if d.len() == 1 => d.iter().next().and_then(|(_, v)| as_f64(v)),
        _ => None,
    }
}

fn as_bool(v: &plist::Value) -> Option<bool> {
    match v {
        plist::Value::Boolean(b) => Some(*b),
        plist::Value::Integer(i) => i.as_unsigned().map(|n| n != 0),
        plist::Value::Dictionary(d) if d.len() == 1 => d.iter().next().and_then(|(_, v)| as_bool(v)),
        _ => None,
    }
}

fn as_string(v: &plist::Value) -> Option<String> {
    match v {
        plist::Value::String(s) => Some(s.clone()),
        plist::Value::Dictionary(d) if d.len() == 1 => d.iter().next().and_then(|(_, v)| as_string(v)),
        _ => None,
    }
}

/// `[w, h]`, or `{"width": w, "height": h}`.
#[expect(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "both sides are checked to be in 1..=65536"
)]
fn as_size(v: &plist::Value) -> Option<(u32, u32)> {
    let (w, h) = match v {
        plist::Value::Array(a) if a.len() == 2 => (as_f64(&a[0])?, as_f64(&a[1])?),
        plist::Value::Dictionary(d) => (as_f64(d.get("width")?)?, as_f64(d.get("height")?)?),
        _ => return None,
    };
    let ok = |x: f64| x.is_finite() && (1.0..=65_536.0).contains(&x);
    (ok(w) && ok(h)).then_some((w.round() as u32, h.round() as u32))
}

/// Finds `key` in `v`, at any depth.
fn find<'a>(v: &'a plist::Value, key: &str, depth: u32) -> Option<&'a plist::Value> {
    if depth > 8 {
        return None;
    }
    match v {
        plist::Value::Dictionary(d) => {
            d.get(key).or_else(|| d.iter().find_map(|(_, inner)| find(inner, key, depth + 1)))
        }
        plist::Value::Array(a) => a.iter().find_map(|inner| find(inner, key, depth + 1)),
        _ => None,
    }
}

fn parse_display(d: &plist::Dictionary) -> Option<DisplayInfo> {
    let native_size = as_size(d.get("nativeSize")?)?;
    #[expect(clippy::cast_possible_truncation, reason = "a float-to-int `as` saturates, and an id is a small integer")]
    let display_id = d.get("displayId").and_then(as_f64).map(|n| n as i64);
    Some(DisplayInfo {
        display_id,
        name: d.get("name").and_then(as_string).unwrap_or_default(),
        primary: d.get("primary").and_then(as_bool).unwrap_or(false),
        external: d.get("external").and_then(as_bool).unwrap_or(false),
        native_size,
        point_scale: d.get("pointScale").and_then(as_f64).filter(|s| *s > 0.0),
        current_orientation: d.get("currentOrientation").and_then(as_string),
        native_orientation: d.get("nativeOrientation").and_then(as_string),
    })
}

/// Reads the `displays` array of a `getdisplayinfo` reply. Displays without a usable `nativeSize`
/// are skipped; no usable display at all is an error.
///
/// # Errors
///
/// A message if the reply has no `displays` array, or none of its displays has a usable `nativeSize`.
pub fn parse_display_info(reply: &plist::Value) -> Result<Vec<DisplayInfo>, String> {
    let displays = find(reply, "displays", 0).and_then(|v| v.as_array()).ok_or("the reply has no `displays` array")?;
    let parsed: Vec<DisplayInfo> = displays.iter().filter_map(|d| d.as_dictionary().and_then(parse_display)).collect();
    if parsed.is_empty() {
        return Err(format!("none of the {} displays in the reply has a usable nativeSize", displays.len()));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::cast_precision_loss,
        reason = "the tests number packets, bytes and times with small loop counters, so a cast does not truncate or lose precision"
    )]
    use super::*;

    fn int(n: i64) -> plist::Value {
        plist::Value::Integer(n.into())
    }

    fn s(v: &str) -> plist::Value {
        plist::Value::String(v.into())
    }

    fn dict(entries: Vec<(&str, plist::Value)>) -> plist::Value {
        let mut d = plist::Dictionary::new();
        for (k, v) in entries {
            d.insert(k.to_string(), v);
        }
        plist::Value::Dictionary(d)
    }

    #[expect(clippy::too_many_arguments, reason = "a test builder: one argument per key of the display's dictionary")]
    fn display(
        id: i64,
        name: &str,
        primary: bool,
        external: bool,
        size: [i64; 2],
        scale: i64,
        cur: &str,
        nat: &str,
    ) -> plist::Value {
        dict(vec![
            ("displayId", int(id)),
            ("name", s(name)),
            ("primary", plist::Value::Boolean(primary)),
            ("external", plist::Value::Boolean(external)),
            ("nativeSize", plist::Value::Array(vec![int(size[0]), int(size[1])])),
            ("pointScale", plist::Value::Real(scale as f64)),
            ("currentOrientation", s(cur)),
            ("nativeOrientation", s(nat)),
        ])
    }

    /// The shape of the reply measured on an iPad: the built-in display and the stream's wireless one.
    fn ipad_reply() -> plist::Value {
        dict(vec![(
            "displays",
            plist::Value::Array(vec![
                display(1, "LCD", true, false, [2388, 1668], 2, "rot90", "rot270"),
                display(3, "Wireless", false, true, [2400, 1680], 1, "rot0", "rot0"),
            ]),
        )])
    }

    #[test]
    fn the_reply_of_the_ipad_is_read() {
        let all = parse_display_info(&ipad_reply()).unwrap();
        assert_eq!(all.len(), 2);
        let p = primary(&all).unwrap();
        assert_eq!(p.display_id, Some(1));
        assert_eq!(p.name, "LCD");
        assert_eq!(p.native_size, (2388, 1668));
        assert_eq!(p.point_scale, Some(2.0));
        assert_eq!(p.current_orientation.as_deref(), Some("rot90"));
        assert_eq!(p.native_orientation.as_deref(), Some("rot270"));
        // The framebuffer is turned by the current orientation: rot90 is 90 degrees clockwise.
        assert_eq!(p.rotation(), Some(90));
        // The stream's own display is the second one.
        assert_eq!(all[1].native_size, (2400, 1680));
        assert!(all[1].external && !all[1].primary);
    }

    #[test]
    fn rotation_is_the_current_orientation_in_degrees() {
        let mut d = parse_display_info(&ipad_reply()).unwrap().remove(0);
        for (cur, want) in [("rot0", 0), ("rot90", 90), ("rot180", 180), ("rot270", 270)] {
            d.current_orientation = Some(cur.into());
            // Whatever the native orientation is.
            for nat in ["rot0", "rot90", "rot270"] {
                d.native_orientation = Some(nat.into());
                assert_eq!(d.rotation(), Some(want), "{cur} native {nat}");
            }
        }
        d.native_orientation = None;
        assert_eq!(d.rotation(), Some(270), "the native orientation is not needed");
        d.current_orientation = Some("sideways".into());
        assert_eq!(d.rotation(), None);
        d.current_orientation = None;
        assert_eq!(d.rotation(), None);
    }

    #[test]
    fn orientations_parse() {
        assert_eq!(orientation_degrees("rot0"), Some(0));
        assert_eq!(orientation_degrees("rot270"), Some(270));
        assert_eq!(orientation_degrees("90"), Some(90));
        assert_eq!(orientation_degrees("rot45"), None);
        assert_eq!(orientation_degrees("rot360"), None);
        assert_eq!(orientation_degrees(""), None);
    }

    #[test]
    fn the_primary_display_is_chosen() {
        let reply = dict(vec![(
            "displays",
            plist::Value::Array(vec![
                display(5, "Ext", true, true, [1920, 1080], 1, "rot0", "rot0"),
                display(2, "Built-in", true, false, [1206, 2622], 3, "rot0", "rot0"),
            ]),
        )]);
        let all = parse_display_info(&reply).unwrap();
        assert_eq!(primary(&all).unwrap().display_id, Some(2));
        // No primary flag at all: the first display.
        let reply = dict(vec![(
            "displays",
            plist::Value::Array(vec![display(7, "A", false, false, [100, 200], 1, "rot0", "rot0")]),
        )]);
        assert_eq!(primary(&parse_display_info(&reply).unwrap()).unwrap().display_id, Some(7));
        assert_eq!(primary(&[]), None);
    }

    #[test]
    fn the_array_may_be_nested_and_values_wrapped() {
        let wrapped = dict(vec![
            ("displayId", dict(vec![("int", int(1))])),
            ("name", s("LCD")),
            ("primary", dict(vec![("bool", plist::Value::Boolean(true))])),
            ("nativeSize", dict(vec![("width", int(1206)), ("height", plist::Value::Real(2622.0))])),
            ("pointScale", dict(vec![("double", plist::Value::Real(3.0))])),
            ("currentOrientation", s("rot0")),
        ]);
        let reply =
            dict(vec![("result", dict(vec![("info", dict(vec![("displays", plist::Value::Array(vec![wrapped]))]))]))]);
        let all = parse_display_info(&reply).unwrap();
        assert_eq!(all[0].native_size, (1206, 2622));
        assert_eq!(all[0].point_scale, Some(3.0));
        assert!(all[0].primary && !all[0].external);
        assert_eq!(all[0].rotation(), Some(0));
    }

    #[test]
    fn replies_without_usable_displays_are_errors() {
        assert!(parse_display_info(&dict(vec![])).unwrap_err().contains("no `displays`"));
        assert!(parse_display_info(&dict(vec![("displays", plist::Value::Array(vec![]))])).is_err());
        let bad = dict(vec![("name", s("x")), ("nativeSize", plist::Value::Array(vec![int(0), int(5)]))]);
        let reply = dict(vec![("displays", plist::Value::Array(vec![bad, s("junk")]))]);
        assert!(parse_display_info(&reply).unwrap_err().contains("usable nativeSize"));
        assert!(parse_display_info(&plist::Value::Boolean(true)).is_err());
    }
}
