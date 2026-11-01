//! New-article announcements on iroh-gossip.
//!
//! The server is the only publisher. Each note is a small signed header.
//! Clients ignore a message whose signature was not made by this endpoint,
//! even when a neighbor relays it. `delivered_from` on a gossip event is the
//! neighbor that handed us the bytes, not the author, so the signature is
//! the check that matters.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use iroh::{Endpoint, EndpointId};
use iroh_gossip::api::{GossipReceiver, GossipSender};
use iroh_gossip::{Gossip, TopicId};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex as AsyncMutex;

use crate::config::Config;
use crate::notify::{ArticleFanout, ArticleNote, signed_bytes};
use crate::storage::DynStorage;

/// ALPN registered beside NNTP. This is iroh-gossip's own protocol id.
pub use iroh_gossip::ALPN as GOSSIP_ALPN;

const MAX_NOTE_BYTES: usize = 2048;
const MAX_NOTES_PER_MINUTE: u32 = 60;

/// Topic for one group on one server. Stable for the life of `iroh.key`.
#[must_use]
pub fn topic_id(endpoint_id: &EndpointId, group: &str) -> TopicId {
    let mut hasher = Sha256::new();
    hasher.update(endpoint_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(group.as_bytes());
    let bytes: [u8; 32] = hasher.finalize().into();
    TopicId::from_bytes(bytes)
}

/// Gossip handler that rejects endpoints the NNTP allowlist would reject.
#[derive(Clone)]
pub struct GatedGossip {
    inner: Gossip,
    config: Arc<tokio::sync::RwLock<Config>>,
    runtime: crate::admin::RuntimeStore,
}

impl std::fmt::Debug for GatedGossip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GatedGossip").finish_non_exhaustive()
    }
}

impl GatedGossip {
    async fn allowed(&self, remote: EndpointId) -> bool {
        let cfg = self.config.read().await;
        if !cfg.iroh.gossip.enable {
            return false;
        }
        match crate::admin::nntp_peer_allowed(&cfg, &self.runtime, remote).await {
            Ok(allowed) => allowed,
            Err(err) => {
                tracing::warn!(error = %err, "gossip allowlist lookup failed");
                false
            }
        }
    }
}

impl ProtocolHandler for GatedGossip {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();
        if !self.allowed(remote).await {
            tracing::warn!(endpoint_id = %remote, "rejected gossip peer not on the allowlist");
            connection.close(1u32.into(), b"endpoint not allowed");
            return Err(AcceptError::from_err(GossipRejected));
        }
        self.inner.accept(connection).await
    }

    async fn shutdown(&self) {
        ProtocolHandler::shutdown(&self.inner).await;
    }
}

#[derive(Debug)]
struct GossipRejected;

impl std::fmt::Display for GossipRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("endpoint not allowed")
    }
}

impl std::error::Error for GossipRejected {}

/// Publishes one signed note per group when an article is stored.
#[derive(Clone)]
pub struct GossipHub {
    endpoint: Endpoint,
    gossip: Gossip,
    config: Arc<tokio::sync::RwLock<Config>>,
    topics: Arc<AsyncMutex<HashMap<String, GossipSender>>>,
    rate: Arc<Mutex<Rate>>,
}

struct Rate {
    start: Instant,
    count: u32,
}

impl GossipHub {
    /// Subscribe to every group that already exists so clients can join before
    /// the next post.
    ///
    /// # Errors
    ///
    /// Returns an error when listing groups fails. Individual subscribe
    /// failures are logged and skipped.
    pub async fn prime(self: &Arc<Self>, storage: &DynStorage) -> anyhow::Result<()> {
        use futures_util::StreamExt;
        let mut groups = storage.list_groups();
        while let Some(group) = groups.next().await {
            let group = group?;
            if let Err(err) = self.ensure_group(&group).await {
                tracing::warn!(group = %group, error = %err, "gossip subscribe failed");
            }
        }
        Ok(())
    }

    async fn enabled(&self) -> bool {
        self.config.read().await.iroh.gossip.enable
    }

    fn allow(&self) -> bool {
        let mut rate = self.rate.lock().unwrap_or_else(|err| err.into_inner());
        let now = Instant::now();
        if now.duration_since(rate.start) >= Duration::from_secs(60) {
            rate.start = now;
            rate.count = 0;
        }
        if rate.count >= MAX_NOTES_PER_MINUTE {
            return false;
        }
        rate.count += 1;
        true
    }

    async fn ensure_group(&self, group: &str) -> anyhow::Result<GossipSender> {
        let mut topics = self.topics.lock().await;
        if let Some(sender) = topics.get(group) {
            return Ok(sender.clone());
        }
        let topic = topic_id(&self.endpoint.id(), group);
        let joined = self
            .gossip
            .subscribe(topic, Vec::new())
            .await
            .map_err(|err| anyhow::anyhow!("gossip subscribe {group}: {err:#}"))?;
        let (sender, receiver) = joined.split();
        tokio::spawn(drain(receiver));
        topics.insert(group.to_string(), sender.clone());
        Ok(sender)
    }

    async fn send(&self, note: ArticleNote) {
        if !self.allow() {
            tracing::warn!(group = %note.group, "dropping notification; rate limit");
            return;
        }
        if !self.enabled().await {
            return;
        }
        let sender = match self.ensure_group(&note.group).await {
            Ok(sender) => sender,
            Err(err) => {
                tracing::warn!(error = %err, "notification not sent");
                return;
            }
        };
        let bytes = match signed_note(&self.endpoint, &note) {
            Ok(bytes) => bytes,
            Err(err) => {
                tracing::warn!(error = %err, "notification not signed");
                return;
            }
        };
        if let Err(err) = sender.broadcast(bytes::Bytes::from(bytes)).await {
            tracing::warn!(error = %err, group = %note.group, "gossip broadcast failed");
        }
    }
}

impl ArticleFanout for GossipHub {
    fn publish(&self, notes: Vec<ArticleNote>) {
        let hub = self.clone();
        tokio::spawn(async move {
            for note in notes {
                hub.send(note).await;
            }
        });
    }
}

async fn drain(mut receiver: GossipReceiver) {
    use futures_util::StreamExt;
    while receiver.next().await.is_some() {}
}

fn signed_note(endpoint: &Endpoint, note: &ArticleNote) -> anyhow::Result<Vec<u8>> {
    let signature = endpoint.secret_key().sign(&signed_bytes(note));
    let body = serde_json::json!({
        "group": note.group,
        "number": note.number,
        "message_id": note.message_id,
        "subject": note.subject,
        "from": note.from,
        "date": note.date,
        "sig": hex_encode(&signature.to_bytes()),
    });
    let bytes = serde_json::to_vec(&body)?;
    if bytes.len() > MAX_NOTE_BYTES {
        anyhow::bail!("notification is {} bytes", bytes.len());
    }
    Ok(bytes)
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

/// Build the gossip handler when `iroh.gossip.enable` is set.
pub async fn start(
    endpoint: Endpoint,
    config: Arc<tokio::sync::RwLock<Config>>,
    runtime: crate::admin::RuntimeStore,
    storage: &DynStorage,
) -> Option<(GatedGossip, Arc<GossipHub>)> {
    if !config.read().await.iroh.gossip.enable {
        tracing::info!("iroh gossip notifications are disabled");
        return None;
    }
    let gossip = Gossip::builder()
        .max_message_size(MAX_NOTE_BYTES)
        .spawn(endpoint.clone());
    let gated = GatedGossip {
        inner: gossip.clone(),
        config: config.clone(),
        runtime,
    };
    let hub = Arc::new(GossipHub {
        endpoint,
        gossip,
        config,
        topics: Arc::new(AsyncMutex::new(HashMap::new())),
        rate: Arc::new(Mutex::new(Rate {
            start: Instant::now(),
            count: 0,
        })),
    });
    if let Err(err) = hub.prime(storage).await {
        tracing::warn!(error = %err, "failed to subscribe existing groups");
    }
    Some((gated, hub))
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    #[test]
    fn topic_id_depends_on_the_server_and_the_group() {
        let key = SecretKey::from_bytes(&[0x11; 32]);
        let id = key.public();
        let general = topic_id(&id, "bots.general");
        let announce = topic_id(&id, "bots.announce");
        assert_ne!(general, announce);
        assert_eq!(general, topic_id(&id, "bots.general"));
    }

    #[test]
    fn signature_round_trip_uses_the_endpoint_id() {
        let secret = SecretKey::generate();
        let note = ArticleNote {
            group: "bots.general".into(),
            number: 3,
            message_id: "<a@bots.local>".into(),
            subject: "hello".into(),
            from: "bot@bots.local".into(),
            date: "Sun, 1 Nov 2026 00:00:00 +0000".into(),
        };
        let signature = secret.sign(&signed_bytes(&note));
        secret
            .public()
            .verify(&signed_bytes(&note), &signature)
            .unwrap();
        let other = SecretKey::generate();
        assert!(
            other
                .public()
                .verify(&signed_bytes(&note), &signature)
                .is_err()
        );
    }
}
