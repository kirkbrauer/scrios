//! The keyframe request limiter: at most one request on the wire per window, and a request made
//! inside the window is remembered and sent when the window ends.
//!
//! Forcing keyframes more often than about one in two seconds can stall the device's encoder, so
//! the sidecar never sends two requests closer than `min_interval`, whoever asks (the `keyframe`
//! command, a lost packet, a full output queue).

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Send the request now.
    SendNow,
    /// Inside the window: it will be released by [`KeyframeLimiter::poll`] when the window ends.
    Coalesced,
}

#[derive(Debug)]
pub struct KeyframeLimiter {
    min_interval: Duration,
    last_sent: Option<Instant>,
    pending: bool,
}

impl KeyframeLimiter {
    pub fn new(min_interval: Duration) -> Self {
        KeyframeLimiter { min_interval, last_sent: None, pending: false }
    }

    fn window_open(&self, now: Instant) -> bool {
        match self.last_sent {
            None => true,
            Some(t) => now.saturating_duration_since(t) >= self.min_interval,
        }
    }

    /// A keyframe is wanted now.
    pub fn request(&mut self, now: Instant) -> Decision {
        if self.window_open(now) {
            self.last_sent = Some(now);
            self.pending = false;
            Decision::SendNow
        } else {
            self.pending = true;
            Decision::Coalesced
        }
    }

    /// Call when the clock passes [`deadline`](Self::deadline): true if a coalesced request is due
    /// and must be sent now.
    pub fn poll(&mut self, now: Instant) -> bool {
        if self.pending && self.window_open(now) {
            self.pending = false;
            self.last_sent = Some(now);
            true
        } else {
            false
        }
    }

    /// When a coalesced request becomes due (None if nothing is waiting).
    pub fn deadline(&self) -> Option<Instant> {
        if !self.pending {
            return None;
        }
        self.last_sent.map(|t| t + self.min_interval)
    }

    /// Drops a held request: a keyframe has arrived, so it is no longer wanted.
    pub fn cancel_pending(&mut self) {
        self.pending = false;
    }

    #[cfg(test)]
    pub fn is_pending(&self) -> bool {
        self.pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(t0: Instant, n: u64) -> Instant {
        t0 + Duration::from_millis(n)
    }

    #[test]
    fn the_first_request_goes_out_at_once() {
        let t0 = Instant::now();
        let mut k = KeyframeLimiter::new(Duration::from_secs(2));
        assert_eq!(k.request(t0), Decision::SendNow);
        assert_eq!(k.deadline(), None);
    }

    #[test]
    fn a_request_inside_the_window_is_coalesced_and_released_when_it_ends() {
        let t0 = Instant::now();
        let mut k = KeyframeLimiter::new(Duration::from_secs(2));
        assert_eq!(k.request(t0), Decision::SendNow);
        assert_eq!(k.request(ms(t0, 100)), Decision::Coalesced);
        assert_eq!(k.deadline(), Some(ms(t0, 2000)));
        // Nothing is released early.
        assert!(!k.poll(ms(t0, 1999)));
        // And exactly once at the end of the window.
        assert!(k.poll(ms(t0, 2000)));
        assert!(!k.poll(ms(t0, 2001)));
        assert_eq!(k.deadline(), None);
    }

    #[test]
    fn many_requests_in_one_window_become_one() {
        let t0 = Instant::now();
        let mut k = KeyframeLimiter::new(Duration::from_secs(2));
        assert_eq!(k.request(t0), Decision::SendNow);
        for i in 1..50 {
            assert_eq!(k.request(ms(t0, i * 10)), Decision::Coalesced);
        }
        let mut sent = 0;
        for t in (0..6000).step_by(50) {
            if k.poll(ms(t0, t)) {
                sent += 1;
            }
        }
        assert_eq!(sent, 1);
    }

    #[test]
    fn the_released_request_starts_a_new_window() {
        let t0 = Instant::now();
        let mut k = KeyframeLimiter::new(Duration::from_secs(2));
        k.request(t0);
        k.request(ms(t0, 10));
        assert!(k.poll(ms(t0, 2000)));
        // A request right after the released one waits for the next window, measured from the release.
        assert_eq!(k.request(ms(t0, 2100)), Decision::Coalesced);
        assert_eq!(k.deadline(), Some(ms(t0, 4000)));
        assert!(!k.poll(ms(t0, 3999)));
        assert!(k.poll(ms(t0, 4000)));
    }

    #[test]
    fn a_request_after_a_quiet_window_is_sent_at_once() {
        let t0 = Instant::now();
        let mut k = KeyframeLimiter::new(Duration::from_secs(2));
        k.request(t0);
        assert_eq!(k.request(ms(t0, 2000)), Decision::SendNow);
        assert_eq!(k.request(ms(t0, 9000)), Decision::SendNow);
    }

    #[test]
    fn never_two_sends_closer_than_the_interval() {
        // Hammer it with requests and polls at 7 ms steps and check the gaps between sends.
        let t0 = Instant::now();
        let mut k = KeyframeLimiter::new(Duration::from_secs(2));
        let mut sends = Vec::new();
        for step in 0..3000u64 {
            let now = ms(t0, step * 7);
            if step % 3 == 0 && k.request(now) == Decision::SendNow {
                sends.push(now);
            }
            if k.poll(now) {
                sends.push(now);
            }
        }
        assert!(sends.len() >= 9);
        for w in sends.windows(2) {
            assert!(w[1] - w[0] >= Duration::from_secs(2));
        }
    }

    #[test]
    fn cancelling_drops_the_held_request_but_not_the_window() {
        let t0 = Instant::now();
        let mut k = KeyframeLimiter::new(Duration::from_secs(2));
        k.request(t0);
        k.request(ms(t0, 5));
        assert!(k.is_pending());
        k.cancel_pending();
        assert!(!k.is_pending());
        assert_eq!(k.deadline(), None);
        assert!(!k.poll(ms(t0, 3000)));
        // The window since the last send still applies to a new request.
        let mut k = KeyframeLimiter::new(Duration::from_secs(2));
        k.request(t0);
        k.cancel_pending();
        assert_eq!(k.request(ms(t0, 1000)), Decision::Coalesced);
    }

    #[test]
    fn a_zero_interval_never_coalesces() {
        let t0 = Instant::now();
        let mut k = KeyframeLimiter::new(Duration::ZERO);
        assert_eq!(k.request(t0), Decision::SendNow);
        assert_eq!(k.request(t0), Decision::SendNow);
        assert!(!k.is_pending());
    }
}
