//! The restart policy for a view engine's reducer (pure): how many
//! times a reducer that panicked is rebuilt, and how long the engine
//! waits before each rebuild.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RestartPolicy {
    /// Rebuilds allowed over the engine's life.
    pub max_restarts: u32,
    /// Wait before the first rebuild; it doubles for each later one.
    pub base_delay: Duration,
    /// Longest wait before a rebuild.
    pub max_delay: Duration,
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self {
            max_restarts: 5,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(5),
        }
    }
}

impl RestartPolicy {
    /// The wait before the next rebuild when `done` rebuilds already
    /// happened; `None` once `max_restarts` are spent.
    pub fn delay(&self, done: u32) -> Option<Duration> {
        (done < self.max_restarts).then(|| {
            let factor = 1u32.checked_shl(done).unwrap_or(u32::MAX);
            self.base_delay.saturating_mul(factor).min(self.max_delay)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(max_restarts: u32) -> RestartPolicy {
        RestartPolicy {
            max_restarts,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_millis(350),
        }
    }

    #[test]
    fn the_wait_doubles_up_to_the_cap() {
        let p = policy(4);
        let waits: Vec<_> = (0..4).map(|n| p.delay(n).unwrap().as_millis()).collect();
        assert_eq!(waits, [100, 200, 350, 350]);
    }

    #[test]
    fn restarts_are_bounded() {
        assert_eq!(policy(2).delay(2), None);
        assert_eq!(policy(2).delay(u32::MAX), None);
        assert_eq!(policy(0).delay(0), None);
    }

    #[test]
    fn a_huge_restart_count_does_not_overflow_the_wait() {
        let p = RestartPolicy {
            max_restarts: u32::MAX,
            ..policy(0)
        };
        assert_eq!(p.delay(40), Some(Duration::from_millis(350)));
    }
}
