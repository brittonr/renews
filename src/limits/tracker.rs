//! Usage tracker for real-time connection and bandwidth tracking.
//!
//! The `UsageTracker` maintains in-memory state for:
//! - Per-user connection counts
//! - Per-user bandwidth usage with rolling window support
//!
//! Usage is periodically persisted to the database and loaded at startup.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use chrono::{DateTime, Duration, Utc};
use dashmap::DashMap;
use tokio::sync::RwLock;

use crate::auth::DynAuth;
use crate::config::UserLimitsConfig;

use super::post_rate::TokenBucket;
use super::{LimitCheckResult, UserLimits, UserUsage};

/// In-memory bandwidth state for a user.
#[derive(Debug)]
struct BandwidthState {
    bytes_uploaded: u64,
    bytes_downloaded: u64,
    window_start: DateTime<Utc>,
}

impl Default for BandwidthState {
    fn default() -> Self {
        Self {
            bytes_uploaded: 0,
            bytes_downloaded: 0,
            window_start: Utc::now(),
        }
    }
}

/// Real-time usage tracker for connection and bandwidth limits.
///
/// This struct maintains in-memory state that is periodically persisted
/// to the database. It is designed to be shared across all connections
/// via `Arc<UsageTracker>`.
pub struct UsageTracker {
    /// Per-user connection counts: username -> atomic count
    connections: DashMap<String, AtomicUsize>,

    /// Per-user bandwidth usage: username -> bandwidth state
    /// Uses Arc to allow cloning the lock out before awaiting, avoiding deadlocks
    bandwidth: DashMap<String, Arc<RwLock<BandwidthState>>>,

    /// Per-user limits cache: username -> limits (cached from DB)
    limits_cache: DashMap<String, UserLimits>,

    /// Default limits from configuration
    defaults: RwLock<UserLimitsConfig>,

    /// Per-account POST token buckets
    post_buckets: DashMap<String, TokenBucket>,

    /// Auth provider for looking up per-user limits and admin status
    auth: DynAuth,
}

impl UsageTracker {
    /// Create a new usage tracker.
    pub fn new(auth: DynAuth, defaults: UserLimitsConfig) -> Self {
        Self {
            connections: DashMap::new(),
            bandwidth: DashMap::new(),
            limits_cache: DashMap::new(),
            defaults: RwLock::new(defaults),
            post_buckets: DashMap::new(),
            auth,
        }
    }

    /// Update default limits (called on config reload).
    pub async fn update_defaults(&self, defaults: UserLimitsConfig) {
        *self.defaults.write().await = defaults;
    }

    /// Get effective limits for a user (from cache, DB, or defaults).
    async fn get_effective_limits(&self, username: &str) -> UserLimits {
        // Check cache first
        if let Some(cached) = self.limits_cache.get(username) {
            return cached.clone();
        }

        // Try to load from database
        if let Ok(Some(db_limits)) = self.auth.get_user_limits(username).await {
            self.limits_cache
                .insert(username.to_string(), db_limits.clone());
            return db_limits;
        }

        // Fall back to defaults
        let defaults = self.defaults.read().await;

        UserLimits {
            can_post: defaults.allow_posting,
            max_connections: if defaults.max_connections == 0 {
                None
            } else {
                Some(defaults.max_connections)
            },
            bandwidth_limit: defaults.bandwidth_limit,
            bandwidth_period_secs: defaults.bandwidth_period,
        }
    }

    /// Consume one posting token for `username`.
    ///
    /// A configured rate of zero allows every post. Admins are not passed here;
    /// the POST handler skips this check for them.
    pub async fn try_consume_post(&self, username: &str) -> LimitCheckResult {
        let (rate, burst) = {
            let defaults = self.defaults.read().await;
            (defaults.post_rate_per_minute, defaults.post_burst)
        };
        if rate == 0 {
            return LimitCheckResult::Allowed;
        }
        let now = Instant::now();
        let mut bucket = self
            .post_buckets
            .entry(username.to_string())
            .or_insert_with(|| TokenBucket::new(rate, burst, now));
        if !bucket.matches(rate, burst) {
            *bucket = TokenBucket::new(rate, burst, now);
        }
        if bucket.try_take(now) {
            LimitCheckResult::Allowed
        } else {
            LimitCheckResult::PostRateExceeded
        }
    }

    /// Invalidate cached limits for a user (call after updating limits in DB).
    pub fn invalidate_limits_cache(&self, username: &str) {
        self.limits_cache.remove(username);
    }

    /// Try to establish a new connection for a user.
    ///
    /// Returns `Allowed` if the connection is permitted, or `ConnectionLimitExceeded`
    /// if the user has reached their connection limit.
    ///
    /// This method increments the connection count if allowed.
    pub async fn try_connect(&self, username: &str) -> LimitCheckResult {
        let limits = self.get_effective_limits(username).await;

        // Check if connections are unlimited
        let Some(max_connections) = limits.max_connections else {
            // Unlimited - just increment and allow
            self.connections
                .entry(username.to_string())
                .or_insert_with(|| AtomicUsize::new(0))
                .fetch_add(1, Ordering::SeqCst);
            return LimitCheckResult::Allowed;
        };

        // Get or create connection counter
        let counter = self
            .connections
            .entry(username.to_string())
            .or_insert_with(|| AtomicUsize::new(0));

        // Try to increment if under limit
        loop {
            let current = counter.load(Ordering::SeqCst);
            if current >= max_connections as usize {
                return LimitCheckResult::ConnectionLimitExceeded;
            }

            // Try to atomically increment
            if counter
                .compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
            {
                return LimitCheckResult::Allowed;
            }
            // If CAS failed, another thread modified the counter - retry
        }
    }

    /// Record a disconnection for a user.
    pub fn disconnect(&self, username: &str) {
        if let Some(counter) = self.connections.get(username) {
            let prev = counter.fetch_sub(1, Ordering::SeqCst);
            // Clean up if this was the last connection
            if prev == 1 {
                // Must drop the Ref before calling remove() to avoid deadlock
                // (get() holds a read lock, remove() needs a write lock on the same shard)
                drop(counter);
                // Remove the entry to avoid memory growth
                // (the entry might have been incremented again by another thread,
                // so we need to check the value and only remove if still 0)
                if let Some((_, counter)) = self.connections.remove(username) {
                    if counter.load(Ordering::SeqCst) > 0 {
                        // Race: someone connected between our check and remove
                        // Put it back
                        self.connections.insert(username.to_string(), counter);
                    }
                }
            }
        }
    }

    /// Get current connection count for a user.
    #[must_use]
    pub fn connection_count(&self, username: &str) -> usize {
        self.connections
            .get(username)
            .map(|c| c.load(Ordering::SeqCst))
            .unwrap_or(0)
    }

    /// Check if a user can post (based on can_post permission).
    pub async fn can_post(&self, username: &str) -> LimitCheckResult {
        let limits = self.get_effective_limits(username).await;
        if limits.can_post {
            LimitCheckResult::Allowed
        } else {
            LimitCheckResult::PostingDisabled
        }
    }

    /// Check if a bandwidth transfer of `bytes` is allowed.
    ///
    /// This does NOT record the usage - call `record_bandwidth` after
    /// the transfer completes successfully.
    pub async fn check_bandwidth(&self, username: &str, bytes: u64) -> LimitCheckResult {
        let limits = self.get_effective_limits(username).await;

        // Check if bandwidth is unlimited
        let Some(limit) = limits.bandwidth_limit else {
            return LimitCheckResult::Allowed;
        };

        // Get or create bandwidth state, cloning the Arc to release the DashMap
        // reference before awaiting on the inner RwLock (prevents deadlock)
        let state_arc = self
            .bandwidth
            .entry(username.to_string())
            .or_insert_with(|| Arc::new(RwLock::new(BandwidthState::default())))
            .clone();

        // Now we can safely await - DashMap reference has been dropped
        let mut state_guard = state_arc.write().await;

        // Check if window needs reset
        if let Some(period_secs) = limits.bandwidth_period_secs {
            let period = Duration::seconds(period_secs as i64);
            let now = Utc::now();
            if now.signed_duration_since(state_guard.window_start) >= period {
                // Window expired - complete reset
                state_guard.bytes_uploaded = 0;
                state_guard.bytes_downloaded = 0;
                state_guard.window_start = now;
            }
        }

        // Check if this transfer would exceed limit
        let current_total = state_guard
            .bytes_uploaded
            .saturating_add(state_guard.bytes_downloaded);
        if current_total.saturating_add(bytes) > limit {
            return LimitCheckResult::BandwidthExceeded;
        }

        LimitCheckResult::Allowed
    }

    /// Record bandwidth usage after a successful transfer.
    ///
    /// # Arguments
    /// * `username` - The user who performed the transfer
    /// * `bytes` - Number of bytes transferred
    /// * `is_upload` - True for uploads (posting), false for downloads (retrieval)
    pub async fn record_bandwidth(&self, username: &str, bytes: u64, is_upload: bool) {
        // Get or create bandwidth state, cloning the Arc to release the DashMap
        // reference before awaiting on the inner RwLock (prevents deadlock)
        let state_arc = self
            .bandwidth
            .entry(username.to_string())
            .or_insert_with(|| Arc::new(RwLock::new(BandwidthState::default())))
            .clone();

        // Now we can safely await - DashMap reference has been dropped
        let mut state_guard = state_arc.write().await;
        if is_upload {
            state_guard.bytes_uploaded = state_guard.bytes_uploaded.saturating_add(bytes);
        } else {
            state_guard.bytes_downloaded = state_guard.bytes_downloaded.saturating_add(bytes);
        }
    }

    /// Get current usage for a user.
    pub async fn get_usage(&self, username: &str) -> UserUsage {
        // Clone the Arc to release the DashMap reference before awaiting
        let state_arc = self.bandwidth.get(username).map(|r| r.clone());

        if let Some(state_arc) = state_arc {
            let state_guard = state_arc.read().await;
            UserUsage {
                bytes_uploaded: state_guard.bytes_uploaded,
                bytes_downloaded: state_guard.bytes_downloaded,
                window_start: Some(state_guard.window_start),
            }
        } else {
            UserUsage::default()
        }
    }

    /// Reset bandwidth usage for a user.
    pub async fn reset_usage(&self, username: &str) {
        // Clone the Arc to release the DashMap reference before awaiting
        let state_arc = self.bandwidth.get(username).map(|r| r.clone());

        if let Some(state_arc) = state_arc {
            let mut state_guard = state_arc.write().await;
            state_guard.bytes_uploaded = 0;
            state_guard.bytes_downloaded = 0;
            state_guard.window_start = Utc::now();
        }

        // Also reset in database
        if let Err(e) = self.auth.reset_user_usage(username).await {
            tracing::warn!(username, error = %e, "Failed to reset user usage in database");
        }
    }

    /// Persist all current usage to the database.
    ///
    /// This should be called periodically (e.g., every minute) to ensure
    /// usage data survives server restarts.
    pub async fn persist(&self) -> anyhow::Result<()> {
        // Collect usernames and their Arc clones to avoid holding DashMap references across await points
        let user_states: Vec<(String, Arc<RwLock<BandwidthState>>)> = self
            .bandwidth
            .iter()
            .map(|e| (e.key().clone(), e.value().clone()))
            .collect();

        for (username, state_arc) in user_states {
            let state = state_arc.read().await;
            let usage = UserUsage {
                bytes_uploaded: state.bytes_uploaded,
                bytes_downloaded: state.bytes_downloaded,
                window_start: Some(state.window_start),
            };
            drop(state); // Release read lock before awaiting on DB

            if let Err(e) = self.auth.set_user_usage(&username, &usage).await {
                tracing::warn!(username, error = %e, "Failed to persist user usage");
            }
        }

        Ok(())
    }

    /// Load usage from database for all users.
    ///
    /// This should be called at server startup to restore usage state.
    pub async fn load(&self) -> anyhow::Result<()> {
        // Load all users' usage from the database
        // For now, we'll load on-demand when users connect
        // This could be extended to pre-load all users if needed
        Ok(())
    }

    /// Load usage for a specific user from the database.
    pub async fn load_user(&self, username: &str) -> anyhow::Result<()> {
        if let Ok(usage) = self.auth.get_user_usage(username).await {
            let state = BandwidthState {
                bytes_uploaded: usage.bytes_uploaded,
                bytes_downloaded: usage.bytes_downloaded,
                window_start: usage.window_start.unwrap_or_else(Utc::now),
            };
            self.bandwidth
                .insert(username.to_string(), Arc::new(RwLock::new(state)));
        }
        Ok(())
    }
}

impl std::fmt::Debug for UsageTracker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("UsageTracker")
            .field("connections_count", &self.connections.len())
            .field("bandwidth_tracked_users", &self.bandwidth.len())
            .finish()
    }
}
