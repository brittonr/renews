//! Process counters for the admin `metrics` command.
//!
//! These are process-local and cheap: atomics on the hot path, one JSON
//! snapshot on request. They reset on restart, which is what a dashboard
//! wants to see after a change.

use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde_json::{Value, json};

/// Counters for one newsgroup.
#[derive(Default)]
pub struct GroupMetrics {
    pub posted: AtomicU64,
    pub fetched: AtomicU64,
}

/// Process-wide counters.
pub struct Metrics {
    started: Instant,
    connections_total: AtomicU64,
    connections_current: AtomicU64,
    articles_posted: AtomicU64,
    articles_fetched: AtomicU64,
    groups: DashMap<String, GroupMetrics>,
}

/// The process counters.
pub static METRICS: LazyLock<Metrics> = LazyLock::new(Metrics::new);

impl Metrics {
    fn new() -> Self {
        Self {
            started: Instant::now(),
            connections_total: AtomicU64::new(0),
            connections_current: AtomicU64::new(0),
            articles_posted: AtomicU64::new(0),
            articles_fetched: AtomicU64::new(0),
            groups: DashMap::new(),
        }
    }

    pub fn connection_opened(&self) {
        self.connections_total.fetch_add(1, Ordering::Relaxed);
        self.connections_current.fetch_add(1, Ordering::Relaxed);
    }

    pub fn connection_closed(&self) {
        self.connections_current.fetch_sub(1, Ordering::Relaxed);
    }

    pub fn article_posted(&self, group: &str) {
        self.articles_posted.fetch_add(1, Ordering::Relaxed);
        self.group(group).posted.fetch_add(1, Ordering::Relaxed);
    }

    pub fn article_fetched(&self, group: &str) {
        self.articles_fetched.fetch_add(1, Ordering::Relaxed);
        self.group(group).fetched.fetch_add(1, Ordering::Relaxed);
    }

    fn group(&self, group: &str) -> dashmap::mapref::one::RefMut<'_, String, GroupMetrics> {
        self.groups.entry(group.to_string()).or_default()
    }

    pub fn uptime(&self) -> Duration {
        self.started.elapsed()
    }

    /// Counters as JSON, groups sorted by name. One entry per group that has
    /// seen a post or a read.
    pub fn snapshot(&self) -> Value {
        let mut groups: Vec<(String, u64, u64)> = self
            .groups
            .iter()
            .map(|entry| {
                (
                    entry.key().clone(),
                    entry.value().posted.load(Ordering::Relaxed),
                    entry.value().fetched.load(Ordering::Relaxed),
                )
            })
            .collect();
        groups.sort();
        json!({
            "uptime_secs": self.uptime().as_secs(),
            "connections": {
                "total": self.connections_total.load(Ordering::Relaxed),
                "current": self.connections_current.load(Ordering::Relaxed),
            },
            "articles": {
                "posted": self.articles_posted.load(Ordering::Relaxed),
                "fetched": self.articles_fetched.load(Ordering::Relaxed),
            },
            "groups": groups
                .into_iter()
                .map(|(group, posted, fetched)| json!({
                    "group": group,
                    "posted": posted,
                    "fetched": fetched,
                }))
                .collect::<Vec<_>>(),
        })
    }
}

/// Counts one NNTP connection for as long as it is held.
pub struct ConnectionGuard;

impl Default for ConnectionGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl ConnectionGuard {
    pub fn new() -> Self {
        METRICS.connection_opened();
        Self
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        METRICS.connection_closed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_move_and_snapshot_is_sorted() {
        let metrics = Metrics::new();
        metrics.connection_opened();
        metrics.connection_opened();
        metrics.connection_closed();
        metrics.article_posted("bots.general");
        metrics.article_posted("bots.claims");
        metrics.article_fetched("bots.general");

        let snapshot = metrics.snapshot();
        assert_eq!(snapshot["connections"]["total"], 2);
        assert_eq!(snapshot["connections"]["current"], 1);
        assert_eq!(snapshot["articles"]["posted"], 2);
        assert_eq!(snapshot["articles"]["fetched"], 1);
        let groups = snapshot["groups"].as_array().unwrap();
        assert_eq!(groups[0]["group"], "bots.claims");
        assert_eq!(groups[1]["group"], "bots.general");
        assert_eq!(groups[1]["fetched"], 1);
    }
}
