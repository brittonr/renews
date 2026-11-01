//! Live new-article notifications over iroh-gossip.
//!
//! The server signs each header with its endpoint key. `delivered_from` is
//! only the neighbor that forwarded the bytes, so a note is accepted when
//! that neighbor is the server and the signature verifies with the server id.
//! Anything else is ignored. If the swarm cannot be joined, callers poll.

use std::time::Duration;

use anyhow::Context as _;
use futures_util::StreamExt;
use iroh::address_lookup::memory::MemoryLookup;
use iroh::endpoint::{Connection, RelayMode, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointId, Signature};
use iroh_gossip::api::Message;
use iroh_gossip::{Gossip, TopicId};
use serde::Deserialize;
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::mpsc;

use crate::{ALPN, BoxStream, ReadRequest, Session, Settings, discover};

/// Header fields from a signed notification. The body is not included.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Notification {
    pub group: String,
    pub number: u64,
    pub message_id: String,
    pub subject: String,
    pub from: String,
    pub date: String,
}

/// A joined gossip subscription plus the NNTP session used to fetch bodies.
pub struct WatchSession {
    session: Session,
    notes: mpsc::Receiver<Notification>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl WatchSession {
    /// Join `groups` with the server as the only bootstrap peer.
    ///
    /// # Errors
    ///
    /// Returns an error when the server cannot be dialed or no neighbor
    /// appears before `join_timeout`.
    pub async fn open(
        settings: Settings,
        groups: &[String],
        join_timeout: Duration,
    ) -> anyhow::Result<Self> {
        if groups.is_empty() {
            anyhow::bail!("watch needs at least one group");
        }
        let mut builder = Endpoint::builder(presets::N0).secret_key(settings.secret_key.clone());
        if !settings.relays {
            builder = builder
                .relay_mode(RelayMode::Disabled)
                .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
                .map_err(|err| anyhow::anyhow!("iroh client bind address: {err}"))?;
        }
        let endpoint = builder
            .bind()
            .await
            .map_err(|err| anyhow::anyhow!("bind iroh endpoint: {err}"))?;
        let addr = discover(&settings.server, &settings.state_dir, settings.discovery)
            .await?
            .addr;
        let server_id = addr.id;
        let lookup = MemoryLookup::new();
        lookup.add_endpoint_info(addr.clone());
        endpoint
            .address_lookup()
            .map_err(|err| anyhow::anyhow!("address lookup: {err}"))?
            .add(lookup);

        let gossip = Gossip::builder()
            .max_message_size(2048)
            .spawn(endpoint.clone());
        let router = Router::builder(endpoint.clone())
            .accept(
                iroh_gossip::ALPN,
                ServerOnly {
                    inner: gossip.clone(),
                    server: server_id,
                },
            )
            .spawn();

        let (tx, rx) = mpsc::channel(32);
        let mut tasks = Vec::new();
        for group in groups {
            let mut topic = gossip
                .subscribe(topic_id(&server_id, group), vec![server_id])
                .await
                .map_err(|err| anyhow::anyhow!("gossip subscribe {group}: {err:#}"))?;
            tokio::time::timeout(join_timeout, topic.joined())
                .await
                .map_err(|_| anyhow::anyhow!("timed out joining gossip for {group}"))?
                .map_err(|err| anyhow::anyhow!("gossip join {group}: {err:#}"))?;
            let (sender, mut receiver) = topic.split();
            let tx = tx.clone();
            let expect = group.clone();
            tasks.push(tokio::spawn(async move {
                let _keep = sender;
                while let Some(event) = receiver.next().await {
                    let Ok(event) = event else { continue };
                    let iroh_gossip::api::Event::Received(message) = event else {
                        continue;
                    };
                    if let Some(note) = accept_note(server_id, &expect, &message)
                        && tx.send(note).await.is_err()
                    {
                        break;
                    }
                }
            }));
        }
        drop(tx);

        let session = dial_nntp(endpoint, router, addr, &settings).await?;
        Ok(Self {
            session,
            notes: rx,
            tasks,
        })
    }

    /// Next accepted notification. `None` when every subscription has ended.
    pub async fn recv(&mut self) -> Option<Notification> {
        self.notes.recv().await
    }

    pub fn session(&self) -> &Session {
        &self.session
    }

    pub fn session_mut(&mut self) -> &mut Session {
        &mut self.session
    }

    /// Quit NNTP and shut the endpoint down.
    ///
    /// # Errors
    ///
    /// Returns an error when `QUIT` is not acknowledged.
    pub async fn close(mut self) -> anyhow::Result<()> {
        for task in self.tasks.drain(..) {
            task.abort();
        }
        self.session.close().await
    }
}

/// Print articles as they are announced.
///
/// Gossip is tried first. A join failure falls back to polling `interval`.
///
/// # Errors
///
/// Returns an error when NNTP fails or `interval` is zero in the poll path.
pub async fn run_watch(
    settings: Settings,
    groups: &[String],
    interval: u64,
    notify_only: bool,
) -> anyhow::Result<()> {
    if groups.is_empty() {
        anyhow::bail!("watch needs at least one group");
    }
    match WatchSession::open(settings.clone(), groups, Duration::from_secs(4)).await {
        Ok(mut watch) => {
            if !notify_only {
                for group in groups {
                    let since = watch.session().cursor(group)?;
                    let articles = watch
                        .session_mut()
                        .read(ReadRequest {
                            group: group.clone(),
                            since,
                            limit: None,
                            update_cursor: true,
                        })
                        .await?;
                    for article in articles {
                        println!("{}", serde_json::to_string(&article)?);
                    }
                }
            }
            eprintln!("watching {} via gossip", groups.join(" "));
            loop {
                let Some(note) = watch.recv().await else {
                    break;
                };
                if notify_only {
                    println!("{}", serde_json::to_string(&note)?);
                    continue;
                }
                let since = watch.session().cursor(&note.group)?;
                if since.is_some_and(|seen| note.number <= seen) {
                    continue;
                }
                let articles = watch
                    .session_mut()
                    .read(ReadRequest {
                        group: note.group.clone(),
                        since,
                        limit: None,
                        update_cursor: true,
                    })
                    .await?;
                for article in articles {
                    println!("{}", serde_json::to_string(&article)?);
                }
            }
            watch.close().await
        }
        Err(err) => {
            eprintln!("gossip unavailable ({err:#}); polling every {interval}s");
            if interval == 0 {
                anyhow::bail!("--interval must be at least 1 second");
            }
            let mut session = Session::connect(settings).await?;
            loop {
                for group in groups {
                    let since = session.cursor(group)?;
                    let articles = session
                        .read(ReadRequest {
                            group: group.clone(),
                            since,
                            limit: None,
                            update_cursor: true,
                        })
                        .await?;
                    for article in articles {
                        println!("{}", serde_json::to_string(&article)?);
                    }
                }
                tokio::time::sleep(Duration::from_secs(interval)).await;
            }
        }
    }
}

/// Block until one signed notification arrives, or `timeout_secs` elapses.
///
/// The result is headers only. An empty vec means the timeout fired.
///
/// # Errors
///
/// Returns an error when the server cannot be dialed at all.
pub async fn wait_for_new(
    settings: Settings,
    groups: &[String],
    timeout_secs: u64,
) -> anyhow::Result<Vec<Notification>> {
    if groups.is_empty() {
        anyhow::bail!("wait_for_new needs at least one group");
    }
    if timeout_secs == 0 || timeout_secs > 300 {
        anyhow::bail!("timeout_secs must be from 1 to 300");
    }
    let budget = Duration::from_secs(timeout_secs);
    let started = std::time::Instant::now();
    if let Ok(mut watch) =
        WatchSession::open(settings.clone(), groups, budget.min(Duration::from_secs(4))).await
    {
        let left = budget.saturating_sub(started.elapsed());
        let note = tokio::time::timeout(left, watch.recv())
            .await
            .ok()
            .flatten();
        let _ = watch.close().await;
        return Ok(note.into_iter().collect());
    }
    let mut session = Session::connect(settings).await?;
    let mut high = Vec::with_capacity(groups.len());
    for group in groups {
        let status = session.client.group(group).await?;
        high.push(status.high);
    }
    loop {
        if started.elapsed() >= budget {
            session.close().await?;
            return Ok(Vec::new());
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
        for (group, seen) in groups.iter().zip(high.iter()) {
            let status = session.client.group(group).await?;
            if status.high > *seen && status.low > 0 {
                let articles = session
                    .read(ReadRequest {
                        group: group.clone(),
                        since: Some(*seen),
                        limit: Some(1),
                        update_cursor: false,
                    })
                    .await?;
                session.close().await?;
                return Ok(articles
                    .into_iter()
                    .map(|article| Notification {
                        group: group.clone(),
                        number: article.number,
                        message_id: article.message_id,
                        subject: article.subject,
                        from: article.from,
                        date: article.date,
                    })
                    .collect());
            }
        }
    }
}

fn topic_id(endpoint_id: &EndpointId, group: &str) -> TopicId {
    let mut hasher = Sha256::new();
    hasher.update(endpoint_id.as_bytes());
    hasher.update(b"\0");
    hasher.update(group.as_bytes());
    let bytes: [u8; 32] = hasher.finalize().into();
    TopicId::from_bytes(bytes)
}

fn signed_bytes(note: &Notification) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"onix-nntp-notify/1\0");
    push_field(&mut out, &note.group);
    out.extend_from_slice(&note.number.to_be_bytes());
    out.push(0);
    push_field(&mut out, &note.message_id);
    push_field(&mut out, &note.subject);
    push_field(&mut out, &note.from);
    push_field(&mut out, &note.date);
    out
}

fn push_field(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(value.as_bytes());
    out.push(0);
}

#[derive(Deserialize)]
struct SignedNote {
    group: String,
    number: u64,
    message_id: String,
    subject: String,
    from: String,
    date: String,
    sig: String,
}

fn accept_note(server: EndpointId, group: &str, message: &Message) -> Option<Notification> {
    if message.delivered_from != server || message.content.len() > 2048 {
        return None;
    }
    let parsed: SignedNote = serde_json::from_slice(&message.content).ok()?;
    if parsed.group != group {
        return None;
    }
    let sig_bytes = decode_hex(&parsed.sig)?;
    let signature = Signature::from_bytes(&sig_bytes);
    let note = Notification {
        group: parsed.group,
        number: parsed.number,
        message_id: parsed.message_id,
        subject: parsed.subject,
        from: parsed.from,
        date: parsed.date,
    };
    server.verify(&signed_bytes(&note), &signature).ok()?;
    Some(note)
}

fn decode_hex(text: &str) -> Option<[u8; 64]> {
    if text.len() != 128 {
        return None;
    }
    let mut out = [0u8; 64];
    let bytes = text.as_bytes();
    for (index, chunk) in out.iter_mut().enumerate() {
        let hi = hex_nibble(bytes[index * 2])?;
        let lo = hex_nibble(bytes[index * 2 + 1])?;
        *chunk = (hi << 4) | lo;
    }
    Some(out)
}

fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

#[derive(Clone)]
struct ServerOnly {
    inner: Gossip,
    server: EndpointId,
}

impl std::fmt::Debug for ServerOnly {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ServerOnly").finish_non_exhaustive()
    }
}

#[derive(Debug)]
struct Rejected;

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("endpoint not allowed")
    }
}

impl std::error::Error for Rejected {}

impl ProtocolHandler for ServerOnly {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if connection.remote_id() != self.server {
            connection.close(1u32.into(), b"endpoint not allowed");
            return Err(AcceptError::from_err(Rejected));
        }
        self.inner.accept(connection).await
    }
}

async fn dial_nntp(
    endpoint: Endpoint,
    router: Router,
    addr: iroh::EndpointAddr,
    settings: &Settings,
) -> anyhow::Result<Session> {
    let connection = endpoint
        .connect(addr, ALPN)
        .await
        .map_err(|err| anyhow::anyhow!("iroh connect: {err}"))?;
    let (send, recv) = connection
        .accept_bi()
        .await
        .map_err(|err| anyhow::anyhow!("accept NNTP stream: {err}"))?;
    let stream = BoxStream::new(crate::BiStream {
        recv,
        send: crate::Flushing::new(send),
    });
    let mut client = nntp_client::NntpClient::connect(stream)
        .await
        .context("reading NNTP greeting")?;
    if !settings.password.is_empty() {
        client
            .authinfo(&settings.username, &settings.password)
            .await
            .context("AUTHINFO")?;
    }
    Ok(Session {
        client,
        endpoint,
        _connection: connection,
        state_dir: settings.state_dir.clone(),
        router: Some(router),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    #[test]
    fn topic_and_signature_bytes_match_the_server() {
        let secret = SecretKey::from_bytes(&[0x11; 32]);
        let id = secret.public();
        assert_eq!(
            topic_id(&id, "bots.general"),
            renews::gossip::topic_id(&id, "bots.general")
        );
        let note = Notification {
            group: "bots.general".into(),
            number: 3,
            message_id: "<a@bots.local>".into(),
            subject: "hello".into(),
            from: "bot@bots.local".into(),
            date: "Sun, 1 Nov 2026 00:00:00 +0000".into(),
        };
        let server_note = renews::notify::ArticleNote {
            group: note.group.clone(),
            number: note.number,
            message_id: note.message_id.clone(),
            subject: note.subject.clone(),
            from: note.from.clone(),
            date: note.date.clone(),
        };
        assert_eq!(
            signed_bytes(&note),
            renews::notify::signed_bytes(&server_note)
        );
    }
}
