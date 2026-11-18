//! The sans-IO core: datagrams and commands in, frames and RTCP datagrams out.
//!
//! The engine owns the depacketizer, the RTP statistics, the keyframe limiter and the counters. It
//! does no I/O and reads no clock: every call carries `now`, frames go to the [`OutQueue`], and the
//! RTCP datagrams to send are collected for the caller ([`Engine::take_rtcp`]). That keeps all of
//! the protocol logic testable without a socket or a phone.

use std::sync::Arc;
use std::time::{Duration, Instant};

use idevice::core_device::{
    ReportBlock, RtpPacket, build_frame_ack, build_keyframe_request, build_liveness, build_rctl, is_rtcp,
};
use tracing::{debug, info, warn};

use crate::codec::{CodecKind, SpsDescription};
use crate::depack::{AccessUnit, Depacketizer, DropReason, Output, PacketMeta};
use crate::displayinfo::{DisplayInfo, orientation_degrees, primary};
use crate::frame::{Event, Frame, Stats};
use crate::keyframe::{Decision, KeyframeLimiter};
use crate::outqueue::{OutQueue, Pushed};
use crate::rtpstats::{RtpStats, SenderReportInfo, parse_sender_report};

/// RTP payload type of the display stream's HEVC.
pub const PAYLOAD_TYPE: u8 = 100;
/// RTP clock rate of the display stream (measured; the negotiation may say otherwise).
pub const DEFAULT_CLOCK_HZ: u32 = 24_000;
pub const DEFAULT_FPS: u32 = 60;
/// No datagram at all (RTP, or the device's sender reports) for this long: the stream is dead.
pub const NO_DATA_TIMEOUT: Duration = Duration::from_secs(30);
/// No RTP (sender reports do not count) for this long: ask for a keyframe.
pub const RTP_STALL_KEYFRAME: Duration = Duration::from_secs(5);
/// No RTP for this long: give up.
pub const RTP_STALL_TIMEOUT: Duration = Duration::from_secs(15);
/// The rates the display stream's RTP clock can have: what a measurement is snapped to.
const CLOCK_CANDIDATES: [u32; 2] = [24_000, 90_000];
/// Device Hub's RCTL feedback goes out about this often.
const RCTL_INTERVAL: Duration = Duration::from_millis(50);
/// How many RTP packets in a row must come from an SSRC other than the stream's before the stream is taken to
/// have a new source and starts over: one stray packet (another sender on the port, a damaged header) must
/// not throw away the parameter sets and the picture.
const SSRC_SWITCH_RUN: usize = 3;

/// Where the RTP clock rate comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockSource {
    /// Assumed (24 kHz, what was measured on a phone).
    Default,
    /// The device's answer said so.
    Negotiated,
    /// Measured from the device's sender reports (they disagreed with the other sources).
    Measured,
}

impl ClockSource {
    pub fn as_str(self) -> &'static str {
        match self {
            ClockSource::Default => "default",
            ClockSource::Negotiated => "negotiated",
            ClockSource::Measured => "measured",
        }
    }
}

#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// The SSRC the offer declared; the sender SSRC of every RTCP packet.
    pub our_ssrc: u32,
    /// RTCP SDES CNAME.
    pub cname: String,
    /// The codec of the stream (HEVC unless the offer named only H.264).
    pub codec: CodecKind,
    pub payload_type: u8,
    pub clock_hz: u32,
    pub clock_source: ClockSource,
    pub fps: u32,
    /// Receiver reports every this often (None: none).
    pub rtcp_interval: Option<Duration>,
    pub keyframe_min_interval: Duration,
    pub stats_interval: Option<Duration>,
    /// `--visible-size`: the size of the picture on screen, overriding the SPS.
    pub visible_size: Option<(u32, u32)>,
    /// The device's SSRC, if the answer said (the first RTP packet confirms or replaces it).
    pub device_ssrc: Option<u32>,
    /// Where the device listens for RTCP (its RTP source port), if known before the first packet.
    pub rtcp_port: Option<u16>,
    /// No datagram of any kind for this long: failure.
    pub no_data_timeout: Duration,
    /// No RTP for this long: ask for a keyframe (None: never).
    pub rtp_stall_keyframe: Option<Duration>,
    /// No RTP for this long: failure (None: never).
    pub rtp_stall_timeout: Option<Duration>,
    /// Also send Device Hub's per-frame acknowledgements and periodic RCTL reports (experimental).
    pub device_hub_feedback: bool,
    /// A display info source exists: the engine asks for a new query when the SPS changes.
    pub display_info: bool,
}

impl EngineConfig {
    /// For another codec: its depacketizer, its SPS and its RTP payload type.
    pub fn for_codec(mut self, codec: CodecKind) -> Self {
        self.codec = codec;
        self.payload_type = codec.payload_type();
        self
    }

    pub fn new(our_ssrc: u32) -> Self {
        EngineConfig {
            our_ssrc,
            cname: format!("{our_ssrc:08x}@scrios"),
            codec: CodecKind::Hevc,
            payload_type: PAYLOAD_TYPE,
            clock_hz: DEFAULT_CLOCK_HZ,
            clock_source: ClockSource::Default,
            fps: DEFAULT_FPS,
            rtcp_interval: Some(Duration::from_secs(5)),
            keyframe_min_interval: Duration::from_secs(2),
            stats_interval: Some(Duration::from_secs(5)),
            visible_size: None,
            device_ssrc: None,
            rtcp_port: None,
            no_data_timeout: NO_DATA_TIMEOUT,
            rtp_stall_keyframe: Some(RTP_STALL_KEYFRAME),
            rtp_stall_timeout: Some(RTP_STALL_TIMEOUT),
            device_hub_feedback: false,
            display_info: false,
        }
    }
}

/// What the device said about its primary display.
#[derive(Debug, Clone, PartialEq)]
pub struct DisplayFacts {
    /// `nativeSize`: the picture's visible size.
    pub size: (u32, u32),
    pub point_scale: Option<f64>,
    pub rotation: Option<u16>,
    pub current_orientation: Option<String>,
    pub native_orientation: Option<String>,
}

impl DisplayFacts {
    fn from_display(d: &DisplayInfo) -> Self {
        DisplayFacts {
            size: d.native_size,
            point_scale: d.point_scale,
            rotation: d.rotation(),
            current_orientation: d.current_orientation.clone(),
            native_orientation: d.native_orientation.clone(),
        }
    }
}

/// Microseconds since the first access unit, from the RTP timestamp. The origin can be moved
/// (a new source, a corrected clock rate) without the times jumping, and the times never go back.
#[derive(Debug)]
struct PtsClock {
    hz: u64,
    /// The time of the anchor, in microseconds.
    anchor_us: u64,
    /// Timestamp ticks since the anchor, up to `last`.
    ticks: i64,
    last: u32,
    started: bool,
    last_pts: u64,
}

impl PtsClock {
    fn new(hz: u32) -> Self {
        PtsClock { hz: u64::from(hz.max(1)), anchor_us: 0, ticks: 0, last: 0, started: false, last_pts: 0 }
    }

    fn pts_us(&mut self, timestamp: u32) -> u64 {
        if self.started {
            // Signed step, so a 32-bit wrap of the timestamp is no jump.
            self.ticks += i64::from(timestamp.wrapping_sub(self.last).cast_signed());
        } else {
            self.started = true;
            self.ticks = 0;
        }
        self.last = timestamp;
        let elapsed_us = u128::try_from(self.ticks).unwrap_or(0) * 1_000_000 / u128::from(self.hz);
        let pts = self.anchor_us + u64::try_from(elapsed_us).unwrap_or(u64::MAX);
        // A unit whose timestamp is behind the one before (a reordered or restarted sender) gets the previous
        // time, never an earlier one: a decoder or a player must not see presentation times go backwards.
        // The ticks go on from where the timestamps are, so the times follow them again once they pass.
        let pts = pts.max(self.last_pts);
        self.last_pts = pts;
        pts
    }

    /// The timestamps start over (a new source): the next unit continues `gap_us` after the last
    /// one sent.
    fn rebase(&mut self, gap_us: u64) {
        self.anchor_us = self.last_pts + gap_us;
        self.started = false;
        self.ticks = 0;
    }

    /// The clock rate changes: times so far stay, later ones count in the new rate from here.
    fn set_hz(&mut self, hz: u32) {
        self.anchor_us = self.last_pts;
        self.ticks = 0;
        self.hz = u64::from(hz.max(1));
    }
}

/// Lets a repeating warning through once per `every`, with a count of what was held back.
#[derive(Debug)]
struct Throttle {
    every: Duration,
    last: Option<Instant>,
    held: u64,
}

impl Throttle {
    fn new(every: Duration) -> Self {
        Throttle { every, last: None, held: 0 }
    }

    /// `Some(n)`: log now (n earlier occurrences were suppressed); `None`: stay quiet.
    fn allow(&mut self, now: Instant) -> Option<u64> {
        match self.last {
            Some(t) if now.saturating_duration_since(t) < self.every => {
                self.held += 1;
                None
            }
            _ => {
                self.last = Some(now);
                Some(std::mem::take(&mut self.held))
            }
        }
    }
}

/// What the clock cross-check concludes from a measured rate: a new rate if the measurement
/// disagrees with the current one by more than 10%, snapped to the nearer of 24 and 90 kHz.
fn clock_correction(current: u32, measured: f64) -> Option<u32> {
    if !measured.is_finite() || measured < 1000.0 {
        return None;
    }
    let cur = f64::from(current);
    if (measured - cur).abs() / cur <= 0.10 {
        return None;
    }
    let nearest = CLOCK_CANDIDATES
        .into_iter()
        .min_by(|a, b| {
            let (da, db) =
                ((measured - f64::from(*a)).abs() / f64::from(*a), (measured - f64::from(*b)).abs() / f64::from(*b));
            da.total_cmp(&db)
        })
        .expect("two candidates");
    (nearest != current).then_some(nearest)
}

/// RTP packets from an SSRC other than the stream's, held (as datagrams, with when and from which port they
/// came) until there are [`SSRC_SWITCH_RUN`] in a row. A packet of the stream's own SSRC, or one from a third
/// SSRC, ends the run.
#[derive(Debug)]
struct NewSource {
    ssrc: u32,
    held: Vec<(Instant, u16, Vec<u8>)>,
}

/// What to do with an RTP packet, by its SSRC.
enum SourceCheck {
    /// It is the stream's (or the stream has no source yet): process it.
    Accept,
    /// It is not, and the run that would make it the new source is not complete: set aside.
    Held,
    /// The run is complete and the stream has started over; these earlier packets of the run come first.
    Switch(Vec<(Instant, u16, Vec<u8>)>),
}

#[derive(Debug)]
pub struct Engine {
    cfg: EngineConfig,
    queue: Arc<OutQueue>,
    asm: Box<dyn Depacketizer>,
    rtp: RtpStats,
    limiter: KeyframeLimiter,
    pts: PtsClock,
    counters: Stats,

    clock_hz: u32,
    clock_source: ClockSource,
    /// The first sender report seen (NTP seconds, RTP timestamp), for the clock cross-check.
    first_sr: Option<(f64, u32)>,
    clock_checked: bool,

    device_ssrc: Option<u32>,
    ssrc_confirmed: bool,
    /// Packets of another SSRC that may become the stream's.
    new_source: Option<NewSource>,
    rtcp_port: Option<u16>,
    port_from_rtp: bool,

    /// Delta units are withheld until a keyframe: set at start and after every loss or overflow.
    need_key: bool,
    /// The SPS the `config` event was built from (and what it said).
    announced: Option<(Vec<u8>, SpsDescription)>,
    /// The last `config` event written (a repeat of the same event is not written again).
    last_config: Option<Event>,
    /// The primary display, once the device has said.
    display: Option<DisplayFacts>,
    /// A new display query is wanted (the SPS changed).
    display_query_wanted: bool,

    outbox: Vec<Vec<u8>>,
    fir_seq: u8,
    next_rtcp: Option<Instant>,
    next_stats: Option<Instant>,
    next_rctl: Option<Instant>,
    frames_received: u16,
    started_at: Instant,
    last_rx: Instant,
    last_rtp: Instant,
    stall_kf_rearm: Instant,
    fatal: Option<String>,

    warn_malformed: Throttle,
    warn_payload_type: Throttle,
    warn_dropped: Throttle,
    warn_other_source: Throttle,
}

impl Engine {
    pub fn new(cfg: EngineConfig, queue: Arc<OutQueue>, now: Instant) -> Self {
        Engine {
            asm: cfg.codec.depacketizer(),
            rtp: RtpStats::new(cfg.clock_hz),
            limiter: KeyframeLimiter::new(cfg.keyframe_min_interval),
            pts: PtsClock::new(cfg.clock_hz),
            counters: Stats::default(),
            clock_hz: cfg.clock_hz,
            clock_source: cfg.clock_source,
            first_sr: None,
            clock_checked: false,
            device_ssrc: cfg.device_ssrc,
            ssrc_confirmed: false,
            new_source: None,
            rtcp_port: cfg.rtcp_port,
            port_from_rtp: false,
            need_key: true,
            announced: None,
            last_config: None,
            display: None,
            display_query_wanted: false,
            outbox: Vec::new(),
            fir_seq: 0,
            next_rtcp: cfg.rtcp_interval.map(|i| now + i),
            next_stats: cfg.stats_interval.map(|i| now + i),
            next_rctl: cfg.device_hub_feedback.then(|| now + RCTL_INTERVAL),
            frames_received: 0,
            started_at: now,
            last_rx: now,
            last_rtp: now,
            stall_kf_rearm: now,
            fatal: None,
            warn_malformed: Throttle::new(Duration::from_secs(5)),
            warn_payload_type: Throttle::new(Duration::from_secs(30)),
            warn_dropped: Throttle::new(Duration::from_secs(2)),
            warn_other_source: Throttle::new(Duration::from_secs(5)),
            queue,
            cfg,
        }
    }

    // ---- inputs -------------------------------------------------------------------------------

    /// A datagram from the device on the stream's socket, from `src_port`.
    pub fn on_datagram(&mut self, now: Instant, src_port: u16, data: &[u8]) {
        self.last_rx = now;
        if is_rtcp(data) {
            self.on_rtcp(now, data);
            return;
        }
        let Some(pkt) = RtpPacket::parse(data) else {
            if let Some(n) = self.warn_malformed.allow(now) {
                warn!("ignoring a datagram that is neither RTP nor RTCP ({} bytes; {n} more before)", data.len());
            }
            return;
        };
        if pkt.payload_type != self.cfg.payload_type {
            if let Some(n) = self.warn_payload_type.allow(now) {
                warn!(
                    "ignoring RTP of payload type {} (expected {}; {n} more before)",
                    pkt.payload_type, self.cfg.payload_type
                );
            }
            return;
        }
        match self.check_source(now, src_port, pkt.ssrc, data) {
            SourceCheck::Accept => {}
            SourceCheck::Held => return,
            // The start of the new stream: the packets of the run, in the order they came.
            SourceCheck::Switch(held) => {
                for (at, port, bytes) in held {
                    if let Some(earlier) = RtpPacket::parse(&bytes) {
                        self.on_rtp(at, port, &earlier);
                    }
                }
            }
        }
        self.on_rtp(now, src_port, &pkt);
    }

    /// An RTP packet of the stream.
    fn on_rtp(&mut self, now: Instant, src_port: u16, pkt: &RtpPacket<'_>) {
        self.last_rtp = now;
        self.counters.packets += 1;
        self.learn_source(src_port, pkt.ssrc);
        self.rtp.on_packet(now, pkt.sequence_number, pkt.timestamp);

        let mut out = Vec::new();
        self.asm.push(
            PacketMeta { seq: pkt.sequence_number, timestamp: pkt.timestamp, marker: pkt.marker },
            pkt.payload,
            &mut out,
        );
        for o in out {
            self.handle_output(now, o);
        }
    }

    /// The `keyframe` command.
    pub fn on_keyframe_command(&mut self, now: Instant) {
        debug!("keyframe requested by command");
        self.request_keyframe(now);
    }

    /// The clock reached (or passed) [`next_deadline`](Self::next_deadline).
    pub fn on_tick(&mut self, now: Instant) {
        if self.limiter.poll(now) {
            debug!("sending the coalesced keyframe request");
            self.send_keyframe_request(now);
        }
        if self.next_rtcp.is_some_and(|t| now >= t) {
            self.send_report(now);
        }
        if self.next_rctl.is_some_and(|t| now >= t) {
            self.next_rctl = Some(now + RCTL_INTERVAL);
            self.send_rctl(now);
        }
        if let Some(interval) = self.cfg.stats_interval
            && self.next_stats.is_some_and(|t| now >= t)
        {
            self.next_stats = Some(now + interval);
            let s = self.stats();
            debug!("stats: {}", self.summary());
            self.queue.push_droppable_control(Frame::control(&Event::Stats(s)));
        }

        // RTP has stopped (sender reports may still come): ask for a keyframe, then give up.
        if self.stall_keyframe_at().is_some_and(|t| now >= t) {
            let silent = now.saturating_duration_since(self.last_rtp);
            warn!("no RTP for {} s: asking for a keyframe", silent.as_secs());
            self.stall_kf_rearm = now + self.cfg.keyframe_min_interval.max(Duration::from_secs(1));
            self.request_keyframe(now);
        }
        if self.fatal.is_none() {
            let rtp_silent = now.saturating_duration_since(self.last_rtp);
            let silent = now.saturating_duration_since(self.last_rx);
            if self.cfg.rtp_stall_timeout.is_some_and(|d| rtp_silent >= d) {
                self.fatal = Some(format!("no RTP media from the device for {} s", rtp_silent.as_secs()));
            } else if silent >= self.cfg.no_data_timeout {
                self.fatal = Some(format!("no data from the device for {} s", silent.as_secs()));
            }
        }
    }

    // ---- outputs ------------------------------------------------------------------------------

    fn stall_keyframe_at(&self) -> Option<Instant> {
        self.cfg.rtp_stall_keyframe.map(|d| (self.last_rtp + d).max(self.stall_kf_rearm))
    }

    /// The earliest time something needs doing (an RTCP report, a held keyframe request, stats,
    /// the silence checks, the feedback timer).
    pub fn next_deadline(&self) -> Option<Instant> {
        [
            self.limiter.deadline(),
            self.next_rtcp,
            self.next_stats,
            self.next_rctl,
            self.stall_keyframe_at(),
            self.cfg.rtp_stall_timeout.map(|d| self.last_rtp + d),
            Some(self.last_rx + self.cfg.no_data_timeout),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// The RTCP datagrams to send to [`rtcp_port`](Self::rtcp_port), oldest first.
    pub fn take_rtcp(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.outbox)
    }

    /// The port the device listens on for RTCP, once known.
    pub fn rtcp_port(&self) -> Option<u16> {
        self.rtcp_port
    }

    /// Why the stream must be given up on, if it must.
    pub fn take_fatal(&mut self) -> Option<String> {
        self.fatal.take()
    }

    /// The counters. Access units, bytes and keyframes are what the writer has written.
    pub fn stats(&self) -> Stats {
        let w = self.queue.written();
        Stats {
            lost: self.asm.lost(),
            access_units: w.units,
            bytes: w.bytes,
            keyframes: w.keyframes,
            b_slices: self.asm.b_slices(),
            edge_downs: self.queue.edge_downs(),
            ..self.counters
        }
    }

    /// The device's SSRC, once known.
    #[cfg(test)]
    pub fn device_ssrc(&self) -> Option<u32> {
        self.device_ssrc
    }

    /// The RTP clock rate in use and where it comes from.
    #[cfg(test)]
    pub fn clock(&self) -> (u32, ClockSource) {
        (self.clock_hz, self.clock_source)
    }

    /// The picture as the last `config` event described it (for mapping touches).
    pub fn view(&self) -> Option<crate::hid::View> {
        match &self.last_config {
            Some(Event::Config { width, height, rotation, native_orientation, .. }) => Some(crate::hid::View {
                width: *width,
                height: *height,
                rotation: rotation.unwrap_or(0),
                native: native_orientation.as_deref().and_then(orientation_degrees),
            }),
            _ => None,
        }
    }

    /// One line for the log: the counters and what the depacketizer saw besides.
    pub fn summary(&self) -> String {
        format!(
            "{:?}; late/duplicate packets {}, units ended without a marker bit {}, parameter sets seen: {}",
            self.stats(),
            self.asm.late(),
            self.asm.units_without_marker(),
            self.asm.has_parameter_sets()
        )
    }

    // ---- RTP / RTCP input ---------------------------------------------------------------------

    fn on_rtcp(&mut self, now: Instant, data: &[u8]) {
        let Some(sr) = parse_sender_report(data) else {
            return;
        };
        if self.device_ssrc.is_none() {
            self.device_ssrc = Some(sr.ssrc);
        }
        if self.device_ssrc == Some(sr.ssrc) {
            self.rtp.on_sender_report(now, sr.ntp_middle);
            self.check_clock(&sr);
        }
    }

    /// Compares the clock rate in use with what two sender reports say (RTP ticks over NTP time).
    /// A rate that is more than 10% off is replaced by the nearer of 24 and 90 kHz.
    fn check_clock(&mut self, sr: &SenderReportInfo) {
        if self.clock_checked {
            return;
        }
        let Some((ntp0, ts0)) = self.first_sr else {
            self.first_sr = Some((sr.ntp_secs, sr.rtp_timestamp));
            return;
        };
        let elapsed = sr.ntp_secs - ntp0;
        if !(1.0..=120.0).contains(&elapsed) {
            if !(0.0..=120.0).contains(&elapsed) {
                self.first_sr = Some((sr.ntp_secs, sr.rtp_timestamp));
            }
            return; // too close together to measure, or too far apart to trust
        }
        self.clock_checked = true;
        let ticks = sr.rtp_timestamp.wrapping_sub(ts0);
        let measured = f64::from(ticks) / elapsed;
        match clock_correction(self.clock_hz, measured) {
            None => debug!(
                "sender reports say the RTP clock runs at {measured:.0} Hz: consistent with {} Hz ({})",
                self.clock_hz,
                self.clock_source.as_str()
            ),
            Some(hz) => {
                warn!(
                    "sender reports say the RTP clock runs at {measured:.0} Hz, not the {} Hz of the {} value: using {hz} Hz",
                    self.clock_hz,
                    self.clock_source.as_str()
                );
                self.clock_hz = hz;
                self.clock_source = ClockSource::Measured;
                self.pts.set_hz(hz);
                self.rtp.set_clock_hz(hz);
                // The config said otherwise: say it again.
                self.send_config(false);
            }
        }
    }

    /// Decides by its SSRC whether an RTP packet belongs to the stream. Once the stream's SSRC is known, a
    /// packet of another one is set aside until [`SSRC_SWITCH_RUN`] of them have come in a row (any packet of
    /// the stream's own, or of a third SSRC, starts the count over); then the source has changed and the
    /// stream starts over.
    fn check_source(&mut self, now: Instant, src_port: u16, ssrc: u32, data: &[u8]) -> SourceCheck {
        let Some(known) = self.device_ssrc.filter(|_| self.ssrc_confirmed) else {
            return SourceCheck::Accept; // the first SSRC seen is taken (learn_source)
        };
        if known == ssrc {
            self.new_source = None;
            return SourceCheck::Accept;
        }
        let mut held = match self.new_source.take() {
            Some(n) if n.ssrc == ssrc => n.held,
            _ => Vec::new(),
        };
        if held.len() + 1 < SSRC_SWITCH_RUN {
            held.push((now, src_port, data.to_vec()));
            self.new_source = Some(NewSource { ssrc, held });
            if let Some(n) = self.warn_other_source.allow(now) {
                warn!(
                    "ignoring RTP of SSRC {ssrc:#x} (the stream's is {known:#x}) until {SSRC_SWITCH_RUN} packets in a row come from it ({n} more before)"
                );
            }
            return SourceCheck::Held;
        }
        warn!("the device's SSRC changed ({known:#x} -> {ssrc:#x}, {SSRC_SWITCH_RUN} packets in a row): starting over");
        self.restart_stream();
        self.device_ssrc = Some(ssrc);
        SourceCheck::Switch(held)
    }

    /// Takes the device's SSRC and RTCP port from the first RTP packet (a change of source is dealt with by
    /// [`check_source`](Self::check_source) before).
    fn learn_source(&mut self, src_port: u16, ssrc: u32) {
        match self.device_ssrc {
            Some(known) if known == ssrc => self.ssrc_confirmed = true,
            Some(known) if self.ssrc_confirmed => {
                // Not reached: check_source has switched the stream to this SSRC already.
                debug_assert!(false, "an RTP packet of SSRC {ssrc:#x} reached a stream of {known:#x}");
            }
            Some(known) => {
                info!("the device's RTP SSRC is {ssrc:#x} (the answer said {known:#x})");
                self.device_ssrc = Some(ssrc);
                self.ssrc_confirmed = true;
            }
            None => {
                self.device_ssrc = Some(ssrc);
                self.ssrc_confirmed = true;
            }
        }
        if !self.port_from_rtp {
            self.port_from_rtp = true;
            if self.rtcp_port != Some(src_port) {
                if let Some(old) = self.rtcp_port {
                    warn!("RTP comes from port {src_port}, not {old}: sending RTCP there");
                } else {
                    debug!("RTCP goes to port {src_port}");
                }
                self.rtcp_port = Some(src_port);
            }
        }
    }

    /// A new source: forget the sequence state and the parameter sets, wait for a keyframe, and
    /// announce the config again (it names the device's SSRC). The presentation times go on from
    /// the last one sent, one frame later.
    fn restart_stream(&mut self) {
        self.asm.reset();
        self.rtp = RtpStats::new(self.clock_hz);
        self.need_key = true;
        self.announced = None;
        self.last_config = None;
        self.first_sr = None;
        self.clock_checked = false;
        self.pts.rebase(1_000_000 / u64::from(self.cfg.fps.max(1)));
    }

    // ---- access units -------------------------------------------------------------------------

    fn handle_output(&mut self, now: Instant, o: Output) {
        match o {
            Output::Dropped { timestamp, reason } => {
                self.counters.dropped_units += 1;
                self.need_key = true;
                if let Some(n) = self.warn_dropped.allow(now) {
                    warn!(
                        "dropped an access unit (RTP timestamp {timestamp}): {}; waiting for a keyframe ({n} more drops before)",
                        describe(reason)
                    );
                }
                self.request_keyframe(now);
            }
            Output::Unit(au) => {
                if self.cfg.device_hub_feedback {
                    // A frame arrived whole: acknowledge it, as Device Hub does.
                    self.frames_received = self.frames_received.wrapping_add(1);
                    self.outbox.push(build_frame_ack(self.cfg.our_ssrc, au.timestamp));
                    self.counters.rtcp += 1;
                }
                self.handle_unit(now, au);
            }
        }
    }

    fn handle_unit(&mut self, now: Instant, au: AccessUnit) {
        if self.need_key && !au.key {
            // Its references are missing: nobody can decode it.
            self.counters.dropped_units += 1;
            self.request_keyframe(now);
            return;
        }

        if let Some(sps) = &au.sps {
            let known = self.announced.as_ref().is_some_and(|(s, _)| s == sps);
            if !known && let Err(e) = self.announce(sps) {
                warn!("dropping a keyframe: {e}");
                self.counters.dropped_units += 1;
                self.need_key = true;
                self.request_keyframe(now);
                return;
            }
        }
        if self.announced.is_none() {
            // Cannot happen after a keyframe was seen; a unit without a config is no use.
            self.counters.dropped_units += 1;
            self.need_key = true;
            self.request_keyframe(now);
            return;
        }

        let key = au.key;
        let pts = self.pts.pts_us(au.timestamp);
        match self.queue.push_unit(Frame::unit(pts, key, au.params, au.payload)) {
            Pushed::Queued => {
                if key {
                    self.need_key = false;
                    // A keyframe is on its way out: a request still held back is not needed.
                    self.limiter.cancel_pending();
                }
            }
            Pushed::Overflow { dropped } => {
                self.counters.dropped_units += dropped as u64;
                self.need_key = true;
                warn!("stdout is not keeping up: dropped {dropped} access units, waiting for a keyframe");
                self.request_keyframe(now);
            }
            Pushed::Closed => {}
        }
    }

    /// Parses the SPS and sends the `config` control frame.
    fn announce(&mut self, sps_nal: &[u8]) -> Result<(), String> {
        let sps = self.cfg.codec.describe_sps(sps_nal).map_err(|e| format!("{e} (SPS bytes: {})", hex(sps_nal)))?;
        let changed = self.announced.is_some();
        self.announced = Some((sps_nal.to_vec(), sps));
        // The SPS changed: the display probably turned. Ask again (the first query was made at start).
        if changed && self.cfg.display_info {
            self.display_query_wanted = true;
        }
        self.send_config(true);
        Ok(())
    }

    /// True once if a new display query is wanted.
    pub fn take_display_query(&mut self) -> bool {
        std::mem::take(&mut self.display_query_wanted)
    }

    /// The answer to a display query. If a `config` was already written and what it says changes,
    /// it is written again.
    pub fn on_display_info(&mut self, result: Result<Vec<DisplayInfo>, String>) {
        match result {
            Ok(displays) => match primary(&displays) {
                Some(d) => {
                    let facts = DisplayFacts::from_display(d);
                    if self.display.as_ref() == Some(&facts) {
                        return; // polled again: nothing changed
                    }
                    info!(
                        "display info: {} {}x{}, scale {:?}, orientation {:?} (native {:?})",
                        d.name,
                        facts.size.0,
                        facts.size.1,
                        facts.point_scale,
                        facts.current_orientation,
                        facts.native_orientation
                    );
                    self.display = Some(facts);
                }
                None => warn!("display info lists no display"),
            },
            Err(e) => {
                warn!(
                    "display info unavailable ({e}); the picture size is {}",
                    if self.cfg.visible_size.is_some() { "--visible-size" } else { "the SPS size" }
                );
                return;
            }
        }
        self.send_config(false);
    }

    /// Writes the `config` event for the SPS announced last. A repeat of the event last written is
    /// skipped unless `force` (the SPS itself changed).
    fn send_config(&mut self, force: bool) {
        let Some((_, sps)) = &self.announced else {
            return;
        };
        let codec = sps.codec.clone();
        // `--visible-size`, else the display's own size, else the SPS (its cropping).
        let (width, height) = self.cfg.visible_size.or(self.display.as_ref().map(|d| d.size)).unwrap_or(sps.visible);
        if width > sps.coded.0 || height > sps.coded.1 {
            warn!("the visible size {width}x{height} is larger than the coded size {}x{}", sps.coded.0, sps.coded.1);
        }
        info!(
            "stream config: {codec}, coded {}x{}, visible {width}x{height}, {} fps, clock {} Hz ({})",
            sps.coded.0,
            sps.coded.1,
            self.cfg.fps,
            self.clock_hz,
            self.clock_source.as_str()
        );
        let event = Event::Config {
            codec,
            width,
            height,
            coded_width: sps.coded.0,
            coded_height: sps.coded.1,
            fps: self.cfg.fps,
            clock_hz: self.clock_hz,
            clock_source: self.clock_source.as_str().to_string(),
            ssrc: self.cfg.our_ssrc,
            device_ssrc: self.device_ssrc.unwrap_or(0),
            rotation: self.display.as_ref().and_then(|d| d.rotation),
            point_scale: self.display.as_ref().and_then(|d| d.point_scale),
            current_orientation: self.display.as_ref().and_then(|d| d.current_orientation.clone()),
            native_orientation: self.display.as_ref().and_then(|d| d.native_orientation.clone()),
        };
        if !force && self.last_config.as_ref() == Some(&event) {
            return;
        }
        self.last_config = Some(event.clone());
        self.queue.push_control(Frame::control(&event));
    }

    // ---- RTCP output --------------------------------------------------------------------------

    /// Asks for a keyframe, through the rate limit.
    fn request_keyframe(&mut self, now: Instant) {
        if self.device_ssrc.is_none() || self.rtcp_port.is_none() {
            // Nothing has arrived yet: the stream's first picture is a keyframe anyway.
            debug!("keyframe request before the stream started: not needed");
            return;
        }
        match self.limiter.request(now) {
            Decision::SendNow => self.send_keyframe_request(now),
            Decision::Coalesced => debug!("keyframe request held until the rate-limit window ends"),
        }
    }

    /// RR + SDES + PLI + FIR in one datagram.
    fn send_keyframe_request(&mut self, now: Instant) {
        let (Some(media_ssrc), Some(_)) = (self.device_ssrc, self.rtcp_port) else {
            return;
        };
        let blocks = self.report_blocks(now);
        let pkt = build_keyframe_request(self.cfg.our_ssrc, &self.cfg.cname, media_ssrc, &blocks, self.fir_seq);
        self.fir_seq = self.fir_seq.wrapping_add(1);
        self.outbox.push(pkt);
        self.counters.plis += 1;
        self.counters.rtcp += 1;
        // The request carried a receiver report: the next one is due a full interval from now.
        self.next_rtcp = self.cfg.rtcp_interval.map(|i| now + i);
    }

    /// RR + SDES.
    fn send_report(&mut self, now: Instant) {
        self.next_rtcp = self.cfg.rtcp_interval.map(|i| now + i);
        if self.rtcp_port.is_none() {
            debug!("no RTCP destination yet: skipping the receiver report");
            return;
        }
        let blocks = self.report_blocks(now);
        self.outbox.push(build_liveness(self.cfg.our_ssrc, &self.cfg.cname, &blocks));
        self.counters.rtcp += 1;
    }

    /// Device Hub's periodic RCTL report (experimental, `--device-hub-feedback`).
    fn send_rctl(&mut self, now: Instant) {
        if self.rtcp_port.is_none() || self.rtp.received() == 0 {
            return;
        }
        // The report's clock is 16 bits of milliseconds: it wraps every 65.5 s.
        let clock_ms =
            u16::try_from(now.saturating_duration_since(self.started_at).as_millis() % 0x1_0000).unwrap_or(0);
        self.outbox.push(build_rctl(self.cfg.our_ssrc, clock_ms, self.frames_received, self.rtp.highest_since_base()));
        self.counters.rtcp += 1;
    }

    fn report_blocks(&mut self, now: Instant) -> Vec<ReportBlock> {
        match self.device_ssrc {
            Some(ssrc) if self.rtp.received() > 0 => vec![self.rtp.report_block(now, ssrc)],
            _ => Vec::new(),
        }
    }
}

fn describe(reason: DropReason) -> &'static str {
    match reason {
        DropReason::Loss => "a packet was lost",
        DropReason::Malformed => "a packet could not be parsed",
        DropReason::TooLarge => "the unit is too large",
        DropReason::MissingParams => "a keyframe with no parameter sets seen yet",
    }
}

/// How many bytes of a NAL unit that could not be read are written out in the message.
const MAX_HEX_BYTES: usize = 32;

/// The bytes in hex, the first [`MAX_HEX_BYTES`] of them, then `...` and the length if there are more (the
/// device can send an SPS of any size up to the packet's, and the message goes to the log).
fn hex(b: &[u8]) -> String {
    let shown: String = b.iter().take(MAX_HEX_BYTES).map(|x| format!("{x:02x}")).collect();
    if b.len() > MAX_HEX_BYTES { format!("{shown}... ({} bytes in all)", b.len()) } else { shown }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::cast_possible_truncation,
        reason = "the tests number packets, bytes and times with small loop counters, so a cast does not truncate or lose precision"
    )]
    use super::*;
    use crate::depack::testutil::*;
    use crate::frame::{FLAG_CONTROL, FLAG_KEY, FLAG_PARAMS};
    use crate::sps::testutil::SpsSpec;

    const OUR: u32 = 0x0BAD_CAFE;
    const DEV: u32 = 0xD00D_0001;
    const PORT: u16 = 50001;

    fn sps_1216x2656() -> Vec<u8> {
        SpsSpec::main_l150(1216, 2656).nal()
    }

    /// For tests that let the clock run without RTP.
    fn no_stall(c: &mut EngineConfig) {
        c.rtp_stall_keyframe = None;
        c.rtp_stall_timeout = None;
    }

    /// A sender report from the device (RFC 3550 6.4.1) at NTP second `ntp_secs`.
    fn sr_bytes(ntp_secs: u32, rtp_ts: u32) -> Vec<u8> {
        let mut sr = vec![0x80, 200, 0, 6];
        sr.extend_from_slice(&DEV.to_be_bytes());
        sr.extend_from_slice(&ntp_secs.to_be_bytes());
        sr.extend_from_slice(&0u32.to_be_bytes());
        sr.extend_from_slice(&rtp_ts.to_be_bytes());
        sr.extend_from_slice(&[0; 8]);
        sr
    }

    struct Rig {
        eng: Engine,
        q: Arc<OutQueue>,
        t0: Instant,
        seq: u16,
        ts: u32,
    }

    impl Rig {
        fn new() -> Rig {
            Self::with(|_| {})
        }

        fn with(f: impl FnOnce(&mut EngineConfig)) -> Rig {
            Self::build(f, Arc::new(OutQueue::new()))
        }

        fn with_queue(q: Arc<OutQueue>) -> Rig {
            Self::build(|_| {}, q)
        }

        fn build(f: impl FnOnce(&mut EngineConfig), q: Arc<OutQueue>) -> Rig {
            let mut cfg = EngineConfig::new(OUR);
            f(&mut cfg);
            let t0 = Instant::now();
            Rig { eng: Engine::new(cfg, q.clone(), t0), q, t0, seq: 100, ts: 90_000 }
        }

        fn at(&self, ms: u64) -> Instant {
            self.t0 + Duration::from_millis(ms)
        }

        /// One RTP packet at `ms` after the start, with the rig's own sequence number.
        fn pkt(&mut self, ms: u64, marker: bool, payload: &[u8]) {
            let seq = self.seq;
            self.seq = self.seq.wrapping_add(1);
            self.pkt_seq(ms, seq, marker, payload);
        }

        fn pkt_seq(&mut self, ms: u64, seq: u16, marker: bool, payload: &[u8]) {
            let bytes = rtp_bytes(seq, self.ts, marker, DEV, payload);
            self.eng.on_datagram(self.at(ms), PORT, &bytes);
        }

        /// A key unit: VPS/SPS/PPS in an aggregation packet, the IDR slice in three fragments.
        fn idr(&mut self, ms: u64, with_params: bool) {
            if with_params {
                self.pkt(ms, false, &ap(&[&VPS, &sps_1216x2656(), &PPS]));
            }
            let frags = fu(&idr_nal(&[7; 30]), 12);
            let n = frags.len();
            for (i, f) in frags.iter().enumerate() {
                self.pkt(ms, i == n - 1, f);
            }
            self.ts += 500;
        }

        fn delta(&mut self, ms: u64) {
            self.pkt(ms, true, &trail(&[1, 2, 3, 4]));
            self.ts += 500;
        }

        fn frames(&self) -> Vec<Frame> {
            self.q.drain()
        }

        fn rtcp(&mut self) -> Vec<Vec<(u8, u8, Vec<u8>)>> {
            self.eng.take_rtcp().iter().map(|d| parse_rtcp(d)).collect()
        }
    }

    fn idr_nal(body: &[u8]) -> Vec<u8> {
        idr(body)
    }

    /// Splits a compound RTCP datagram, checking the lengths: (count/fmt, packet type, whole packet).
    fn parse_rtcp(mut b: &[u8]) -> Vec<(u8, u8, Vec<u8>)> {
        let mut v = Vec::new();
        assert_eq!(b.len() % 4, 0);
        while !b.is_empty() {
            assert_eq!(b[0] >> 6, 2, "RTCP version");
            let len = (usize::from(u16::from_be_bytes([b[2], b[3]])) + 1) * 4;
            assert!(len <= b.len(), "packet longer than the datagram");
            v.push((b[0] & 0x1f, b[1], b[..len].to_vec()));
            b = &b[len..];
        }
        v
    }

    fn json(f: &Frame) -> serde_json::Value {
        assert_eq!(f.flags, FLAG_CONTROL);
        serde_json::from_slice(&f.payload).unwrap()
    }

    // ---- frames -------------------------------------------------------------------------------

    #[test]
    fn config_comes_before_the_first_unit_and_describes_the_stream() {
        let mut r = Rig::new();
        r.idr(0, true);
        let f = r.frames();
        assert_eq!(f.len(), 2);
        let c = json(&f[0]);
        assert_eq!(c["event"], "config");
        assert_eq!(c["codec"], "hev1.1.6.L150.B0");
        // No display info, no --visible-size: the SPS size.
        assert_eq!((c["width"].as_u64(), c["height"].as_u64()), (Some(1216), Some(2656)));
        assert_eq!(c["coded_width"], 1216);
        assert_eq!(c["coded_height"], 2656);
        assert_eq!(c["fps"], 60);
        assert_eq!(c["clock_hz"], 24000);
        assert_eq!(c["clock_source"], "default");
        assert_eq!(c["ssrc"], OUR);
        assert_eq!(c["device_ssrc"], DEV);
        // Then the key unit with all three sets.
        assert_eq!(f[1].flags, FLAG_KEY | FLAG_PARAMS);
        assert_eq!(f[1].pts_us, 0);
        assert!(f[1].payload.starts_with(&annexb(&[&VPS, &sps_1216x2656(), &PPS])));
        let s = r.eng.stats();
        assert_eq!((s.packets, s.access_units, s.keyframes, s.dropped_units), (4, 1, 1, 0));
        assert_eq!(s.bytes as usize, f[1].payload.len());
    }

    #[test]
    fn visible_size_override_replaces_the_sps_size_in_the_config() {
        let mut r = Rig::with(|c| c.visible_size = Some((1206, 2622)));
        r.idr(0, true);
        let c = json(&r.frames()[0]);
        assert_eq!((c["width"].as_u64(), c["height"].as_u64()), (Some(1206), Some(2622)));
        assert_eq!((c["coded_width"].as_u64(), c["coded_height"].as_u64()), (Some(1216), Some(2656)));
    }

    #[test]
    fn a_conformance_window_gives_the_visible_size_when_nothing_overrides_it() {
        let mut spec = SpsSpec::main_l150(1216, 2656);
        spec.conf_window = Some([0, 5, 0, 17]);
        let mut r = Rig::new();
        r.pkt(0, false, &ap(&[&VPS, &spec.nal(), &PPS]));
        r.pkt(0, true, &idr(&[1; 4]));
        let c = json(&r.frames()[0]);
        assert_eq!((c["width"].as_u64(), c["height"].as_u64()), (Some(1206), Some(2622)));
        assert_eq!(c["coded_width"], 1216);
    }

    #[test]
    fn delta_units_follow_with_pts_from_the_rtp_clock() {
        let mut r = Rig::new();
        r.idr(0, true);
        r.ts = 90_000 + 24_000; // one second of the 24 kHz clock later
        r.delta(10);
        r.ts += 12_000;
        r.delta(20);
        let f = r.frames();
        assert_eq!(f.len(), 4); // config, key, two deltas
        assert_eq!((f[2].flags, f[2].pts_us), (0, 1_000_000));
        assert_eq!((f[3].flags, f[3].pts_us), (0, 1_000_000 + 500_000 + 20_833));
        assert_eq!(r.eng.stats().access_units, 3);
        assert_eq!(r.eng.stats().keyframes, 1);
    }

    #[test]
    fn pts_clock_handles_the_timestamp_wrap() {
        let mut p = PtsClock::new(24_000);
        assert_eq!(p.pts_us(u32::MAX - 11_999), 0);
        assert_eq!(p.pts_us(12_000), 1_000_000); // 24_000 ticks later, across the wrap
        assert_eq!(p.pts_us(12_000 + 24_000), 2_000_000);
        // Out-of-order timestamps never go below zero.
        let mut p = PtsClock::new(24_000);
        p.pts_us(1000);
        assert_eq!(p.pts_us(500), 0);
    }

    #[test]
    fn a_new_sps_gets_a_new_config() {
        let mut r = Rig::new();
        r.idr(0, true);
        r.delta(10);
        // The display rotated: a keyframe with a different SPS.
        let rotated = SpsSpec::main_l150(2656, 1216).nal();
        r.pkt(20, false, &ap(&[&VPS, &rotated, &PPS]));
        r.pkt(20, true, &idr(&[2; 4]));
        let f = r.frames();
        let configs: Vec<_> = f.iter().filter(|f| f.is_control()).map(json).collect();
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[1]["coded_width"], 2656);
        assert_eq!(configs[1]["coded_height"], 1216);
        // The same SPS again (the next keyframe) does not repeat it.
        r.pkt(30, false, &ap(&[&VPS, &rotated, &PPS]));
        r.pkt(30, true, &idr(&[3; 4]));
        let f = r.frames();
        assert_eq!(f.len(), 1);
        assert!(f[0].is_key());
    }

    #[test]
    fn a_keyframe_without_parameter_sets_carries_the_cached_ones() {
        let mut r = Rig::new();
        r.idr(0, true);
        r.delta(10);
        r.idr(20, false);
        let f = r.frames();
        let key2 = f.last().unwrap();
        assert_eq!(key2.flags, FLAG_KEY | FLAG_PARAMS);
        assert!(key2.payload.starts_with(&annexb(&[&VPS, &sps_1216x2656(), &PPS])));
    }

    // ---- loss, withholding, keyframe requests -------------------------------------------------

    #[test]
    fn a_lost_packet_drops_the_unit_withholds_deltas_and_asks_for_a_keyframe() {
        let mut r = Rig::new();
        r.idr(0, true);
        r.delta(10);
        r.rtcp(); // nothing yet
        r.frames();

        // A unit with a lost packet.
        let seq = r.seq;
        r.pkt_seq(20, seq, false, &fu(&trail(&[0; 40]), 14)[0]);
        r.seq = seq + 2; // skip one
        let last = fu(&trail(&[0; 40]), 14).pop().unwrap();
        r.pkt(20, true, &last);
        r.ts += 500;
        assert!(r.frames().is_empty());
        let s = r.eng.stats();
        assert_eq!((s.lost, s.dropped_units, s.plis), (1, 1, 1));

        // The PLI and FIR went out in one compound datagram with our SSRC, at the device.
        let sent = r.rtcp();
        assert_eq!(sent.len(), 1);
        let types: Vec<(u8, u8)> = sent[0].iter().map(|p| (p.0, p.1)).collect();
        assert_eq!(types, vec![(1, 201), (1, 202), (1, 206), (4, 206)]);

        // The next delta units are not sent: the picture they refer to never left.
        r.delta(30);
        r.delta(40);
        assert!(r.frames().is_empty());
        assert_eq!(r.eng.stats().dropped_units, 3);
        assert_eq!(r.eng.stats().plis, 1, "inside the rate-limit window: coalesced");

        // A keyframe resumes the stream (and brings the cached sets with it).
        r.idr(50, false);
        r.delta(60);
        let f = r.frames();
        assert_eq!(f.len(), 2);
        assert_eq!(f[0].flags, FLAG_KEY | FLAG_PARAMS);
        assert_eq!(f[1].flags, 0);
    }

    #[test]
    fn keyframe_requests_are_rate_limited_and_coalesced_end_to_end() {
        let mut r = Rig::new();
        r.idr(0, true);
        r.frames();
        r.eng.on_keyframe_command(r.at(100));
        assert_eq!(r.eng.stats().plis, 1);
        assert_eq!(r.rtcp().len(), 1);
        // Three more inside the window: nothing is sent.
        for ms in [200, 900, 1500] {
            r.eng.on_keyframe_command(r.at(ms));
        }
        assert_eq!(r.eng.stats().plis, 1);
        assert!(r.rtcp().is_empty());
        // The window ends 2000 ms after the first send: the held request goes out, once.
        assert_eq!(r.eng.next_deadline().map(|d| d - r.t0), Some(Duration::from_millis(2100)));
        r.eng.on_tick(r.at(2099));
        assert_eq!(r.eng.stats().plis, 1);
        r.eng.on_tick(r.at(2100));
        assert_eq!(r.eng.stats().plis, 2);
        assert_eq!(r.rtcp().len(), 1);
        r.eng.on_tick(r.at(2200));
        r.eng.on_tick(r.at(4300));
        assert_eq!(r.eng.stats().plis, 2);
        // Nothing else is held; a new request after a quiet window goes out at once.
        r.eng.on_keyframe_command(r.at(4500));
        assert_eq!(r.eng.stats().plis, 3);
    }

    #[test]
    fn successive_requests_carry_a_fir_sequence_number_that_counts_up() {
        let mut r = Rig::new();
        r.idr(0, true);
        r.eng.on_keyframe_command(r.at(10));
        r.eng.on_keyframe_command(r.at(2500));
        let sent = r.rtcp();
        let fir_seq = |d: &Vec<(u8, u8, Vec<u8>)>| d.iter().find(|p| p.0 == 4 && p.1 == 206).unwrap().2[16];
        assert_eq!(fir_seq(&sent[0]) + 1, fir_seq(&sent[1]));
    }

    #[test]
    fn a_keyframe_command_before_any_packet_is_not_needed_and_not_sent() {
        let mut r = Rig::new();
        r.eng.on_keyframe_command(r.at(10));
        assert_eq!(r.eng.stats().plis, 0);
        assert!(r.rtcp().is_empty());
        // And it did not use up the rate-limit window.
        r.idr(20, true);
        r.eng.on_keyframe_command(r.at(30));
        assert_eq!(r.eng.stats().plis, 1);
    }

    #[test]
    fn a_full_output_queue_drops_the_backlog_and_waits_for_a_keyframe() {
        let q = Arc::new(OutQueue::with_limits(2, 1 << 20));
        let mut r = Rig::with_queue(q.clone());
        r.idr(0, true); // config + unit 1
        r.delta(5); // unit 2
        r.delta(10); // does not fit: both queued units and this one are dropped
        let s = r.eng.stats();
        assert_eq!(s.access_units, 0, "nothing was written: both were dropped from the queue");
        assert_eq!(s.dropped_units, 3);
        assert_eq!(s.plis, 1, "a keyframe was asked for");
        assert_eq!(q.queued_units(), (0, 0));
        // The config is still queued, and later deltas are withheld.
        r.delta(15);
        assert_eq!(r.eng.stats().dropped_units, 4);
        r.idr(20, false);
        let f = q.drain();
        assert!(f[0].is_control());
        assert_eq!(f.last().unwrap().flags, FLAG_KEY | FLAG_PARAMS);
    }

    #[test]
    fn a_keyframe_that_does_not_fit_leaves_the_stream_waiting() {
        let q = Arc::new(OutQueue::with_limits(1, 1 << 20));
        let mut r = Rig::with_queue(q);
        r.idr(0, true);
        r.idr(5, false); // second key: the queue holds one unit, so both are dropped
        assert_eq!(r.eng.stats().dropped_units, 2);
        r.delta(10); // still waiting for a keyframe
        assert_eq!(r.eng.stats().dropped_units, 3);
        let s = r.eng.stats();
        assert_eq!((s.access_units, s.keyframes), (0, 0), "nothing reached the writer");
    }

    // ---- RTCP ---------------------------------------------------------------------------------

    #[test]
    fn receiver_reports_go_out_every_interval_with_our_ssrc_and_a_block_for_the_device() {
        let mut r = Rig::with(|c| {
            c.rtcp_interval = Some(Duration::from_secs(5));
            no_stall(c);
        });
        r.idr(0, true);
        r.delta(10);
        r.delta(20);
        assert!(r.eng.take_rtcp().is_empty());
        assert_eq!(r.eng.next_deadline().map(|d| d - r.t0), Some(Duration::from_secs(5)));
        r.eng.on_tick(r.at(4999));
        assert!(r.eng.take_rtcp().is_empty());
        r.eng.on_tick(r.at(5000));
        let sent = r.eng.take_rtcp();
        assert_eq!(sent.len(), 1);
        let pk = parse_rtcp(&sent[0]);
        assert_eq!(pk.iter().map(|p| p.1).collect::<Vec<_>>(), vec![201, 202]);

        // Receiver report: RFC 3550 6.4.2: V=2 P=0 RC=1, PT=201, length 7, our SSRC, one block.
        let rr = &pk[0].2;
        assert_eq!(rr.len(), 32);
        assert_eq!(&rr[0..4], &[0x81, 201, 0x00, 0x07]);
        assert_eq!(&rr[4..8], &OUR.to_be_bytes());
        let block = &rr[8..32];
        assert_eq!(&block[0..4], &DEV.to_be_bytes(), "report block source");
        assert_eq!(block[4], 0, "fraction lost");
        assert_eq!(&block[5..8], &[0, 0, 0], "cumulative lost");
        // Six RTP packets so far (seq 100..=105): extended highest sequence 105.
        assert_eq!(&block[8..12], &105u32.to_be_bytes());

        // SDES: RFC 3550 6.5: V=2 SC=1, PT=202, our SSRC, CNAME item.
        let sdes = &pk[1].2;
        assert_eq!(sdes[0], 0x81);
        assert_eq!(sdes[1], 202);
        assert_eq!(&sdes[4..8], &OUR.to_be_bytes());
        assert_eq!(sdes[8], 1, "CNAME item");
        let n = usize::from(sdes[9]);
        assert_eq!(&sdes[10..10 + n], format!("{OUR:08x}@scrios").as_bytes());
        assert_eq!(sdes[10 + n], 0, "end of items");

        // And again an interval later, not before.
        r.eng.on_tick(r.at(9999));
        assert!(r.eng.take_rtcp().is_empty());
        r.eng.on_tick(r.at(10_000));
        assert_eq!(r.eng.take_rtcp().len(), 1);
        assert_eq!(r.eng.stats().rtcp, 2);
    }

    #[test]
    fn a_zero_interval_means_no_receiver_reports() {
        let mut r = Rig::with(|c| {
            c.rtcp_interval = None;
            no_stall(c);
        });
        r.idr(0, true);
        assert_eq!(r.eng.next_deadline().map(|d| d - r.t0), Some(Duration::from_secs(5)), "only the stats tick");
        r.eng.on_tick(r.at(5000));
        assert!(r.eng.take_rtcp().is_empty());
    }

    #[test]
    fn a_receiver_report_before_any_packet_has_no_report_block() {
        let mut r = Rig::with(|c| c.rtcp_port = Some(PORT));
        r.eng.on_tick(r.at(5000));
        let sent = r.rtcp();
        assert_eq!(sent.len(), 1);
        assert_eq!(&sent[0][0].2[..], &[0x80, 201, 0, 1, 0x0B, 0xAD, 0xCA, 0xFE][..]);
    }

    #[test]
    fn the_report_block_carries_loss_and_the_last_sender_report() {
        let mut r = Rig::new();
        r.idr(0, true);
        // A sender report from the device: SR, its SSRC, NTP 0x11112222_33334444.
        let mut sr = vec![0x80, 200, 0, 6];
        sr.extend_from_slice(&DEV.to_be_bytes());
        sr.extend_from_slice(&0x1111_2222u32.to_be_bytes());
        sr.extend_from_slice(&0x3333_4444u32.to_be_bytes());
        sr.extend_from_slice(&[0; 12]);
        r.eng.on_datagram(r.at(100), PORT, &sr);
        // One lost packet (104); the unit that follows is dropped and a keyframe asked for at
        // 200 ms: that request carries the receiver report.
        let seq = r.seq;
        r.pkt_seq(200, seq + 1, true, &trail(&[1; 4]));
        let sent = r.rtcp();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0][0].1, 201);
        let block = sent[0][0].2[8..32].to_vec();
        assert_eq!(&block[0..4], &DEV.to_be_bytes());
        assert_eq!(block[4], ((1u32 << 8) / 6) as u8, "fraction lost: 1 of 6 expected");
        assert_eq!(&block[5..8], &[0, 0, 1], "cumulative lost");
        assert_eq!(&block[8..12], &105u32.to_be_bytes(), "extended highest sequence number");
        assert_eq!(&block[16..20], &0x2222_3333u32.to_be_bytes(), "LSR: middle 32 bits of the NTP time");
        let dlsr = u32::from_be_bytes(block[20..24].try_into().unwrap());
        assert_eq!(dlsr, 6553, "DLSR: 100 ms in 1/65536 s");
    }

    #[test]
    fn pli_and_fir_follow_rfc_4585_and_rfc_5104() {
        let mut r = Rig::with(|c| {
            c.stats_interval = None;
            no_stall(c);
        });
        r.idr(0, true);
        r.eng.on_keyframe_command(r.at(10));
        let sent = r.rtcp();
        let pli = &sent[0][2].2;
        // RFC 4585 6.3.1: V=2 FMT=1 PT=206 length=2, sender SSRC (ours), media source SSRC (the device's).
        assert_eq!(&pli[..], &[0x81, 0xCE, 0x00, 0x02, 0x0B, 0xAD, 0xCA, 0xFE, 0xD0, 0x0D, 0x00, 0x01][..]);
        // RFC 5104 4.3.1: V=2 FMT=4 PT=206 length=4, sender SSRC, media source SSRC 0, FCI: SSRC, seq, 3 reserved bytes.
        let fir = &sent[0][3].2;
        assert_eq!(&fir[..4], &[0x84, 0xCE, 0x00, 0x04]);
        assert_eq!(&fir[4..8], &OUR.to_be_bytes());
        assert_eq!(&fir[8..12], &[0, 0, 0, 0]);
        assert_eq!(&fir[12..16], &DEV.to_be_bytes());
        assert_eq!(&fir[17..20], &[0, 0, 0]);
        // The request is also a valid compound packet: it starts with the report.
        assert_eq!(sent[0][0].1, 201);
        assert_eq!(sent[0][1].1, 202);
        // The report timer restarted with it.
        assert_eq!(r.eng.next_deadline().map(|d| d - r.t0), Some(Duration::from_millis(5010)));
    }

    #[test]
    fn rtcp_goes_to_the_port_rtp_comes_from() {
        let mut r = Rig::with(|c| c.rtcp_port = Some(50001));
        assert_eq!(r.eng.rtcp_port(), Some(50001));
        let bytes = rtp_bytes(1, 1, true, DEV, &trail(&[1; 4]));
        r.eng.on_datagram(r.at(1), 40000, &bytes);
        assert_eq!(r.eng.rtcp_port(), Some(40000));
        // The first packet decides; later ones from elsewhere do not move it.
        r.eng.on_datagram(r.at(2), 40001, &rtp_bytes(2, 1, true, DEV, &trail(&[1; 4])));
        assert_eq!(r.eng.rtcp_port(), Some(40000));
    }

    // ---- stream hygiene -----------------------------------------------------------------------

    #[test]
    fn other_payload_types_and_junk_are_ignored() {
        let mut r = Rig::new();
        let mut other = rtp_bytes(1, 1, true, DEV, &trail(&[1; 4]));
        other[1] = 96 | 0x80;
        r.eng.on_datagram(r.at(1), PORT, &other);
        r.eng.on_datagram(r.at(2), PORT, &[1, 2, 3]);
        r.eng.on_datagram(r.at(3), PORT, &[]);
        assert_eq!(r.eng.stats().packets, 0);
        assert!(r.frames().is_empty());
        assert_eq!(r.eng.device_ssrc(), None);
    }

    #[test]
    fn an_sps_that_cannot_be_read_is_dumped_in_the_message_only_up_to_a_limit() {
        let mut r = Rig::new();
        // Short: all of it.
        let short = vec![0x42, 0x01, 0xAB, 0xCD];
        let why = r.eng.announce(&short).unwrap_err();
        assert!(why.ends_with("(SPS bytes: 4201abcd)"), "{why}");
        // Exactly the limit: all of it, nothing added.
        let mut exact = vec![0x42, 0x01];
        exact.resize(MAX_HEX_BYTES, 0xAB);
        let why = r.eng.announce(&exact).unwrap_err();
        assert!(why.ends_with(&format!("(SPS bytes: {})", hex(&exact))) && !why.contains("..."), "{why}");
        assert_eq!(hex(&exact).len(), 2 * MAX_HEX_BYTES);
        // Long: the first 32 bytes, an ellipsis and the size.
        let mut long = vec![0x42, 0x01];
        long.resize(5000, 0xAB);
        let why = r.eng.announce(&long).unwrap_err();
        assert!(why.len() < 250, "{} bytes: {why}", why.len());
        assert!(why.ends_with(&format!("(SPS bytes: 4201{}... (5000 bytes in all))", "ab".repeat(30))), "{why}");
        assert!(r.frames().is_empty(), "nothing was announced");
    }

    #[test]
    fn the_first_rtp_ssrc_wins_over_the_answers_and_a_later_change_starts_over() {
        let mut r = Rig::with(|c| c.device_ssrc = Some(0x1111));
        assert_eq!(r.eng.device_ssrc(), Some(0x1111));
        r.idr(0, true);
        assert_eq!(r.eng.device_ssrc(), Some(DEV));
        assert_eq!(json(&r.frames()[0])["device_ssrc"], DEV);
        // The source changes: new SSRC, the sequence restarts, the state is reset (once three packets in a
        // row have come from it, see `a_new_ssrc_needs_a_run_of_packets_...`).
        for (i, seq) in [5000u16, 5001, 5002].into_iter().enumerate() {
            assert_eq!(r.eng.device_ssrc(), Some(DEV), "still the old one after {i} packets");
            let bytes = rtp_bytes(seq, 5, i == 2, 0xBEEF, &idr(&[1; 4]));
            r.eng.on_datagram(r.at(50), PORT, &bytes);
        }
        assert_eq!(r.eng.device_ssrc(), Some(0xBEEF));
        // No parameter sets since the reset: that keyframe cannot be sent.
        assert!(r.frames().is_empty());
        assert_eq!(r.eng.stats().dropped_units, 1);
    }

    #[test]
    fn a_new_ssrc_needs_a_run_of_packets_before_the_stream_starts_over() {
        let mut r = Rig::with(no_stall);
        r.idr(0, true);
        r.delta(10);
        assert_eq!(r.frames().len(), 3, "config, the key unit and one delta unit");
        let before = r.eng.stats();
        let stray = |seq: u16, ssrc: u32| rtp_bytes(seq, 77, true, ssrc, &trail(&[9; 4]));

        // One stray packet, and another from a third SSRC, then one of the stream's own: nothing is reset, the
        // strays are not counted as packets and are not decoded.
        r.eng.on_datagram(r.at(20), PORT, &stray(900, 0xBEEF));
        r.eng.on_datagram(r.at(21), PORT, &stray(901, 0xCAFE));
        r.delta(22);
        assert_eq!(r.eng.device_ssrc(), Some(DEV));
        let f = r.frames();
        assert_eq!(f.len(), 1, "only the stream's own delta unit: {f:?}");
        assert!(!f[0].is_control() && f[0].flags & FLAG_KEY == 0);
        let after = r.eng.stats();
        assert_eq!(after.packets, before.packets + 1);
        assert_eq!(after.dropped_units, before.dropped_units, "nothing was dropped, nothing needs a keyframe");

        // Two from 0xBEEF, a packet of the stream between them, two more: the run was broken, so still no switch.
        r.eng.on_datagram(r.at(30), PORT, &stray(910, 0xBEEF));
        r.eng.on_datagram(r.at(31), PORT, &stray(911, 0xBEEF));
        r.delta(32);
        r.eng.on_datagram(r.at(33), PORT, &stray(912, 0xBEEF));
        r.eng.on_datagram(r.at(34), PORT, &stray(913, 0xBEEF));
        assert_eq!(r.eng.device_ssrc(), Some(DEV));
        assert_eq!(r.frames().len(), 1);

        // Three in a row switch: the stream starts over, and all three go to the depacketizer.
        r.delta(36); // the stream's own, so that the count starts at 0
        r.frames();
        r.eng.on_datagram(r.at(40), PORT, &stray(920, 0xBEEF));
        assert_eq!(r.eng.device_ssrc(), Some(DEV));
        r.eng.on_datagram(r.at(41), PORT, &stray(921, 0xBEEF));
        assert_eq!(r.eng.device_ssrc(), Some(DEV));
        r.eng.on_datagram(r.at(42), PORT, &stray(922, 0xBEEF));
        assert_eq!(r.eng.device_ssrc(), Some(0xBEEF));
        // All three reached the depacketizer (they were delta units of a stream with no parameter sets).
        assert_eq!(r.eng.stats().packets, after.packets + 1 + 1 + 3);
        assert_eq!(r.eng.stats().dropped_units, after.dropped_units + 3, "delta units, and no keyframe yet");
    }

    #[test]
    fn strays_of_another_ssrc_do_not_count_as_media_for_the_stall_watch() {
        let mut r = Rig::with(|c| {
            c.rtp_stall_keyframe = None;
            c.rtp_stall_timeout = Some(Duration::from_secs(15));
            c.no_data_timeout = Duration::from_secs(3600);
        });
        r.idr(0, true);
        // The device goes quiet; a stray of another SSRC every second (never three in a row of the same one)
        // does not keep the stream alive.
        for s in 1..=14u16 {
            let stray = rtp_bytes(s, 1, true, if s % 2 == 0 { 0xBEEF } else { 0xCAFE }, &trail(&[9; 4]));
            r.eng.on_datagram(r.at(u64::from(s) * 1000), PORT, &stray);
            r.eng.on_tick(r.at(u64::from(s) * 1000));
            assert_eq!(r.eng.take_fatal(), None);
        }
        r.eng.on_tick(r.at(15_000));
        assert!(r.eng.take_fatal().unwrap().contains("no RTP media"));
    }

    #[test]
    fn silence_is_fatal_but_sender_reports_count_as_life() {
        let mut r = Rig::with(|c| {
            c.no_data_timeout = Duration::from_secs(30);
            no_stall(c);
        });
        r.idr(0, true);
        r.eng.on_tick(r.at(29_000));
        assert_eq!(r.eng.take_fatal(), None);
        // A sender report resets the silence clock.
        let mut sr = vec![0x80, 200, 0, 6];
        sr.extend_from_slice(&DEV.to_be_bytes());
        sr.extend_from_slice(&[0; 20]);
        r.eng.on_datagram(r.at(29_500), PORT, &sr);
        r.eng.on_tick(r.at(58_000));
        assert_eq!(r.eng.take_fatal(), None);
        r.eng.on_tick(r.at(59_500));
        assert!(r.eng.take_fatal().unwrap().contains("no data from the device"));
    }

    #[test]
    fn stats_events_come_at_the_interval_and_count_since_start() {
        let mut r = Rig::with(|c| c.stats_interval = Some(Duration::from_secs(5)));
        r.idr(0, true);
        r.delta(10);
        r.frames();
        r.eng.on_tick(r.at(5000));
        let f = r.frames();
        assert_eq!(f.len(), 1);
        let s = json(&f[0]);
        assert_eq!(s["event"], "stats");
        assert_eq!(s["packets"], 5);
        assert_eq!(s["access_units"], 2);
        assert_eq!(s["keyframes"], 1);
        assert_eq!(s["lost"], 0);
        assert_eq!(s["dropped_units"], 0);
        r.eng.on_tick(r.at(9999));
        assert!(r.frames().is_empty());
        r.eng.on_tick(r.at(10_000));
        assert_eq!(r.frames().len(), 1);

        // Interval 0: none.
        let mut r = Rig::with(|c| c.stats_interval = None);
        r.eng.on_tick(r.at(60_000));
        assert!(r.frames().is_empty());
    }

    // ---- keyframe cancellation, stalls, feedback, clock, pts ------------------------------------

    #[test]
    fn a_keyframe_going_out_cancels_a_held_request() {
        let mut r = Rig::with(no_stall);
        r.idr(0, true);
        r.frames();
        r.eng.on_keyframe_command(r.at(100)); // sent
        r.eng.on_keyframe_command(r.at(500)); // held until 2100
        assert_eq!(r.eng.stats().plis, 1);
        assert_eq!(r.eng.next_deadline().map(|d| d - r.t0), Some(Duration::from_millis(2100)));
        // The device's IDR (the answer to the first request) arrives.
        r.idr(600, false);
        assert_eq!(r.frames().len(), 1);
        r.eng.on_tick(r.at(2100));
        r.eng.on_tick(r.at(2200));
        assert_eq!(r.eng.stats().plis, 1, "no second request 2 s after an IDR already arrived");
        assert_ne!(r.eng.next_deadline().map(|d| d - r.t0), Some(Duration::from_millis(2100)));
        // A request after the window still goes out.
        r.eng.on_keyframe_command(r.at(3000));
        assert_eq!(r.eng.stats().plis, 2);
    }

    #[test]
    fn a_held_request_survives_a_keyframe_that_did_not_get_out() {
        // The key unit overflowed the queue: nothing was written, so the held request stays.
        let q = Arc::new(OutQueue::with_limits(1, 1 << 20));
        let mut r = Rig::with_queue(q);
        r.idr(0, true);
        r.eng.on_keyframe_command(r.at(100));
        r.eng.on_keyframe_command(r.at(200));
        r.idr(300, false); // overflows
        assert_eq!(r.eng.next_deadline().map(|d| d - r.t0), Some(Duration::from_millis(2100)));
    }

    #[test]
    fn rtp_silence_asks_for_a_keyframe_then_fails_and_sender_reports_and_junk_do_not_count() {
        let mut r = Rig::with(|c| {
            c.rtcp_interval = None;
            c.stats_interval = None;
        });
        r.idr(0, true);
        r.frames();
        r.rtcp();
        // Sender reports (and junk) every second, no RTP.
        for s in 1..=14u64 {
            r.eng.on_datagram(r.at(s * 1000), PORT, &sr_bytes(s as u32, 90_000 + (s as u32) * 24_000));
            r.eng.on_datagram(r.at(s * 1000), PORT, &[1, 2, 3]);
            r.eng.on_tick(r.at(s * 1000));
            if s < 5 {
                assert_eq!(r.eng.stats().plis, 0, "not yet at {s} s");
            }
        }
        assert_eq!(r.eng.take_fatal(), None, "still alive at 14 s");
        let plis = r.eng.stats().plis;
        assert!((1..=6).contains(&plis), "asked for a keyframe, rate-limited: {plis}");
        // Every request is a keyframe request to the device.
        for d in r.eng.take_rtcp() {
            assert_eq!(d[1], 201);
        }
        r.eng.on_datagram(r.at(15_000), PORT, &sr_bytes(15, 1));
        r.eng.on_tick(r.at(15_000));
        let msg = r.eng.take_fatal().unwrap();
        assert!(msg.contains("no RTP media from the device for 15 s"), "{msg}");
    }

    #[test]
    fn rtp_arriving_again_clears_the_stall() {
        let mut r = Rig::with(|c| {
            c.rtcp_interval = None;
            c.stats_interval = None;
        });
        r.idr(0, true);
        r.eng.on_tick(r.at(4000));
        r.delta(4500);
        r.eng.on_tick(r.at(9000)); // 4.5 s after the last RTP: no request
        assert_eq!(r.eng.stats().plis, 0);
        r.eng.on_tick(r.at(9500));
        assert_eq!(r.eng.stats().plis, 1);
        r.delta(10_000);
        r.eng.on_tick(r.at(24_900));
        assert_eq!(r.eng.take_fatal(), None, "14.9 s after the last RTP");
        r.eng.on_tick(r.at(25_000));
        assert!(r.eng.take_fatal().unwrap().contains("no RTP media"));
    }

    #[test]
    fn the_stall_limits_can_be_turned_off() {
        let mut r = Rig::with(|c| {
            no_stall(c);
            c.rtcp_interval = None;
            c.stats_interval = None;
        });
        r.idr(0, true);
        for s in 1..=25u64 {
            r.eng.on_datagram(r.at(s * 1000), PORT, &sr_bytes(s as u32, 1));
            r.eng.on_tick(r.at(s * 1000));
        }
        assert_eq!(r.eng.take_fatal(), None);
        assert_eq!(r.eng.stats().plis, 0);
    }

    #[test]
    fn device_hub_feedback_is_off_by_default() {
        let mut r = Rig::with(no_stall);
        r.idr(0, true);
        r.delta(10);
        r.eng.on_tick(r.at(100));
        assert!(r.eng.take_rtcp().is_empty());
    }

    #[test]
    fn device_hub_feedback_sends_a_frame_ack_per_picture_and_rctl_every_50_ms() {
        let mut r = Rig::with(|c| {
            c.device_hub_feedback = true;
            c.rtcp_interval = None;
            c.stats_interval = None;
            no_stall(c);
        });
        r.idr(0, true); // RTP timestamp 90000
        let out = r.eng.take_rtcp();
        // 80 cc 00 03 | our SSRC | 00 00 00 05 | RTP timestamp: the APP packet Device Hub sends per frame.
        let mut ack = vec![0x80, 0xCC, 0x00, 0x03];
        ack.extend_from_slice(&OUR.to_be_bytes());
        ack.extend_from_slice(&[0, 0, 0, 5]);
        ack.extend_from_slice(&90_000u32.to_be_bytes());
        assert_eq!(out, vec![ack]);

        r.delta(10); // sequence numbers 100..=104 so far
        assert_eq!(r.eng.take_rtcp().len(), 1, "one more ack");
        assert_eq!(r.eng.next_deadline().map(|d| d - r.t0), Some(Duration::from_millis(50)));
        r.eng.on_tick(r.at(49));
        assert!(r.eng.take_rtcp().is_empty());
        r.eng.on_tick(r.at(50));
        let out = r.eng.take_rtcp();
        assert_eq!(out.len(), 1);
        let rctl = &out[0];
        assert_eq!(rctl.len(), 32);
        assert_eq!(&rctl[..4], &[0x80, 0xCC, 0x00, 0x07]);
        assert_eq!(&rctl[4..8], &OUR.to_be_bytes());
        assert_eq!(&rctl[8..12], b"RCTL");
        assert_eq!(&rctl[16..18], &2u16.to_be_bytes(), "pictures received");
        assert_eq!(&rctl[24..26], &50u16.to_be_bytes(), "milliseconds since the start");
        assert_eq!(&rctl[28..30], &4u16.to_be_bytes(), "highest sequence number since the base");
        r.eng.on_tick(r.at(100));
        assert_eq!(r.eng.take_rtcp().len(), 1);
        // A dropped (incomplete) unit is not acknowledged.
        let seq = r.seq;
        r.pkt_seq(110, seq + 1, true, &trail(&[1; 4]));
        let out = r.eng.take_rtcp();
        assert!(!out.is_empty(), "the loss asks for a keyframe");
        assert!(out.iter().all(|d| d[1] != 204), "no acknowledgement for a unit that was not whole");
    }

    #[test]
    fn the_clock_rate_is_checked_against_two_sender_reports() {
        // Confirmed: 48 000 ticks in 2 s is 24 kHz.
        let mut r = Rig::with(no_stall);
        r.idr(0, true);
        r.frames();
        r.eng.on_datagram(r.at(100), PORT, &sr_bytes(1000, 5_000));
        r.eng.on_datagram(r.at(2100), PORT, &sr_bytes(1002, 5_000 + 48_000));
        assert_eq!(r.eng.clock(), (24_000, ClockSource::Default));
        assert!(r.frames().is_empty(), "no new config");
    }

    #[test]
    fn a_clock_that_disagrees_by_more_than_10_percent_is_replaced_and_announced_without_a_pts_jump() {
        let mut r = Rig::with(no_stall);
        r.idr(0, true); // pts 0 at RTP timestamp 90000
        r.delta(10); // 500 ticks later at 24 kHz: 20 833 us
        let f = r.frames();
        assert_eq!(f.last().unwrap().pts_us, 20_833);

        // Two sender reports one second apart that say 90 000 ticks a second.
        r.eng.on_datagram(r.at(100), PORT, &sr_bytes(1000, 5_000));
        assert_eq!(r.eng.clock().1, ClockSource::Default);
        r.eng.on_datagram(r.at(1100), PORT, &sr_bytes(1001, 5_000 + 90_000));
        assert_eq!(r.eng.clock(), (90_000, ClockSource::Measured));

        // The config is sent again with the new rate and its source ...
        let f = r.frames();
        assert_eq!(f.len(), 1);
        let c = json(&f[0]);
        assert_eq!(c["event"], "config");
        assert_eq!(c["clock_hz"], 90_000);
        assert_eq!(c["clock_source"], "measured");
        assert_eq!(c["codec"], "hev1.1.6.L150.B0");
        // ... and the next picture continues from the last time, in the new rate: 500 ticks = 5 555 us.
        r.delta(1200);
        assert_eq!(r.frames()[0].pts_us, 20_833 + 5_555);
        // The check happens once.
        r.eng.on_datagram(r.at(3100), PORT, &sr_bytes(1003, 5_000 + 3 * 90_000));
        assert_eq!(r.eng.clock().0, 90_000);
    }

    #[test]
    fn a_negotiated_rate_is_reported_as_such_and_kept_when_the_reports_agree() {
        let mut r = Rig::with(|c| {
            c.clock_hz = 90_000;
            c.clock_source = ClockSource::Negotiated;
            no_stall(c);
        });
        r.idr(0, true);
        let c = json(&r.frames()[0]);
        assert_eq!((c["clock_hz"].as_u64(), c["clock_source"].as_str()), (Some(90_000), Some("negotiated")));
        r.eng.on_datagram(r.at(100), PORT, &sr_bytes(7, 0));
        r.eng.on_datagram(r.at(1300), PORT, &sr_bytes(9, 180_000)); // 90 kHz over 2 s
        assert_eq!(r.eng.clock(), (90_000, ClockSource::Negotiated));
        assert!(r.frames().is_empty());
        // A wrong negotiated rate is overruled by the measurement.
        let mut r = Rig::with(|c| {
            c.clock_hz = 90_000;
            c.clock_source = ClockSource::Negotiated;
            no_stall(c);
        });
        r.idr(0, true);
        r.frames();
        r.eng.on_datagram(r.at(100), PORT, &sr_bytes(7, 0));
        r.eng.on_datagram(r.at(1300), PORT, &sr_bytes(9, 48_000)); // 24 kHz over 2 s
        assert_eq!(r.eng.clock(), (24_000, ClockSource::Measured));
    }

    #[test]
    fn the_clock_correction_snaps_to_24_or_90_khz() {
        assert_eq!(clock_correction(24_000, 24_500.0), None); // within 10%
        assert_eq!(clock_correction(24_000, 90_000.0), Some(90_000));
        assert_eq!(clock_correction(24_000, 85_000.0), Some(90_000));
        assert_eq!(clock_correction(90_000, 24_500.0), Some(24_000));
        assert_eq!(clock_correction(24_000, 30_000.0), None); // off by 25%, but still nearest to 24k
        assert_eq!(clock_correction(24_000, 500.0), None);
        assert_eq!(clock_correction(24_000, f64::NAN), None);
        assert_eq!(clock_correction(24_000, f64::INFINITY), None);
    }

    #[test]
    fn a_sender_report_clock_check_ignores_reports_too_close_or_too_far_apart() {
        let mut r = Rig::with(no_stall);
        r.idr(0, true);
        r.frames();
        r.eng.on_datagram(r.at(10), PORT, &sr_bytes(1000, 0));
        // Half a second later: not measurable, and no conclusion is drawn from it.
        let mut sr = sr_bytes(1000, 45_000);
        sr[12..16].copy_from_slice(&0x8000_0000u32.to_be_bytes()); // .5 s
        r.eng.on_datagram(r.at(20), PORT, &sr);
        assert_eq!(r.eng.clock().1, ClockSource::Default);
        // Three minutes later: the first report is dropped as too old, this one becomes the first.
        r.eng.on_datagram(r.at(30), PORT, &sr_bytes(1200, 0));
        r.eng.on_datagram(r.at(40), PORT, &sr_bytes(1202, 180_000)); // 90 kHz over 2 s
        assert_eq!(r.eng.clock(), (90_000, ClockSource::Measured));
    }

    #[test]
    fn pts_continues_without_a_jump_when_the_devices_ssrc_changes() {
        let mut r = Rig::with(no_stall);
        r.idr(0, true);
        r.ts = 90_000 + 24_000;
        r.delta(10); // pts 1 000 000
        r.frames();
        // A new source: new SSRC, new sequence numbers, timestamps from nowhere. Its first two packets are held
        // until the third makes the run, and then they are the start of the new stream, none of them lost.
        let sps = sps_1216x2656();
        let frags = fu(&idr(&[4; 8]), 4);
        assert_eq!(frags.len(), 2);
        r.eng.on_datagram(r.at(50), PORT, &rtp_bytes(7000, 7, false, 0xBEEF, &ap(&[&VPS, &sps, &PPS])));
        r.eng.on_datagram(r.at(50), PORT, &rtp_bytes(7001, 7, false, 0xBEEF, &frags[0]));
        assert_eq!(r.eng.device_ssrc(), Some(DEV));
        r.eng.on_datagram(r.at(50), PORT, &rtp_bytes(7002, 7, true, 0xBEEF, &frags[1]));
        let f = r.frames();
        assert_eq!(f.len(), 2, "config again (it names the new SSRC), then the keyframe");
        assert_eq!(json(&f[0])["device_ssrc"], 0xBEEF);
        // One frame (1/60 s) after the last time sent.
        assert_eq!(f[1].pts_us, 1_000_000 + 16_666);
        // And from there on in step with the new timestamps (500 ticks = 20 833 us).
        r.eng.on_datagram(r.at(60), PORT, &rtp_bytes(7003, 507, true, 0xBEEF, &trail(&[1; 4])));
        assert_eq!(r.frames()[0].pts_us, 1_000_000 + 16_666 + 20_833);
    }

    #[test]
    fn pts_never_goes_back_when_a_timestamp_does() {
        let mut p = PtsClock::new(24_000);
        assert_eq!(p.pts_us(1000), 0);
        assert_eq!(p.pts_us(1000 + 24_000), 1_000_000);
        // A unit that is half a second behind: the previous time again, not 500 000.
        assert_eq!(p.pts_us(1000 + 12_000), 1_000_000);
        // The same timestamp twice is fine (equal, not lower).
        assert_eq!(p.pts_us(1000 + 12_000), 1_000_000);
        // Going forward from the late one follows the timestamps again: 2400 ticks past it is 100 ms, but that
        // is still behind the 1 s already given, so the time waits for them to catch up ...
        assert_eq!(p.pts_us(1000 + 14_400), 1_000_000);
        // ... and goes on from there once they have: 24 000 + 2 400 ticks is 1.1 s.
        assert_eq!(p.pts_us(1000 + 26_400), 1_100_000);
        // A timestamp from before the very first one is the first time.
        let mut p = PtsClock::new(24_000);
        assert_eq!(p.pts_us(50_000), 0);
        assert_eq!(p.pts_us(49_000), 0);
    }

    #[test]
    fn the_pts_of_the_units_written_never_goes_back() {
        let mut r = Rig::with(no_stall);
        r.idr(0, true);
        r.delta(10);
        r.delta(20);
        // A delta unit with an earlier timestamp (reordered by the sender): written, with the time before.
        r.ts -= 2000;
        r.delta(30);
        r.ts += 2000 + 500;
        r.delta(40);
        r.delta(50);
        let pts: Vec<u64> = r.frames().iter().filter(|f| !f.is_control()).map(|f| f.pts_us).collect();
        assert_eq!(pts.len(), 6);
        assert!(pts.windows(2).all(|w| w[0] <= w[1]), "{pts:?}");
        assert_eq!(pts[3], pts[2], "the late unit gets the time of the one before it");
        assert!(pts[5] > pts[3], "{pts:?}");
    }

    #[test]
    fn pts_clock_rebase_and_rate_change() {
        let mut p = PtsClock::new(24_000);
        assert_eq!(p.pts_us(1000), 0);
        assert_eq!(p.pts_us(1000 + 24_000), 1_000_000);
        p.rebase(16_666);
        assert_eq!(p.pts_us(5), 1_016_666, "the first unit of the new source");
        assert_eq!(p.pts_us(5 + 12_000), 1_516_666);
        p.set_hz(90_000);
        assert_eq!(p.pts_us(5 + 12_000 + 90_000), 2_516_666);
    }

    #[test]
    fn the_stats_count_what_the_writer_wrote_not_what_was_queued() {
        let mut r = Rig::with(no_stall);
        r.idr(0, true);
        r.delta(10);
        let s = r.eng.stats();
        assert_eq!((s.access_units, s.bytes, s.keyframes), (0, 0, 0), "queued, not yet written");
        let frames = r.frames(); // stands for the writer
        let units: Vec<_> = frames.iter().filter(|f| !f.is_control()).collect();
        let s = r.eng.stats();
        assert_eq!(s.access_units, 2);
        assert_eq!(s.keyframes, 1);
        assert_eq!(s.bytes as usize, units.iter().map(|f| f.payload.len()).sum::<usize>());
    }

    // ---- H.264 ---------------------------------------------------------------------------------

    mod h264 {
        use super::*;
        use crate::avc::testutil::{
            AvcSpsSpec, PPS as PPS264, avc1_box, avcc_record, fu_a, idr as idr264, make_box, slice, stap_a,
        };

        const PT: u8 = 123;

        fn datagram(seq: u16, ts: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
            let mut d = rtp_bytes(seq, ts, marker, DEV, payload);
            d[1] = PT | if marker { 0x80 } else { 0 };
            d
        }

        struct H264Rig {
            eng: Engine,
            q: Arc<OutQueue>,
            t0: Instant,
            seq: u16,
            ts: u32,
        }

        impl H264Rig {
            fn new() -> Self {
                let t0 = Instant::now();
                let q = Arc::new(OutQueue::new());
                let cfg = EngineConfig::new(OUR).for_codec(CodecKind::H264);
                H264Rig { eng: Engine::new(cfg, q.clone(), t0), q, t0, seq: 100, ts: 90_000 }
            }

            fn at(&self, ms: u64) -> Instant {
                self.t0 + Duration::from_millis(ms)
            }

            fn pkt(&mut self, ms: u64, marker: bool, payload: &[u8]) {
                let d = datagram(self.seq, self.ts, marker, payload);
                self.seq = self.seq.wrapping_add(1);
                self.eng.on_datagram(self.at(ms), PORT, &d);
            }

            /// The SPS as the device sends it, and as it goes out (with a bitstream restriction).
            fn sps() -> Vec<u8> {
                AvcSpsSpec::baseline_vga().nal()
            }

            fn sent_sps() -> Vec<u8> {
                match crate::avc::add_bitstream_restriction(&Self::sps()).unwrap() {
                    crate::avc::SpsRewrite::Patched(p) => p,
                    other => panic!("{other:?}"),
                }
            }

            /// SPS and PPS in a STAP-A, then the IDR in three fragments.
            fn idr(&mut self, ms: u64, with_sets: bool) {
                if with_sets {
                    self.pkt(ms, false, &stap_a(&[&Self::sps(), &PPS264]));
                }
                let frags = fu_a(&idr264(&[7; 30]), 12);
                let n = frags.len();
                for (i, f) in frags.iter().enumerate() {
                    self.pkt(ms, i == n - 1, f);
                }
                self.ts += 3000;
            }

            fn delta(&mut self, ms: u64) {
                self.pkt(ms, true, &slice(&[1, 2, 3, 4]));
                self.ts += 3000;
            }
        }

        #[test]
        fn an_h264_stream_gets_an_avc1_config_and_the_key_and_params_flags() {
            let mut r = H264Rig::new();
            r.idr(0, true);
            r.delta(10);
            let f = r.q.drain();
            assert_eq!(f.len(), 3);
            let c = json(&f[0]);
            assert_eq!(c["event"], "config");
            assert_eq!(c["codec"], "avc1.42C01E");
            assert_eq!((c["width"].as_u64(), c["height"].as_u64()), (Some(640), Some(480)));
            assert_eq!((c["coded_width"].as_u64(), c["coded_height"].as_u64()), (Some(640), Some(480)));
            assert_eq!(f[1].flags, FLAG_KEY | FLAG_PARAMS);
            let mut want = Vec::new();
            for nal in [&H264Rig::sent_sps()[..], &PPS264[..]] {
                want.extend_from_slice(&[0, 0, 0, 1]);
                want.extend_from_slice(nal);
            }
            assert!(f[1].payload.starts_with(&want));
            assert_eq!(f[2].flags, 0);
            let s = r.eng.stats();
            assert_eq!((s.access_units, s.keyframes, s.dropped_units), (2, 1, 0));
        }

        /// A non-IDR slice with the given slice_type in its header.
        fn typed_slice(slice_type: u32) -> Vec<u8> {
            let mut w = crate::sps::testutil::BitWriter::default();
            w.ue(0);
            w.ue(slice_type);
            w.ue(0);
            for _ in 0..8 {
                w.u(8, 0x5A);
            }
            let mut n = vec![0x41];
            n.extend_from_slice(&crate::sps::testutil::escape_rbsp(&w.finish()));
            n
        }

        #[test]
        fn b_slices_are_counted_in_the_stats_and_the_stream_goes_on_unchanged() {
            let mut r = H264Rig::new();
            r.idr(0, true);
            let b = typed_slice(1);
            let p = typed_slice(0);
            r.pkt(10, true, &p);
            r.ts += 3000;
            assert_eq!(r.eng.stats().b_slices, 0);
            r.pkt(20, true, &b);
            r.ts += 3000;
            r.pkt(30, true, &typed_slice(6));
            r.ts += 3000;
            r.pkt(40, true, &p);
            let f = r.q.drain();
            let s = r.eng.stats();
            assert_eq!(s.b_slices, 2);
            // Every picture was written, in order, with the slice as it came.
            assert_eq!((s.access_units, s.keyframes, s.dropped_units), (5, 1, 0));
            let units: Vec<_> = f.iter().filter(|f| !f.is_control_frame()).collect();
            assert_eq!(units.len(), 5);
            assert_eq!(units[2].payload, [&[0u8, 0, 0, 1][..], &b[..]].concat());
            assert!(units.windows(2).all(|w| w[0].pts_us <= w[1].pts_us));
            // And the stats line carries the count.
            let line = crate::frame::Frame::control(&crate::frame::Event::Stats(s));
            assert!(String::from_utf8_lossy(&line.payload).contains("\"b_slices\":2"));
        }

        #[test]
        fn the_visible_size_of_an_h264_stream_is_its_cropped_size_and_the_coded_size_its_macroblocks() {
            let mut r = H264Rig::new();
            let sps = AvcSpsSpec::high_1080p().nal();
            r.pkt(0, false, &stap_a(&[&sps, &PPS264]));
            r.pkt(0, true, &idr264(&[1; 4]));
            let c = json(&r.q.drain()[0]);
            assert_eq!(c["codec"], "avc1.640028");
            assert_eq!((c["width"].as_u64(), c["height"].as_u64()), (Some(1920), Some(1080)));
            assert_eq!((c["coded_width"].as_u64(), c["coded_height"].as_u64()), (Some(1920), Some(1088)));
        }

        #[test]
        fn the_sets_of_the_configuration_packet_go_in_front_of_an_idr_that_has_none() {
            let mut r = H264Rig::new();
            let avcc = make_box(b"avcC", &avcc_record(66, 0xC0, 30, &[&H264Rig::sps()], &[&PPS264]));
            let mut config = vec![0x92, 0xE6, 0xC0, 0xA3];
            config.extend_from_slice(&avc1_box(640, 480, &[avcc]));
            r.pkt(0, true, &config);
            r.ts += 3000;
            r.idr(10, false);
            let f = r.q.drain();
            assert_eq!(f.len(), 2, "config event, key unit; the config packet makes no picture: {f:?}");
            assert_eq!(json(&f[0])["codec"], "avc1.42C01E");
            assert_eq!(f[1].flags, FLAG_KEY | FLAG_PARAMS);
            assert!(f[1].payload.starts_with(&[0, 0, 0, 1, 0x67]));
        }

        #[test]
        fn loss_in_an_h264_stream_withholds_deltas_asks_for_a_keyframe_and_resumes_at_the_idr() {
            let mut r = H264Rig::new();
            r.idr(0, true);
            r.delta(10);
            r.q.drain();
            r.eng.take_rtcp();
            // A unit with a lost fragment.
            let frags = fu_a(&slice(&[0; 40]), 14);
            let seq = r.seq;
            let d = datagram(seq, r.ts, false, &frags[0]);
            r.eng.on_datagram(r.at(20), PORT, &d);
            let d = datagram(seq + 2, r.ts, true, &frags[2]);
            r.eng.on_datagram(r.at(20), PORT, &d);
            r.seq = seq + 3;
            r.ts += 3000;
            assert!(r.q.drain().is_empty());
            let s = r.eng.stats();
            assert_eq!((s.lost, s.dropped_units, s.plis), (1, 1, 1));
            let sent = r.eng.take_rtcp();
            assert_eq!(sent.len(), 1);
            r.delta(30);
            assert!(r.q.drain().is_empty(), "a delta without its reference is withheld");
            r.idr(50, false);
            r.delta(60);
            let f = r.q.drain();
            assert_eq!(f.iter().map(|f| f.flags).collect::<Vec<_>>(), vec![FLAG_KEY | FLAG_PARAMS, 0]);
        }

        #[test]
        fn payload_types_of_the_other_codec_are_ignored() {
            let mut r = H264Rig::new();
            let mut d = rtp_bytes(1, 1, true, DEV, &slice(&[1; 4]));
            d[1] = 100 | 0x80; // an HEVC payload type
            r.eng.on_datagram(r.at(1), PORT, &d);
            assert_eq!(r.eng.stats().packets, 0);
        }
    }

    #[test]
    fn units_keep_flowing_through_a_long_run_without_growing_state() {
        // 20 000 pictures (and the loss of every 50th unit's middle packet) through the engine.
        let mut r = Rig::new();
        r.idr(0, true);
        r.frames();
        let mut sent_keys = 0;
        for i in 1..20_000u64 {
            let ms = i * 22;
            if i % 50 == 0 {
                // lose a packet inside this unit
                let frags = fu(&trail(&[3; 60]), 20);
                let seq = r.seq;
                r.pkt_seq(ms, seq, false, &frags[0]);
                r.seq = seq.wrapping_add(2);
                r.pkt(ms, true, &frags[2]);
                r.ts += 500;
            } else if i % 53 == 0 {
                r.idr(ms, false);
                sent_keys += 1;
            } else {
                r.delta(ms);
            }
            r.eng.on_tick(r.at(ms));
            r.eng.take_rtcp();
            r.q.drain();
        }
        assert!(sent_keys > 100);
        let s = r.eng.stats();
        assert_eq!(s.lost, 399);
        assert!(s.access_units > 5_000);
    }
}
