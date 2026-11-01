//! Exponential backoff with jitter for retrying the server.
//!
//! The first retry comes after 1 to 2 seconds; each failure in a row doubles
//! the wait, up to 30 to 60 seconds. A random factor between one half and one
//! spreads the retries of machines that lost the same server at once.

/// The wait after the first failure, before jitter.
pub const FIRST: f64 = 2.0;
/// The longest wait, before jitter: the worst-case delay before the bridge
/// notices that the server is back when nothing else wakes it.
pub const CAP: f64 = 60.0;

#[derive(Debug, Default)]
pub struct Backoff {
    failures: u32,
}

impl Backoff {
    /// Count one more failure; the seconds to wait before the next try.
    pub fn next_delay(&mut self) -> f64 {
        let delay = Self::ceiling(self.failures);
        self.failures = self.failures.saturating_add(1);
        delay * (0.5 + 0.5 * unit_random())
    }

    /// The longest wait after `failures` earlier failures in a row.
    pub fn ceiling(failures: u32) -> f64 {
        (FIRST * 2f64.powi(failures.min(16) as i32)).min(CAP)
    }

    pub fn reset(&mut self) {
        self.failures = 0;
    }

    pub fn failures(&self) -> u32 {
        self.failures
    }
}

/// A random number in [0, 1), from the standard library's per-process
/// random hash keys (no dependency, not for cryptography).
fn unit_random() -> f64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(crate::fmtutil::time_ns());
    (hasher.finish() >> 11) as f64 / (1u64 << 53) as f64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delays_double_to_the_cap_with_jitter_and_reset() {
        let mut backoff = Backoff::default();
        let mut ceilings = Vec::new();
        for _ in 0..10 {
            let ceiling = Backoff::ceiling(backoff.failures());
            let delay = backoff.next_delay();
            assert!(
                delay >= ceiling / 2.0 && delay <= ceiling,
                "{delay} vs {ceiling}"
            );
            ceilings.push(ceiling);
        }
        assert_eq!(
            ceilings,
            [2.0, 4.0, 8.0, 16.0, 32.0, 60.0, 60.0, 60.0, 60.0, 60.0]
        );
        backoff.reset();
        assert!(backoff.next_delay() <= FIRST);
        // The jitter really varies.
        let draws: std::collections::HashSet<u64> =
            (0..20).map(|_| (unit_random() * 1e9) as u64).collect();
        assert!(draws.len() > 15);
    }
}
