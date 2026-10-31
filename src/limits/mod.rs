//! User limits and usage tracking module.
//!
//! This module provides per-user rate limiting and usage tracking functionality:
//! - Post permission control (can_post flag)
//! - Bandwidth limits (combined upload + download)
//! - Connection limits (max simultaneous connections)
//! - Usage tracking with time-windowed resets

mod post_rate;
mod tracker;

pub use post_rate::TokenBucket;
pub use tracker::UsageTracker;

use chrono::{DateTime, Utc};

/// Per-user limit configuration.
///
/// These limits can be set per-user in the database, or default values
/// from the configuration file are used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserLimits {
    /// Whether the user is allowed to post articles
    pub can_post: bool,

    /// Maximum number of simultaneous connections (None = unlimited)
    pub max_connections: Option<u32>,

    /// Combined bandwidth limit in bytes (None = unlimited)
    pub bandwidth_limit: Option<u64>,

    /// Bandwidth period in seconds (None = absolute/lifetime limit)
    pub bandwidth_period_secs: Option<u64>,
}

impl Default for UserLimits {
    fn default() -> Self {
        Self {
            can_post: true,
            max_connections: None,
            bandwidth_limit: None,
            bandwidth_period_secs: None,
        }
    }
}

impl UserLimits {
    /// Create new limits with all unlimited
    #[must_use]
    pub fn unlimited() -> Self {
        Self::default()
    }

    /// Check if connections are unlimited
    #[must_use]
    pub fn is_connections_unlimited(&self) -> bool {
        self.max_connections.is_none()
    }

    /// Check if bandwidth is unlimited
    #[must_use]
    pub fn is_bandwidth_unlimited(&self) -> bool {
        self.bandwidth_limit.is_none()
    }
}

/// Current usage statistics for a user.
#[derive(Debug, Clone, Default)]
pub struct UserUsage {
    /// Total bytes uploaded (articles posted)
    pub bytes_uploaded: u64,

    /// Total bytes downloaded (articles retrieved)
    pub bytes_downloaded: u64,

    /// Start of the current bandwidth window (for time-based limits)
    pub window_start: Option<DateTime<Utc>>,
}

impl UserUsage {
    /// Get total bandwidth used (upload + download combined)
    #[must_use]
    pub fn total_bandwidth(&self) -> u64 {
        self.bytes_uploaded.saturating_add(self.bytes_downloaded)
    }

    /// Reset all usage counters
    pub fn reset(&mut self) {
        self.bytes_uploaded = 0;
        self.bytes_downloaded = 0;
        self.window_start = Some(Utc::now());
    }
}

/// Result of a limit check operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LimitCheckResult {
    /// Operation is allowed
    Allowed,

    /// User's posting permission is disabled
    PostingDisabled,

    /// Bandwidth limit has been exceeded
    BandwidthExceeded,

    /// Connection limit has been exceeded
    ConnectionLimitExceeded,

    /// The account's posting token bucket is empty
    PostRateExceeded,
}

impl LimitCheckResult {
    /// Check if the result indicates the operation is allowed
    #[must_use]
    pub fn is_allowed(&self) -> bool {
        matches!(self, Self::Allowed)
    }

    /// Check if the result indicates any kind of limit exceeded
    #[must_use]
    pub fn is_denied(&self) -> bool {
        !self.is_allowed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_user_limits_default() {
        let limits = UserLimits::default();
        assert!(limits.can_post);
        assert!(limits.is_connections_unlimited());
        assert!(limits.is_bandwidth_unlimited());
    }

    #[test]
    fn test_user_limits_with_values() {
        let limits = UserLimits {
            can_post: false,
            max_connections: Some(5),
            bandwidth_limit: Some(1024 * 1024 * 1024), // 1 GB
            bandwidth_period_secs: Some(30 * 24 * 60 * 60), // 30 days
        };
        assert!(!limits.can_post);
        assert!(!limits.is_connections_unlimited());
        assert!(!limits.is_bandwidth_unlimited());
        assert_eq!(limits.max_connections, Some(5));
    }

    #[test]
    fn test_user_usage_total_bandwidth() {
        let usage = UserUsage {
            bytes_uploaded: 1000,
            bytes_downloaded: 2000,
            window_start: None,
        };
        assert_eq!(usage.total_bandwidth(), 3000);
    }

    #[test]
    fn test_user_usage_reset() {
        let mut usage = UserUsage {
            bytes_uploaded: 1000,
            bytes_downloaded: 2000,
            window_start: None,
        };
        usage.reset();
        assert_eq!(usage.bytes_uploaded, 0);
        assert_eq!(usage.bytes_downloaded, 0);
        assert!(usage.window_start.is_some());
    }

    #[test]
    fn test_limit_check_result() {
        assert!(LimitCheckResult::Allowed.is_allowed());
        assert!(!LimitCheckResult::Allowed.is_denied());

        assert!(!LimitCheckResult::PostingDisabled.is_allowed());
        assert!(LimitCheckResult::PostingDisabled.is_denied());

        assert!(!LimitCheckResult::BandwidthExceeded.is_allowed());
        assert!(LimitCheckResult::BandwidthExceeded.is_denied());

        assert!(!LimitCheckResult::ConnectionLimitExceeded.is_allowed());
        assert!(LimitCheckResult::ConnectionLimitExceeded.is_denied());

        assert!(!LimitCheckResult::PostRateExceeded.is_allowed());
        assert!(LimitCheckResult::PostRateExceeded.is_denied());
    }
}
