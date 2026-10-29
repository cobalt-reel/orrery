//! Frame times: how long `render_output` takes per frame (CPU time on the compositor thread; the GPU's own work
//! runs on after it), for `state` and a periodic log.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::ipc::FramesView;

/// Frames kept for the statistics: about a minute at 60 Hz.
pub const WINDOW: usize = 4096;
/// The info log line comes at most this often.
pub const LOG_EVERY: Duration = Duration::from_secs(10);
/// A screen's failed frames are logged at most this often: one that keeps failing would fill the log at 60 Hz.
pub const WARN_EVERY: Duration = Duration::from_secs(10);

/// Whether a failed frame is logged, the last one having been logged at `last`.
#[must_use]
pub fn warn_due(last: Option<Instant>, now: Instant) -> bool {
    last.is_none_or(|t| now.duration_since(t) >= WARN_EVERY)
}

pub struct FrameStats {
    count: u64,
    window: VecDeque<Duration>,
    /// Since the last log line: frames, total and longest.
    period: (u64, Duration, Duration),
    last_log: Instant,
}

impl FrameStats {
    pub fn new(now: Instant) -> Self {
        Self {
            count: 0,
            window: VecDeque::with_capacity(WINDOW),
            period: (0, Duration::ZERO, Duration::ZERO),
            last_log: now,
        }
    }

    /// One frame took `d`; the log line to write when one is due (frames since the last one).
    pub fn record(&mut self, d: Duration, now: Instant) -> Option<String> {
        self.count += 1;
        if self.window.len() == WINDOW {
            self.window.pop_front();
        }
        self.window.push_back(d);
        let (n, sum, max) = &mut self.period;
        *n += 1;
        *sum += d;
        *max = (*max).max(d);
        if now.duration_since(self.last_log) < LOG_EVERY {
            return None;
        }
        let (n, sum, max) = std::mem::replace(&mut self.period, (0, Duration::ZERO, Duration::ZERO));
        let secs = now.duration_since(self.last_log).as_secs_f64();
        self.last_log = now;
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        Some(format!(
            "frames: {n} in {secs:.0} s ({:.1}/s), mean {:.2} ms, max {:.2} ms",
            n as f64 / secs,
            ms(sum) / n.max(1) as f64,
            ms(max)
        ))
    }

    /// Statistics over the last `WINDOW` frames (and the count of all); None before the first frame.
    pub fn view(&self) -> Option<FramesView> {
        if self.window.is_empty() {
            return None;
        }
        let mut us: Vec<u64> = (self.window.iter())
            .map(|d| u64::try_from(d.as_micros()).unwrap_or(u64::MAX))
            .collect();
        us.sort_unstable();
        let n = us.len();
        // Nearest rank.
        let p95 = us[(n * 95).div_ceil(100).clamp(1, n) - 1];
        Some(FramesView {
            count: self.count,
            min_us: us[0],
            mean_us: us.iter().sum::<u64>() / n as u64,
            p95_us: p95,
            max_us: us[n - 1],
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failing_screen_is_logged_once_per_ten_seconds() {
        let t = Instant::now();
        assert!(warn_due(None, t), "the first failure");
        assert!(!warn_due(Some(t), t + Duration::from_secs(9)));
        assert!(warn_due(Some(t), t + Duration::from_secs(10)));
    }

    #[test]
    fn statistics_over_the_window_and_a_log_line_every_ten_seconds() {
        let t0 = Instant::now();
        let mut f = FrameStats::new(t0);
        assert_eq!(f.view(), None);
        // 1..=100 ms.
        for i in 1..=100u64 {
            assert_eq!(f.record(Duration::from_millis(i), t0 + Duration::from_millis(i)), None);
        }
        let v = f.view().unwrap();
        assert_eq!(
            (v.count, v.min_us, v.mean_us, v.p95_us, v.max_us),
            (100, 1000, 50_500, 95_000, 100_000)
        );
        let line = f.record(Duration::from_millis(2), t0 + LOG_EVERY).expect("due");
        assert!(line.starts_with("frames: 101 in 10 s"), "{line}");
        assert!(line.contains("max 100.00 ms"), "{line}");
        assert_eq!(
            f.record(Duration::from_millis(2), t0 + LOG_EVERY + Duration::from_secs(1)),
            None
        );
    }

    #[test]
    fn the_window_keeps_the_latest_frames_and_counts_all() {
        let t0 = Instant::now();
        let mut f = FrameStats::new(t0);
        for _ in 0..WINDOW {
            f.record(Duration::from_millis(50), t0);
        }
        for _ in 0..WINDOW {
            f.record(Duration::from_millis(1), t0);
        }
        let v = f.view().unwrap();
        assert_eq!((v.count, v.max_us), (2 * WINDOW as u64, 1000));
    }
}
