//! What the window says about itself: the title, and the numbers `--show-stats` puts in it.
//!
//! The numbers are the viewer's own: how many pictures were put on the screen a second, and how long each took from the
//! moment the stream handed over its access unit to the moment the picture was submitted for display (decoding, waiting
//! for the window's turn and drawing). That is the time the viewer adds; it is not the time from the device's screen
//! to yours, which also holds the device's encoder and the USB link, and which this program cannot see.

use std::fmt::{self, Write as _};
use std::time::{Duration, Instant};

/// How often the numbers are worked out (and the title changes).
pub const REPORT_EVERY: Duration = Duration::from_secs(1);

/// Counts what happened since the last [`report`](Meter::report).
#[derive(Debug)]
pub struct Meter {
    since: Instant,
    shown: u32,
    decode: Duration,
    latency: Duration,
    latency_max: Duration,
    /// Pictures decoded and replaced by a newer one before the window had drawn them.
    stale: u64,
    stale_total: u64,
}

/// What a report says.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Report {
    /// Pictures put on the screen a second.
    pub fps: f64,
    /// The average time to decode a picture, in milliseconds.
    pub decode_ms: f64,
    /// The average and the longest time from the access unit's arrival to the picture's submission, in milliseconds.
    pub latency_ms: f64,
    pub latency_max_ms: f64,
    /// Decoded pictures that were replaced before they were shown, in this period.
    pub stale: u64,
}

impl Meter {
    #[must_use]
    pub fn new(now: Instant) -> Meter {
        Meter {
            since: now,
            shown: 0,
            decode: Duration::ZERO,
            latency: Duration::ZERO,
            latency_max: Duration::ZERO,
            stale: 0,
            stale_total: 0,
        }
    }

    /// A picture was submitted at `now`; its access unit arrived at `arrived` and decoding took `decode`.
    pub fn shown(&mut self, now: Instant, arrived: Instant, decode: Duration) {
        let latency = now.saturating_duration_since(arrived);
        self.shown += 1;
        self.decode += decode;
        self.latency += latency;
        self.latency_max = self.latency_max.max(latency);
    }

    /// `n` decoded pictures were dropped unseen because a newer one came first.
    pub fn stale(&mut self, n: u64) {
        self.stale += n;
        self.stale_total += n;
    }

    /// The numbers of the period since the last report, if it is at least [`REPORT_EVERY`] long (and starts the next).
    pub fn report(&mut self, now: Instant) -> Option<Report> {
        let elapsed = now.saturating_duration_since(self.since);
        if elapsed < REPORT_EVERY {
            return None;
        }
        let n = f64::from(self.shown.max(1));
        let report = Report {
            fps: f64::from(self.shown) / elapsed.as_secs_f64(),
            decode_ms: self.decode.as_secs_f64() * 1000.0 / n,
            latency_ms: self.latency.as_secs_f64() * 1000.0 / n,
            latency_max_ms: self.latency_max.as_secs_f64() * 1000.0,
            stale: self.stale,
        };
        *self = Meter { stale_total: self.stale_total, ..Meter::new(now) };
        Some(report)
    }

    /// Pictures dropped unseen since the start.
    #[must_use]
    pub fn stale_total(&self) -> u64 {
        self.stale_total
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{:.0} fps, {:.0} ms (max {:.0}), decode {:.1} ms",
            self.fps, self.latency_ms, self.latency_max_ms, self.decode_ms
        )
    }
}

/// The numbers for the title: the report, and what the stream itself dropped (access units it could not deliver in time).
#[must_use]
pub fn stats_line(report: &Report, stream_dropped: u64) -> String {
    let mut line = report.to_string();
    // Writing to a `String` does not fail.
    if report.stale > 0 {
        let _ = write!(line, ", skipped {}", report.stale);
    }
    if stream_dropped > 0 {
        let _ = write!(line, ", {stream_dropped} dropped");
    }
    line
}

/// What the window title says besides its name.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum Status {
    /// Until the first picture.
    #[default]
    Connecting,
    /// Showing the device. `note` is a standing remark (the keys and touch are not available).
    Live { stats: Option<String>, note: Option<String> },
    /// The stream failed or ended with this to say.
    Ended(String),
    /// The window was closed and the stream is being stopped on the device.
    Stopping,
}

/// The title of the window: its name, then what `status` says.
#[must_use]
pub fn title(base: &str, status: &Status) -> String {
    match status {
        Status::Connecting => format!("{base} - connecting..."),
        Status::Live { stats, note } => {
            let mut title = base.to_owned();
            for part in [note, stats].into_iter().flatten() {
                title.push_str(" - ");
                title.push_str(part);
            }
            title
        }
        Status::Ended(why) => format!("{base} - {why}"),
        Status::Stopping => format!("{base} - stopping..."),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn nothing_is_reported_before_a_second_has_passed() {
        let t0 = Instant::now();
        let mut m = Meter::new(t0);
        m.shown(t0 + ms(10), t0 + ms(2), ms(1));
        assert_eq!(m.report(t0 + ms(999)), None);
        assert!(m.report(t0 + ms(1000)).is_some());
    }

    #[test]
    fn the_report_is_the_average_over_the_period_and_then_starts_again() {
        let t0 = Instant::now();
        let mut m = Meter::new(t0);
        // 50 pictures in the second: each decoded in 2 ms and shown 10 ms after it arrived, one of them 40 ms after.
        for i in 0..50u64 {
            let arrived = t0 + ms(i * 20);
            let late = if i == 7 { 40 } else { 10 };
            m.shown(arrived + ms(late), arrived, ms(2));
        }
        m.stale(3);
        let r = m.report(t0 + ms(1000)).expect("a report");
        assert!((r.fps - 50.0).abs() < 1e-9);
        assert!((r.decode_ms - 2.0).abs() < 1e-9);
        assert!((r.latency_ms - (49.0 * 10.0 + 40.0) / 50.0).abs() < 1e-9, "{}", r.latency_ms);
        assert!((r.latency_max_ms - 40.0).abs() < 1e-9);
        assert_eq!(r.stale, 3);
        // The next period starts empty; the total of the dropped does not.
        assert_eq!(m.stale_total(), 3);
        let r = m.report(t0 + ms(2000)).expect("a report");
        assert_eq!((r.fps, r.stale), (0.0, 0));
        assert_eq!(m.stale_total(), 3);
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "no time at all: exactly zero")]
    fn a_picture_that_seems_to_arrive_after_it_was_shown_has_no_negative_latency() {
        let t0 = Instant::now();
        let mut m = Meter::new(t0);
        m.shown(t0, t0 + ms(5), ms(0));
        assert_eq!(m.report(t0 + ms(1000)).expect("a report").latency_ms, 0.0);
    }

    #[test]
    fn the_report_reads_as_a_line_for_the_title() {
        let r = Report { fps: 59.6, decode_ms: 1.234, latency_ms: 12.4, latency_max_ms: 31.2, stale: 0 };
        assert_eq!(r.to_string(), "60 fps, 12 ms (max 31), decode 1.2 ms");
    }

    #[test]
    fn the_line_adds_what_was_skipped_and_dropped_only_when_there_is_some() {
        let mut r = Report { fps: 60.0, decode_ms: 1.0, latency_ms: 10.0, latency_max_ms: 20.0, stale: 0 };
        assert_eq!(stats_line(&r, 0), "60 fps, 10 ms (max 20), decode 1.0 ms");
        assert_eq!(stats_line(&r, 4), "60 fps, 10 ms (max 20), decode 1.0 ms, 4 dropped");
        r.stale = 2;
        assert_eq!(stats_line(&r, 4), "60 fps, 10 ms (max 20), decode 1.0 ms, skipped 2, 4 dropped");
    }

    #[test]
    fn the_title_says_what_is_going_on() {
        assert_eq!(title("scrios", &Status::Connecting), "scrios - connecting...");
        assert_eq!(title("scrios", &Status::Live { stats: None, note: None }), "scrios");
        assert_eq!(title("scrios", &Status::Live { stats: Some("60 fps".into()), note: None }), "scrios - 60 fps");
        assert_eq!(
            title("My phone", &Status::Live { stats: Some("60 fps".into()), note: Some("view only".into()) }),
            "My phone - view only - 60 fps"
        );
        assert_eq!(title("scrios", &Status::Ended("the device went away".into())), "scrios - the device went away");
        assert_eq!(title("scrios", &Status::Stopping), "scrios - stopping...");
    }
}
