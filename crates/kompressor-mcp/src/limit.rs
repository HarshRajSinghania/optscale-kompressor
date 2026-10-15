//! The per-tenant ceiling on retrievals.
//!
//! The streamable-HTTP surface is stateless, so there is no session in which to
//! notice that a model has asked for the same fragment forty times. Nothing else
//! on this surface bounds a retrieval loop, and an unbounded one is not merely
//! slow: every fragment it fetches is re-injected into the next request and
//! billed again, so a loop can spend more than the compression it is undoing
//! ever saved. This counter is the only guard against that, which is why it is
//! enforced here rather than left to the caller.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use parking_lot::Mutex;

const WINDOW: Duration = Duration::from_secs(60);

/// Tenants tracked before the map is swept of finished windows.
///
/// A sweep is O(tracked tenants) and only runs when a new tenant arrives after
/// the map has grown, so a single busy tenant never pays for it.
const SWEEP_THRESHOLD: usize = 1024;

/// Whether a retrieval may proceed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    /// Under the limit; the retrieval is counted.
    Allowed,
    /// Over the limit. Carries the configured ceiling and the seconds until the
    /// window resets, so the caller can say something actionable rather than
    /// just refusing.
    Limited {
        /// Retrievals allowed per minute for this tenant.
        per_minute: u32,
        /// Whole seconds until the current window ends.
        retry_after_seconds: u64,
    },
}

struct Window {
    started: Instant,
    count: u32,
}

/// A fixed-window retrieval counter, keyed by tenant.
///
/// Fixed rather than sliding on purpose: the thing being prevented is a runaway
/// loop making thousands of calls, and a fixed window stops that just as well
/// while costing one integer per tenant. The burst a fixed window permits at a
/// boundary is bounded by the same ceiling and is not worth a ring buffer.
pub struct RetrievalLimiter {
    per_minute: u32,
    windows: Mutex<HashMap<String, Window>>,
}

impl RetrievalLimiter {
    /// A limiter allowing `per_minute` retrievals per tenant. Zero disables the
    /// limit, which is how an operator turns the guard off deliberately.
    pub fn new(per_minute: u32) -> RetrievalLimiter {
        RetrievalLimiter {
            per_minute,
            windows: Mutex::new(HashMap::new()),
        }
    }

    /// Count one retrieval for `tenant` and say whether it may proceed.
    pub fn check(&self, tenant: &str) -> Decision {
        self.check_at(tenant, Instant::now())
    }

    fn check_at(&self, tenant: &str, now: Instant) -> Decision {
        if self.per_minute == 0 {
            return Decision::Allowed;
        }
        let mut windows = self.windows.lock();
        if windows.len() >= SWEEP_THRESHOLD && !windows.contains_key(tenant) {
            windows.retain(|_, window| now.duration_since(window.started) < WINDOW);
        }
        let window = windows.entry(tenant.to_string()).or_insert(Window {
            started: now,
            count: 0,
        });
        let elapsed = now.duration_since(window.started);
        if elapsed >= WINDOW {
            window.started = now;
            window.count = 0;
        }
        if window.count >= self.per_minute {
            return Decision::Limited {
                per_minute: self.per_minute,
                retry_after_seconds: WINDOW
                    .saturating_sub(now.duration_since(window.started))
                    .as_secs()
                    + 1,
            };
        }
        window.count += 1;
        Decision::Allowed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tenant_may_retrieve_up_to_its_ceiling_and_is_then_refused() {
        let limiter = RetrievalLimiter::new(3);
        for _ in 0..3 {
            assert_eq!(limiter.check("team-a"), Decision::Allowed);
        }
        match limiter.check("team-a") {
            Decision::Limited {
                per_minute,
                retry_after_seconds,
            } => {
                assert_eq!(per_minute, 3);
                assert!(retry_after_seconds > 0, "a refusal must say when to retry");
            }
            Decision::Allowed => panic!("the ceiling was not enforced"),
        }
    }

    #[test]
    fn one_tenants_runaway_loop_does_not_refuse_another_tenant() {
        let limiter = RetrievalLimiter::new(1);
        assert_eq!(limiter.check("team-a"), Decision::Allowed);
        assert!(matches!(limiter.check("team-a"), Decision::Limited { .. }));
        assert_eq!(
            limiter.check("team-b"),
            Decision::Allowed,
            "the counter is per tenant, not global"
        );
    }

    #[test]
    fn the_window_reopens_once_a_minute_has_passed() {
        let limiter = RetrievalLimiter::new(1);
        let start = Instant::now();
        assert_eq!(limiter.check_at("team-a", start), Decision::Allowed);
        assert!(matches!(
            limiter.check_at("team-a", start),
            Decision::Limited { .. }
        ));
        assert_eq!(
            limiter.check_at("team-a", start + WINDOW + Duration::from_millis(1)),
            Decision::Allowed
        );
    }

    #[test]
    fn a_ceiling_of_zero_turns_the_guard_off() {
        let limiter = RetrievalLimiter::new(0);
        for _ in 0..1000 {
            assert_eq!(limiter.check("team-a"), Decision::Allowed);
        }
    }

    #[test]
    fn finished_windows_do_not_accumulate_forever() {
        let limiter = RetrievalLimiter::new(5);
        let start = Instant::now();
        for i in 0..SWEEP_THRESHOLD {
            limiter.check_at(&format!("tenant-{i}"), start);
        }
        // A tenant arriving after the window has closed sweeps the dead entries
        // rather than letting one handle per caller ever seen stay resident.
        limiter.check_at("late", start + WINDOW + Duration::from_millis(1));
        assert!(limiter.windows.lock().len() < SWEEP_THRESHOLD);
    }
}
