// SPDX-License-Identifier: GPL-3.0-or-later
//! Bounded stderr diagnostics: a token bucket (burst 20, then one line per
//! 6 s) shared by every event; suppressed lines are counted and the count
//! is attached to the next line that gets through.

use std::time::{Duration, Instant};

pub const BURST: u32 = 20;
pub const REFILL: Duration = Duration::from_secs(6);
const MAX_LINE_CHARS: usize = 512;

#[derive(Debug)]
pub struct LogLimiter {
    tokens: u32,
    last_refill: Instant,
    suppressed: u64,
    /// When false, nothing is written (tests).
    enabled: bool,
}

impl LogLimiter {
    pub fn new(now: Instant) -> Self {
        Self {
            tokens: BURST,
            last_refill: now,
            suppressed: 0,
            enabled: true,
        }
    }

    pub fn silent(now: Instant) -> Self {
        Self {
            enabled: false,
            ..Self::new(now)
        }
    }

    /// Returns the line that was (or would be) written, `None` when
    /// suppressed. Lines are `event=led.<event> <detail>`.
    pub fn log(&mut self, now: Instant, event: &str, detail: &str) -> Option<String> {
        while self.tokens < BURST && now.saturating_duration_since(self.last_refill) >= REFILL {
            self.tokens += 1;
            self.last_refill += REFILL;
        }
        if self.tokens == BURST {
            self.last_refill = now;
        }
        if self.tokens == 0 {
            self.suppressed = self.suppressed.saturating_add(1);
            return None;
        }
        self.tokens -= 1;
        let mut line = format!("event=led.{event} {detail}");
        if self.suppressed > 0 {
            line.push_str(&format!(" suppressed={}", self.suppressed));
            self.suppressed = 0;
        }
        let line: String = line.chars().take(MAX_LINE_CHARS).collect();
        if self.enabled {
            eprintln!("{line}");
        }
        Some(line)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bursts_then_refills_and_reports_suppressed() {
        let t0 = Instant::now();
        let mut l = LogLimiter::silent(t0);
        for _ in 0..BURST {
            assert!(l.log(t0, "x", "a").is_some());
        }
        assert!(l.log(t0, "x", "a").is_none());
        assert!(l.log(t0, "x", "a").is_none());
        assert!(l.log(t0 + REFILL / 2, "x", "a").is_none());
        let line = l.log(t0 + REFILL, "x", "b").unwrap();
        assert!(line.ends_with("suppressed=3"), "{line}");
        assert!(l.log(t0 + REFILL, "x", "a").is_none());
        // A long quiet period refills only up to the burst.
        let later = t0 + REFILL * 1000;
        for _ in 0..BURST {
            assert!(l.log(later, "x", "a").is_some());
        }
        assert!(l.log(later, "x", "a").is_none());
        let long = "y".repeat(5000);
        let mut l = LogLimiter::silent(t0);
        assert!(l.log(t0, "x", &long).unwrap().chars().count() <= MAX_LINE_CHARS);
    }
}
