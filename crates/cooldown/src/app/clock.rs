//! Adapters for the [`Clock`] port: where the run's single "now" comes from.
//!
//! [`SystemClock`] reads the wall clock and is what every real invocation uses. [`FixedClock`]
//! returns a constant instant, so a run can be evaluated "as of" a chosen time — used by the
//! `--now` debug flag to regenerate the README screenshots reproducibly, and available to tests.
//! The composition root ([`crate::cli`]) selects one and samples it once into the [`Workspace`].
//!
//! [`Workspace`]: crate::app::Workspace

pub use cooldown_core::Clock;
use jiff::Timestamp;

/// The real clock: the current wall-clock instant.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> Timestamp {
        Timestamp::now()
    }
}

/// A clock pinned to a fixed instant, so time-dependent output is reproducible.
#[derive(Debug, Clone, Copy)]
pub struct FixedClock(Timestamp);

impl FixedClock {
    /// Builds a clock that always reports `instant`.
    #[must_use]
    pub fn new(instant: Timestamp) -> Self {
        FixedClock(instant)
    }
}

impl Clock for FixedClock {
    fn now(&self) -> Timestamp {
        self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, FixedClock};
    use jiff::Timestamp;

    #[test]
    fn fixed_clock_reports_its_instant() {
        let instant: Timestamp = "2026-06-22T00:00:00Z".parse().expect("instant");
        assert_eq!(FixedClock::new(instant).now(), instant);
    }
}
