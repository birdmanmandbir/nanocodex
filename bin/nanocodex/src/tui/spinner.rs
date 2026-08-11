//! Demand-driven spinner shared by streaming components.
//!
//! Adapted from `clabby/tact` at
//! `4df68c820427643216d6f2d61c58af89acc27a30`, licensed Apache-2.0.

use std::time::{Duration, Instant};

pub(super) const SPINNER_INTERVAL: Duration = Duration::from_millis(80);
const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

#[derive(Clone, Copy, Debug)]
pub(super) struct Spinner {
    started_at: Instant,
    next_frame: Instant,
    frame: usize,
}

impl Spinner {
    pub(super) fn new(now: Instant) -> Self {
        Self {
            started_at: now,
            next_frame: now + SPINNER_INTERVAL,
            frame: 0,
        }
    }

    /// Advances after the deadline and reports whether the displayed symbol changed.
    pub(super) fn advance(&mut self, now: Instant) -> bool {
        if now < self.next_frame {
            return false;
        }

        let frame = frame_index(self.started_at, now);
        self.next_frame = now + SPINNER_INTERVAL;
        if frame == self.frame {
            return false;
        }
        self.frame = frame;
        true
    }

    pub(super) const fn symbol(self) -> &'static str {
        FRAMES[self.frame]
    }

    pub(super) const fn deadline(self) -> Instant {
        self.next_frame
    }
}

fn frame_index(started_at: Instant, now: Instant) -> usize {
    let ticks =
        now.saturating_duration_since(started_at).as_millis() / SPINNER_INTERVAL.as_millis();
    usize::try_from(ticks).unwrap_or(usize::MAX) % FRAMES.len()
}

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use super::{SPINNER_INTERVAL, Spinner};

    #[test]
    fn spinner_advances_only_at_demand_driven_deadlines() {
        let started_at = Instant::now();
        let mut spinner = Spinner::new(started_at);

        assert_eq!(spinner.symbol(), "⠋");
        assert!(!spinner.advance(started_at + SPINNER_INTERVAL / 2));
        assert_eq!(spinner.deadline(), started_at + SPINNER_INTERVAL);
        assert!(spinner.advance(started_at + SPINNER_INTERVAL));
        assert_eq!(spinner.symbol(), "⠙");
        assert_eq!(spinner.deadline(), started_at + SPINNER_INTERVAL * 2);
    }

    #[test]
    fn delayed_spinner_frame_schedules_its_next_deadline_in_the_future() {
        let started_at = Instant::now();
        let mut spinner = Spinner::new(started_at);
        let delayed = started_at + SPINNER_INTERVAL * 25;

        assert!(spinner.advance(delayed));
        assert_eq!(spinner.symbol(), "⠴");
        assert_eq!(spinner.deadline(), delayed + SPINNER_INTERVAL);
    }

    #[test]
    fn a_full_cycle_does_not_request_an_unchanged_render() {
        let started_at = Instant::now();
        let mut spinner = Spinner::new(started_at);
        let delayed = started_at + SPINNER_INTERVAL * 20;

        assert!(!spinner.advance(delayed));
        assert_eq!(spinner.symbol(), "⠋");
        assert_eq!(spinner.deadline(), delayed + SPINNER_INTERVAL);
    }
}
