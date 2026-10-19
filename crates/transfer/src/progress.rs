//! Progress, speed, and ETA estimation — spec §9.3, NFR-PERF-007.
//!
//! The UI must not flicker between 37 MB/s and 12 MB/s on Wi-Fi, and must
//! not show an ETA from a single sample. This module keeps a short window
//! of (time, cumulative bytes) samples and derives:
//!
//! * **speed** — bytes/second over the window, smoothed with an
//!   exponentially-weighted moving average (EWMA, α = 0.35),
//! * **ETA** — only once we have ≥ 2 samples spanning ≥ 1 s *and* a
//!   non-zero speed (spec: "ETA may be omitted when too little data
//!   exists to estimate it reliably"),
//! * **fraction** — honest progress: the receiver's *written* bytes over
//!   the offered total (spec §9.3: sender-read ≠ receiver-written).
//!
//! All units are **bytes per second internally**; formatting to MB/s or
//! MiB/s happens at the display edge (spec NFR-PERF-006, §13.2).

use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// How much history we keep for speed estimation.
const WINDOW: Duration = Duration::from_secs(3);
/// Minimum span before we trust a speed estimate at all.
const MIN_SPAN: Duration = Duration::from_millis(750);
/// EWMA smoothing factor: lower = steadier, higher = more responsive.
const EWMA_ALPHA: f64 = 0.35;

/// Rolling speed/ETA estimator for one transfer.
#[derive(Debug)]
pub struct ProgressMeter {
    total_bytes: u64,
    /// Last confirmed byte count (receiver-written or sender-confirmed).
    bytes: u64,
    samples: VecDeque<(Instant, u64)>,
    /// Smoothed bytes/second.
    speed_bps: f64,
}

impl ProgressMeter {
    pub fn new(total_bytes: u64) -> Self {
        Self {
            total_bytes,
            bytes: 0,
            samples: VecDeque::new(),
            speed_bps: 0.0,
        }
    }

    /// Record the current cumulative byte count at "now".
    pub fn record(&mut self, cumulative_bytes: u64) {
        // Never go backwards (a reset/restart would poison the average).
        self.bytes = self.bytes.max(cumulative_bytes);
        let now = Instant::now();
        self.samples.push_back((now, self.bytes));
        // Drop samples older than the window; always keep at least two so
        // the next update can compute a rate immediately.
        while self.samples.len() > 2 {
            let (t, _) = *self.samples.front().unwrap();
            if now.duration_since(t) > WINDOW {
                self.samples.pop_front();
            } else {
                break;
            }
        }
        self.recompute_speed();
    }

    fn recompute_speed(&mut self) {
        let (first, last) = match (self.samples.front(), self.samples.back()) {
            (Some(&first), Some(&last)) if self.samples.len() >= 2 => (first, last),
            _ => return,
        };
        let span = last.0.duration_since(first.0);
        if span < MIN_SPAN {
            return; // too little data: keep the previous smoothing
        }
        let delta_bytes = last.1.saturating_sub(first.1) as f64;
        let instant_speed = delta_bytes / span.as_secs_f64();
        // First meaningful reading seeds the EWMA directly.
        if self.speed_bps == 0.0 {
            self.speed_bps = instant_speed;
        } else {
            self.speed_bps = EWMA_ALPHA * instant_speed + (1.0 - EWMA_ALPHA) * self.speed_bps;
        }
    }

    /// Bytes recorded so far.
    pub fn bytes(&self) -> u64 {
        self.bytes
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    /// 0.0 – 1.0 (empty totals report 1.0: nothing left to do).
    pub fn fraction(&self) -> f64 {
        if self.total_bytes == 0 {
            return 1.0;
        }
        (self.bytes as f64 / self.total_bytes as f64).clamp(0.0, 1.0)
    }

    /// Smoothed bytes per second; `0.0` until we have enough samples.
    pub fn speed_bps(&self) -> f64 {
        self.speed_bps
    }

    /// Remaining seconds, or `None` when it cannot be estimated
    /// reliably (spec NFR-PERF-007).
    pub fn eta_seconds(&self) -> Option<u64> {
        if self.speed_bps <= 1.0 || self.bytes >= self.total_bytes {
            return None;
        }
        let remaining = (self.total_bytes - self.bytes) as f64;
        Some((remaining / self.speed_bps).round() as u64)
    }

    /// True once every offered byte has been accounted for.
    pub fn is_complete(&self) -> bool {
        self.bytes >= self.total_bytes
    }
}

/// Format a byte count using **IEC binary units** with an explicit
/// `i` prefix (spec NFR-UX-006: consistent, clearly labeled units).
///
/// `format_bytes(1536) == "1.5 KiB"`.
pub fn format_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KiB", "MiB", "GiB", "TiB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else if value >= 100.0 {
        format!("{value:.0} {}", UNITS[unit])
    } else if value >= 10.0 {
        format!("{value:.1} {}", UNITS[unit])
    } else {
        format!("{value:.2} {}", UNITS[unit])
    }
}

/// Format a speed as MiB/s — labeled explicitly so nobody confuses it
/// with megabits (spec §13.2).
pub fn format_speed(bytes_per_second: f64) -> String {
    if bytes_per_second <= 0.0 {
        return "—".to_string();
    }
    format!("{}/s", format_bytes(bytes_per_second as u64))
}

/// Format seconds as a human ETA (`45s`, `3m 12s`, `1h 04m`).
pub fn format_eta(seconds: u64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else {
        format!("{}h {:02}m", seconds / 3600, (seconds % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed samples with explicit timestamps by driving the internals —
    /// we can't reliably sleep in unit tests, so we verify the math via
    /// `record` + real (tiny) time gaps where needed, and pure helpers
    /// separately.
    #[test]
    fn fraction_and_completion() {
        let mut m = ProgressMeter::new(1000);
        assert!((m.fraction() - 0.0).abs() < f64::EPSILON);
        m.record(500);
        assert!((m.fraction() - 0.5).abs() < 1e-9);
        m.record(1000);
        assert!(m.is_complete());

        // Zero-total transfers read as complete (empty file).
        let empty = ProgressMeter::new(0);
        assert!((empty.fraction() - 1.0).abs() < f64::EPSILON);
        assert!(empty.is_complete());
    }

    /// Byte counts never regress even if the caller reports less.
    #[test]
    fn bytes_never_go_backwards() {
        let mut m = ProgressMeter::new(100);
        m.record(60);
        m.record(30); // out-of-order report must not undo progress
        assert_eq!(m.bytes(), 60);
    }

    /// No ETA before the estimator has enough data (spec NFR-PERF-007).
    #[test]
    fn no_eta_without_speed() {
        let mut m = ProgressMeter::new(1_000_000);
        m.record(100);
        assert!(m.eta_seconds().is_none(), "single sample must not yield ETA");
        // Complete transfers also have no ETA.
        let mut m = ProgressMeter::new(100);
        m.record(100);
        assert!(m.eta_seconds().is_none());
    }

    /// With a real (short) time gap the meter produces a speed + ETA.
    #[test]
    fn speed_appears_after_samples_span_time() {
        let mut m = ProgressMeter::new(10_000);
        m.record(1000);
        std::thread::sleep(Duration::from_millis(850));
        m.record(5000);
        assert!(m.speed_bps() > 0.0, "speed should exist after 850ms span");
        assert!(m.eta_seconds().is_some(), "ETA should appear with a real rate");
    }

    #[test]
    fn byte_formatting_is_iec() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(512), "512 B");
        assert_eq!(format_bytes(1024), "1.00 KiB");
        assert_eq!(format_bytes(1536), "1.50 KiB");
        assert_eq!(format_bytes(1024 * 1024), "1.00 MiB");
        assert_eq!(format_bytes(5 * 1024 * 1024 * 1024), "5.00 GiB");
    }

    #[test]
    fn speed_and_eta_formatting() {
        assert_eq!(format_speed(0.0), "—");
        assert_eq!(format_speed(1024.0 * 1024.0), "1.00 MiB/s");
        assert_eq!(format_eta(45), "45s");
        assert_eq!(format_eta(192), "3m 12s");
        assert_eq!(format_eta(3840), "1h 04m");
    }
}
