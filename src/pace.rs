//! Politeness pacing: keep a minimum interval *between* upstream requests.
//!
//! The rule this implements is exactly what the config documents — "pause
//! between upstream requests" — and nothing more:
//!
//! - The **first** request of a run never waits. Sleeping before the only
//!   request a process makes, or after the last one, spends wall-clock time
//!   without protecting anybody.
//! - Subsequent requests wait only for the time still missing since the
//!   previous one, so slow requests already "pay" part of the interval.
//! - The pacer is **shared** across batch workers, so `-j 8` fetches eight
//!   pages concurrently but still issues them at the configured global rate,
//!   instead of multiplying the request rate by the worker count.
//!
//! Pacing is a property of the request stream, never of how chatty the CLI is:
//! `--quiet` must not make webseek faster or ruder.

use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Shared minimum-interval limiter.
#[derive(Debug)]
pub struct Pacer {
    interval: Duration,
    /// When the most recent request slot was handed out.
    last: Mutex<Option<Instant>>,
}

impl Pacer {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: Mutex::new(None),
        }
    }

    /// A pacer that never sleeps.
    pub fn disabled() -> Self {
        Self::new(Duration::ZERO)
    }

    pub fn is_enabled(&self) -> bool {
        !self.interval.is_zero()
    }

    /// Block until the caller may issue its request, then claim the slot.
    ///
    /// Claiming happens while the lock is held, so concurrent workers queue up
    /// one interval apart rather than all waking at once.
    pub fn wait(&self) {
        if !self.is_enabled() {
            return;
        }
        let sleep_for = {
            let Ok(mut slot) = self.last.lock() else {
                return; // poisoned: never block the caller on a pacing detail
            };
            let now = Instant::now();
            // `slot` holds the instant the previous caller was *scheduled* for,
            // which may still be in the future when several workers arrive at
            // once. Building on it — rather than on "now" — is what makes N
            // concurrent workers queue up N intervals apart instead of two.
            let target = match *slot {
                Some(prev) => (prev + self.interval).max(now),
                None => now, // first request of the run goes immediately
            };
            *slot = Some(target);
            target.saturating_duration_since(now)
        };
        if !sleep_for.is_zero() {
            std::thread::sleep(sleep_for);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn first_request_never_waits() {
        let p = Pacer::new(Duration::from_millis(500));
        let t = Instant::now();
        p.wait();
        assert!(
            t.elapsed() < Duration::from_millis(100),
            "the first request of a run must not sleep"
        );
    }

    #[test]
    fn second_request_waits_the_interval() {
        let p = Pacer::new(Duration::from_millis(150));
        p.wait();
        let t = Instant::now();
        p.wait();
        assert!(
            t.elapsed() >= Duration::from_millis(120),
            "expected the interval to be enforced, waited {:?}",
            t.elapsed()
        );
    }

    #[test]
    fn disabled_pacer_never_sleeps() {
        let p = Pacer::disabled();
        let t = Instant::now();
        for _ in 0..5 {
            p.wait();
        }
        assert!(t.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn concurrent_workers_share_one_global_rate() {
        // Four workers, three intervals of spacing between four slots.
        let p = Arc::new(Pacer::new(Duration::from_millis(80)));
        let start = Instant::now();
        std::thread::scope(|s| {
            for _ in 0..4 {
                let p = p.clone();
                s.spawn(move || p.wait());
            }
        });
        let elapsed = start.elapsed();
        assert!(
            elapsed >= Duration::from_millis(200),
            "parallel workers must not multiply the request rate (took {elapsed:?})"
        );
    }

    #[test]
    fn elapsed_time_counts_towards_the_interval() {
        let p = Pacer::new(Duration::from_millis(200));
        p.wait();
        std::thread::sleep(Duration::from_millis(210));
        let t = Instant::now();
        p.wait(); // interval already elapsed -> no extra sleep
        assert!(t.elapsed() < Duration::from_millis(60));
    }
}
