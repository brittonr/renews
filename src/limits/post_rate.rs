//! Per-account posting token bucket.
//!
//! Tokens refill continuously at `posts_per_minute / 60` and never exceed
//! `burst`. Each accepted POST consumes one token. A rate of zero means the
//! bucket is not used.

use std::time::Instant;

/// In-memory token bucket for one account.
#[derive(Debug, Clone)]
pub struct TokenBucket {
    tokens: f64,
    updated: Instant,
    rate_per_sec: f64,
    capacity: f64,
    posts_per_minute: u32,
    burst: u32,
}

impl TokenBucket {
    /// `posts_per_minute` of zero means unlimited and [`Self::try_take`] always succeeds.
    /// A burst of zero is treated as one so a configured rate cannot lock the account out.
    #[must_use]
    pub fn new(posts_per_minute: u32, burst: u32, now: Instant) -> Self {
        let burst = if posts_per_minute == 0 {
            0
        } else {
            burst.max(1)
        };
        let capacity = f64::from(burst);
        Self {
            tokens: capacity,
            updated: now,
            rate_per_sec: f64::from(posts_per_minute) / 60.0,
            capacity,
            posts_per_minute,
            burst,
        }
    }

    /// Replace the bucket when the operator changes the configured rate.
    pub fn matches(&self, posts_per_minute: u32, burst: u32) -> bool {
        let burst = if posts_per_minute == 0 {
            0
        } else {
            burst.max(1)
        };
        self.posts_per_minute == posts_per_minute && self.burst == burst
    }

    /// Consume one token if one is available at `now`.
    pub fn try_take(&mut self, now: Instant) -> bool {
        if self.posts_per_minute == 0 {
            return true;
        }
        self.refill(now);
        if self.tokens >= 1.0 {
            self.tokens -= 1.0;
            true
        } else {
            false
        }
    }

    fn refill(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.updated).as_secs_f64();
        self.updated = now;
        if elapsed <= 0.0 || self.rate_per_sec <= 0.0 {
            return;
        }
        self.tokens = (self.tokens + elapsed * self.rate_per_sec).min(self.capacity);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn burst_then_refill() {
        let start = Instant::now();
        // One post per second, two in a burst.
        let mut bucket = TokenBucket::new(60, 2, start);
        assert!(bucket.try_take(start));
        assert!(bucket.try_take(start));
        assert!(!bucket.try_take(start), "burst is exhausted");

        let later = start + Duration::from_secs(1);
        assert!(bucket.try_take(later), "one token refills after a second");
        assert!(!bucket.try_take(later));
    }

    #[test]
    fn zero_rate_is_unlimited() {
        let now = Instant::now();
        let mut bucket = TokenBucket::new(0, 0, now);
        for _ in 0..20 {
            assert!(bucket.try_take(now));
        }
    }

    #[test]
    fn refill_does_not_exceed_burst() {
        let start = Instant::now();
        let mut bucket = TokenBucket::new(60, 2, start);
        assert!(bucket.try_take(start));
        let later = start + Duration::from_secs(120);
        assert!(bucket.try_take(later));
        assert!(bucket.try_take(later));
        assert!(!bucket.try_take(later));
    }
}
