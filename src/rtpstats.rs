//! Receiver-side RTP statistics for the RTCP report block (RFC 3550 appendix A): extended highest
//! sequence number, cumulative and interval loss, interarrival jitter, and the sender-report
//! timing (LSR, DLSR) the report block echoes.

use std::time::Instant;

use idevice::core_device::ReportBlock;

/// RTCP sender report packet type.
const PT_SR: u8 = 200;

/// What a sender report says about the sender's clocks (RFC 3550 6.4.1).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SenderReportInfo {
    pub ssrc: u32,
    /// The NTP wallclock time of the report, in seconds (the sender's clock).
    pub ntp_secs: f64,
    /// The middle 32 bits of the NTP time, as echoed in report blocks (LSR).
    pub ntp_middle: u32,
    /// The RTP timestamp that corresponds to the NTP time.
    pub rtp_timestamp: u32,
}

/// The first sender report in a (compound) RTCP datagram.
#[must_use]
pub fn parse_sender_report(buf: &[u8]) -> Option<SenderReportInfo> {
    let mut off = 0;
    while off + 4 <= buf.len() {
        let words = usize::from(u16::from_be_bytes([buf[off + 2], buf[off + 3]]));
        let len = (words + 1) * 4;
        if off + len > buf.len() {
            return None;
        }
        if buf[off + 1] == PT_SR && len >= 20 {
            let w = |i: usize| u32::from_be_bytes(buf[off + i..off + i + 4].try_into().expect("4 bytes"));
            let (ssrc, msw, lsw, rtp_timestamp) = (w(4), w(8), w(12), w(16));
            return Some(SenderReportInfo {
                ssrc,
                ntp_secs: f64::from(msw) + f64::from(lsw) / 4_294_967_296.0,
                ntp_middle: (msw << 16) | (lsw >> 16),
                rtp_timestamp,
            });
        }
        off += len;
    }
    None
}

const SEQ_MOD: u32 = 1 << 16;
/// A forward jump larger than this is a restart, not a loss (RFC 3550 A.1).
const MAX_DROPOUT: u16 = 3000;
/// A step back smaller than this is a reordered or duplicate packet.
const MAX_MISORDER: u16 = 100;

#[derive(Debug)]
pub struct RtpStats {
    clock_hz: u32,
    started: bool,
    base_seq: u32,
    max_seq: u16,
    cycles: u32,
    bad_seq: u32,
    received: u32,
    expected_prior: u32,
    received_prior: u32,
    /// Interarrival jitter in RTP timestamp units.
    jitter: f64,
    epoch: Option<Instant>,
    prev: Option<(f64, u32)>,        // (arrival in timestamp units, RTP timestamp)
    last_sr: Option<(u32, Instant)>, // (middle 32 bits of the NTP time, arrival)
}

impl RtpStats {
    #[must_use]
    pub fn new(clock_hz: u32) -> Self {
        RtpStats {
            clock_hz: clock_hz.max(1),
            started: false,
            base_seq: 0,
            max_seq: 0,
            cycles: 0,
            bad_seq: SEQ_MOD + 1,
            received: 0,
            expected_prior: 0,
            received_prior: 0,
            jitter: 0.0,
            epoch: None,
            prev: None,
            last_sr: None,
        }
    }

    fn init_seq(&mut self, seq: u16) {
        self.base_seq = u32::from(seq);
        self.max_seq = seq;
        self.bad_seq = SEQ_MOD + 1;
        self.cycles = 0;
        self.received = 0;
        self.received_prior = 0;
        self.expected_prior = 0;
    }

    /// Accounts for one received RTP packet (every one, in or out of order).
    pub fn on_packet(&mut self, now: Instant, seq: u16, timestamp: u32) {
        if !self.started {
            self.started = true;
            self.init_seq(seq);
        } else {
            let udelta = seq.wrapping_sub(self.max_seq);
            if udelta < MAX_DROPOUT {
                if seq < self.max_seq {
                    self.cycles = self.cycles.wrapping_add(SEQ_MOD); // wrapped
                }
                self.max_seq = seq;
            } else if u32::from(udelta) <= SEQ_MOD - u32::from(MAX_MISORDER) {
                // A big jump: believe it if the next packet follows it (the source restarted).
                if u32::from(seq) == self.bad_seq {
                    self.init_seq(seq);
                } else {
                    self.bad_seq = (u32::from(seq) + 1) & (SEQ_MOD - 1);
                    return;
                }
            }
            // else: a duplicate or reordered packet: counted, the highest sequence stays.
        }
        self.received = self.received.wrapping_add(1);

        // Interarrival jitter (A.8), in timestamp units, from the arrival times.
        let epoch = *self.epoch.get_or_insert(now);
        let arrival = now.saturating_duration_since(epoch).as_secs_f64() * f64::from(self.clock_hz);
        if let Some((prev_arrival, prev_ts)) = self.prev {
            let ts_delta = f64::from(timestamp.wrapping_sub(prev_ts).cast_signed());
            let d = (arrival - prev_arrival) - ts_delta;
            self.jitter += (d.abs() - self.jitter) / 16.0;
        }
        self.prev = Some((arrival, timestamp));
    }

    /// Notes a sender report's NTP time (its middle 32 bits) for the next report block's LSR/DLSR.
    pub fn on_sender_report(&mut self, now: Instant, ntp_middle: u32) {
        self.last_sr = Some((ntp_middle, now));
    }

    /// A new RTP clock rate (the jitter is in its units).
    pub fn set_clock_hz(&mut self, clock_hz: u32) {
        self.clock_hz = clock_hz.max(1);
        self.prev = None;
    }

    /// The highest sequence number received, counted from the first one (low 16 bits).
    #[expect(clippy::cast_possible_truncation, reason = "the low 16 bits are what is asked for")]
    #[must_use]
    pub fn highest_since_base(&self) -> u16 {
        self.extended_highest().wrapping_sub(self.base_seq) as u16
    }

    #[must_use]
    pub fn received(&self) -> u32 {
        self.received
    }

    /// Extended highest sequence number received (cycles in the high 16 bits).
    #[must_use]
    pub fn extended_highest(&self) -> u32 {
        self.cycles.wrapping_add(u32::from(self.max_seq))
    }

    fn expected(&self) -> u32 {
        self.extended_highest().wrapping_sub(self.base_seq).wrapping_add(1)
    }

    /// Cumulative packets lost, clamped to the report's 24 bits (a negative count, from duplicates, is 0).
    #[must_use]
    pub fn cumulative_lost(&self) -> u32 {
        let lost = i64::from(self.expected()) - i64::from(self.received);
        u32::try_from(lost.clamp(0, 0x00FF_FFFF)).unwrap_or(0) // in range: clamped
    }

    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "the jitter is not negative, and a float-to-int `as` saturates"
    )]
    #[must_use]
    pub fn jitter(&self) -> u32 {
        self.jitter as u32
    }

    /// The report block about `source_ssrc`, closing the loss interval (the next call's
    /// `fraction_lost` covers the packets since this one).
    pub fn report_block(&mut self, now: Instant, source_ssrc: u32) -> ReportBlock {
        let expected = self.expected();
        let expected_interval = expected.wrapping_sub(self.expected_prior);
        self.expected_prior = expected;
        let received_interval = self.received.wrapping_sub(self.received_prior);
        self.received_prior = self.received;
        let lost_interval = i64::from(expected_interval) - i64::from(received_interval);
        let fraction_lost = if expected_interval == 0 || lost_interval <= 0 {
            0
        } else {
            u8::try_from(((lost_interval << 8) / i64::from(expected_interval)).min(255)).unwrap_or(u8::MAX)
        };
        let (lsr, dlsr) = match self.last_sr {
            Some((ntp_middle, at)) => {
                let secs = now.saturating_duration_since(at).as_secs_f64();
                #[expect(
                    clippy::cast_possible_truncation,
                    clippy::cast_sign_loss,
                    reason = "seconds are not negative, and a float-to-int `as` saturates"
                )]
                let dlsr = (secs * 65536.0) as u32;
                (ntp_middle, dlsr)
            }
            None => (0, 0),
        };
        ReportBlock {
            source_ssrc,
            fraction_lost,
            cumulative_lost: self.cumulative_lost(),
            highest_seq: self.extended_highest(),
            jitter: self.jitter(),
            lsr,
            dlsr,
        }
    }
}

#[cfg(test)]
mod tests {
    #![expect(
        clippy::cast_possible_truncation,
        reason = "the tests number packets, bytes and times with small loop counters, so a cast does not truncate or lose precision"
    )]
    use std::time::Duration;

    use super::*;

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    #[test]
    #[expect(clippy::float_cmp, reason = "0x11112222 + 0.5 is exact in a double, and the parse must give exactly that")]
    fn sender_reports_are_parsed_from_compound_packets() {
        // An RR first, then the SR: SSRC, NTP 0x11112222.80000000 (= 286331682.5 s), RTP ts 7777.
        let mut b = vec![0x80, 201, 0, 1, 0, 0, 0, 9];
        b.extend_from_slice(&[0x80, 200, 0, 6]);
        b.extend_from_slice(&0xAABB_CCDDu32.to_be_bytes());
        b.extend_from_slice(&0x1111_2222u32.to_be_bytes());
        b.extend_from_slice(&0x8000_0000u32.to_be_bytes());
        b.extend_from_slice(&7777u32.to_be_bytes());
        b.extend_from_slice(&[0; 8]);
        let sr = parse_sender_report(&b).unwrap();
        assert_eq!(sr.ssrc, 0xAABB_CCDD);
        assert_eq!(sr.ntp_secs, f64::from(0x1111_2222u32) + 0.5);
        assert_eq!(sr.ntp_middle, 0x2222_8000);
        assert_eq!(sr.rtp_timestamp, 7777);
        // Truncated, or no SR at all.
        assert_eq!(parse_sender_report(&b[..20]), None);
        assert_eq!(parse_sender_report(&b[..8]), None);
        assert_eq!(parse_sender_report(&[]), None);
        assert_eq!(parse_sender_report(&[0x80, 200, 0, 0]), None);
    }

    #[test]
    fn highest_sequence_counts_cycles() {
        let t0 = Instant::now();
        let mut s = RtpStats::new(24_000);
        for (i, seq) in [65533u16, 65534, 65535, 0, 1, 2].into_iter().enumerate() {
            s.on_packet(at(t0, i as u64), seq, 0);
        }
        assert_eq!(s.extended_highest(), 0x0001_0002);
        assert_eq!(s.received(), 6);
        assert_eq!(s.cumulative_lost(), 0);
    }

    #[test]
    fn loss_is_expected_minus_received_and_the_fraction_covers_the_interval() {
        let t0 = Instant::now();
        let mut s = RtpStats::new(24_000);
        // 100 packets with 10 missing (every tenth).
        for seq in 0..100u16 {
            if seq % 10 != 5 {
                s.on_packet(at(t0, u64::from(seq)), seq, u32::from(seq) * 100);
            }
        }
        let b = s.report_block(at(t0, 100), 0xABCD);
        assert_eq!(b.source_ssrc, 0xABCD);
        assert_eq!(b.highest_seq, 99);
        assert_eq!(b.cumulative_lost, 10);
        // 10 of the 100 expected so far.
        assert_eq!(b.fraction_lost, ((10u32 << 8) / 100) as u8);

        // The next interval, with no loss, reports a fraction of 0 and the same cumulative count.
        for seq in 100..150u16 {
            s.on_packet(at(t0, 200 + u64::from(seq)), seq, u32::from(seq) * 100);
        }
        let b = s.report_block(at(t0, 400), 0xABCD);
        assert_eq!(b.fraction_lost, 0);
        assert_eq!(b.cumulative_lost, 10);
        assert_eq!(b.highest_seq, 149);
    }

    #[test]
    fn duplicates_do_not_make_a_negative_loss() {
        let t0 = Instant::now();
        let mut s = RtpStats::new(24_000);
        for seq in [1u16, 2, 3, 3, 3, 4] {
            s.on_packet(at(t0, 1), seq, 0);
        }
        assert_eq!(s.cumulative_lost(), 0);
        let b = s.report_block(at(t0, 2), 1);
        assert_eq!(b.fraction_lost, 0);
    }

    #[test]
    fn a_restart_with_a_new_base_is_adopted_on_the_second_packet() {
        let t0 = Instant::now();
        let mut s = RtpStats::new(24_000);
        for seq in 0..10u16 {
            s.on_packet(at(t0, u64::from(seq)), seq, 0);
        }
        // A jump of 20000: not believed at first (and not counted) ...
        s.on_packet(at(t0, 20), 20_000, 0);
        assert_eq!(s.extended_highest(), 9);
        assert_eq!(s.received(), 10);
        // ... but believed when the next packet follows it.
        s.on_packet(at(t0, 21), 20_001, 0);
        assert_eq!(s.extended_highest(), 20_001);
        assert_eq!(s.cumulative_lost(), 0);
    }

    #[test]
    fn steady_arrivals_have_no_jitter_and_uneven_ones_do() {
        let t0 = Instant::now();
        // 24 kHz clock, a picture every 1000/48 ms is 500 ticks: arrivals exactly on schedule.
        let mut s = RtpStats::new(24_000);
        for i in 0..50u64 {
            let ts = (i * 500) as u32;
            s.on_packet(t0 + Duration::from_micros(i * 500 * 1_000_000 / 24_000), i as u16, ts);
        }
        assert!(s.jitter() <= 1, "jitter {}", s.jitter());

        // Every other picture arrives 10 ms (240 ticks) late.
        let mut s = RtpStats::new(24_000);
        for i in 0..200u64 {
            let ts = (i * 500) as u32;
            let late = if i % 2 == 0 { 0 } else { 10_000 };
            s.on_packet(t0 + Duration::from_micros(i * 500 * 1_000_000 / 24_000 + late), i as u16, ts);
        }
        // |D| alternates around 240 ticks; the filtered value converges near 240.
        assert!((200..=250).contains(&s.jitter()), "jitter {}", s.jitter());
    }

    #[test]
    fn timestamp_wrap_does_not_disturb_jitter() {
        let t0 = Instant::now();
        let mut s = RtpStats::new(24_000);
        let start = u32::MAX - 1000;
        for i in 0..40u64 {
            s.on_packet(
                t0 + Duration::from_micros(i * 500 * 1_000_000 / 24_000),
                i as u16,
                start.wrapping_add((i * 500) as u32),
            );
        }
        assert!(s.jitter() <= 1, "jitter {}", s.jitter());
    }

    #[test]
    fn lsr_and_dlsr_echo_the_last_sender_report() {
        let t0 = Instant::now();
        let mut s = RtpStats::new(24_000);
        s.on_packet(t0, 1, 0);
        // No sender report yet: zeros.
        let b = s.report_block(at(t0, 10), 7);
        assert_eq!((b.lsr, b.dlsr), (0, 0));
        s.on_sender_report(at(t0, 100), 0x1234_5678);
        // 500 ms later: DLSR is in 1/65536 s units.
        let b = s.report_block(at(t0, 600), 7);
        assert_eq!(b.lsr, 0x1234_5678);
        assert_eq!(b.dlsr, 32768);
    }

    #[test]
    fn cumulative_loss_is_clamped_to_24_bits() {
        let t0 = Instant::now();
        let mut s = RtpStats::new(24_000);
        s.on_packet(t0, 0, 0);
        // Walk the highest sequence far ahead in steps below the dropout limit with nothing received between.
        let mut seq = 0u16;
        for _ in 0..(1 << 13) {
            seq = seq.wrapping_add(2999);
            s.on_packet(t0, seq, 0);
        }
        assert_eq!(s.cumulative_lost(), 0x00FF_FFFF);
    }
}
