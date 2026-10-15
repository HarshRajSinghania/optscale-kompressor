//! Injectable wall-clock seconds.
//!
//! Every TTL decision in this crate reads time through a [`Clock`] rather than
//! calling [`std::time::SystemTime`] directly. That is not a testing
//! affordance bolted on afterwards: the guarantees worth testing here are all
//! *about* the passage of time — an idle window that a read restarts, an
//! absolute lifetime that a read cannot restart — and a suite that establishes
//! them by sleeping is either slow enough that nobody runs it or short enough
//! that a loaded CI box fails it. With a hand-driven clock the same suite
//! proves the eight-hour cap in microseconds.

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// A source of unix seconds, shared cheaply between store handles.
#[derive(Clone)]
pub struct Clock(Arc<dyn Fn() -> u64 + Send + Sync>);

impl Clock {
    /// The host clock.
    pub fn system() -> Clock {
        Clock(Arc::new(|| {
            // A host clock set before the epoch answers 0, which reads as "very
            // old" and purges the entry. Storing fragments is a best-effort path
            // that must never panic, and expiring early is the safe direction:
            // the caller re-compresses, where a panic would take the request.
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        }))
    }

    /// A clock backed by an arbitrary function.
    pub fn from_fn(f: impl Fn() -> u64 + Send + Sync + 'static) -> Clock {
        Clock(Arc::new(f))
    }

    /// The current time in unix seconds.
    pub fn now(&self) -> u64 {
        (self.0)()
    }
}

impl Default for Clock {
    fn default() -> Clock {
        Clock::system()
    }
}

impl fmt::Debug for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // the closure has nothing to show; reporting `now` would make two
        // `Debug` renderings of one store differ for no reason
        f.write_str("Clock")
    }
}

/// A clock the caller advances by hand.
///
/// Cloning it shares the same instant, so a store built from
/// [`ManualClock::clock`] moves when the test moves it.
#[derive(Clone, Debug, Default)]
pub struct ManualClock(Arc<AtomicU64>);

impl ManualClock {
    /// A clock stopped at `secs`.
    pub fn at(secs: u64) -> ManualClock {
        ManualClock(Arc::new(AtomicU64::new(secs)))
    }

    /// A [`Clock`] reading this instant, for handing to a store.
    pub fn clock(&self) -> Clock {
        let cell = Arc::clone(&self.0);
        Clock::from_fn(move || cell.load(Ordering::SeqCst))
    }

    /// Jump to `secs`.
    pub fn set(&self, secs: u64) {
        self.0.store(secs, Ordering::SeqCst);
    }

    /// Move forward by `secs`.
    pub fn advance(&self, secs: u64) {
        self.0.fetch_add(secs, Ordering::SeqCst);
    }

    /// The instant this clock is stopped at.
    pub fn now(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}
