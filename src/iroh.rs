//! NNTP over [iroh](https://www.iroh.computer/).
//!
//! Each accepted QUIC bi-stream is handed to [`crate::handle_client`]. The
//! stream is treated as a secure connection, so `AUTHINFO` is allowed without
//! `allow_auth_insecure_connections`. Writes are flushed after every chunk:
//! iroh's send stream keeps bytes in a QUIC buffer until `poll_flush`, and the
//! NNTP handler does not flush most responses (including the greeting).

use std::io;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use std::time::Duration;

use anyhow::Context as _;
use iroh::endpoint::{Accepting, Connection, RelayMode, presets};
use iroh::protocol::{AcceptError, ProtocolHandler, Router};
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use iroh_tickets::Ticket;
use iroh_tickets::endpoint::EndpointTicket;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::RwLock;

use crate::auth::DynAuth;
use crate::config::Config;
use crate::limits::UsageTracker;
use crate::queue::ArticleQueue;
use crate::storage::DynStorage;

/// ALPN for the NNTP-over-iroh protocol.
pub const ALPN: &[u8] = b"/onix/nntp/1";

/// Shared server pieces the iroh listener needs.
pub struct IrohParts {
    pub storage: DynStorage,
    pub auth: DynAuth,
    pub config: Arc<RwLock<Config>>,
    pub queue: ArticleQueue,
    pub usage_tracker: Arc<UsageTracker>,
}

/// How to bind the iroh endpoint.
pub struct IrohBind {
    pub secret_key: SecretKey,
    pub relay_mode: RelayMode,
    /// Wait briefly for an n0 relay so the printed ticket includes one.
    /// Tests on a single machine set this to false and pass direct addresses.
    pub wait_for_relay: bool,
    /// Where to store the latest ticket so `renews admin discovery-txt` can
    /// print a DNS TXT value without parsing the journal.
    pub ticket_path: Option<std::path::PathBuf>,
    /// Bind only this socket. `None` binds every interface.
    /// Tests pass `127.0.0.1:0` so the ticket contains a loopback address.
    pub bind_addr: Option<std::net::SocketAddr>,
    /// SQLite URI for runtime peers (`iroh-runtime.db`).
    pub runtime_db: String,
}

/// A running iroh listener.
pub struct IrohServer {
    router: Router,
    /// Stable endpoint id (the public half of the persisted secret key).
    pub endpoint_id: EndpointId,
    /// Canonical endpoint ticket, printed once at startup.
    pub ticket: String,
}

impl IrohServer {
    /// Endpoint address currently advertised by the listener.
    #[must_use]
    pub fn addr(&self) -> EndpointAddr {
        self.router.endpoint().addr()
    }

    /// Stop accepting connections and close the endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error if the router fails to shut down.
    pub async fn shutdown(self) -> anyhow::Result<()> {
        self.router
            .shutdown()
            .await
            .map_err(|err| anyhow::anyhow!("iroh router shutdown: {err}"))
    }
}

/// Load a 32-byte secret key, or create one and store it with mode `0600`.
///
/// # Errors
///
/// Returns an error if the file cannot be read, is the wrong size, or cannot
/// be created.
pub fn load_or_create_secret_key(path: &Path) -> anyhow::Result<SecretKey> {
    if path.exists() {
        let bytes = std::fs::read(path)
            .with_context(|| format!("reading iroh secret key {}", path.display()))?;
        let array: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            anyhow::anyhow!(
                "iroh secret key {} is {} bytes; expected 32",
                path.display(),
                bytes.len()
            )
        })?;
        return Ok(SecretKey::from_bytes(&array));
    }

    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }

    let key = SecretKey::generate();
    write_secret_key(path, &key.to_bytes())?;
    Ok(key)
}

fn write_secret_key(path: &Path, bytes: &[u8; 32]) -> anyhow::Result<()> {
    let tmp = path.with_extension("tmp");
    {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&tmp)
            .with_context(|| format!("creating {}", tmp.display()))?;
        file.write_all(bytes)?;
    }
    std::fs::rename(&tmp, path)
        .with_context(|| format!("renaming {} to {}", tmp.display(), path.display()))?;
    Ok(())
}

/// Bind an endpoint and accept `/onix/nntp/1` bi-streams.
///
/// # Errors
///
/// Returns an error if the endpoint cannot bind.
pub async fn serve(parts: IrohParts, bind: IrohBind) -> anyhow::Result<IrohServer> {
    let mut builder = Endpoint::builder(presets::N0)
        .secret_key(bind.secret_key)
        .relay_mode(bind.relay_mode);
    if let Some(addr) = bind.bind_addr {
        builder = builder
            .bind_addr(addr)
            .map_err(|err| anyhow::anyhow!("iroh bind address {addr}: {err}"))?;
    }
    let endpoint = builder
        .bind()
        .await
        .map_err(|err| anyhow::anyhow!("bind iroh endpoint: {err}"))?;

    let runtime = crate::admin::RuntimeStore::open(&bind.runtime_db).await?;
    let live = Arc::new(crate::admin::LivePeers::default());
    let started = std::time::Instant::now();
    let endpoint_id_for_admin = endpoint.id().to_string();
    let ticket_slot = Arc::new(std::sync::RwLock::new(String::new()));
    let nntp = NntpProtocol {
        storage: parts.storage.clone(),
        auth: parts.auth.clone(),
        config: parts.config.clone(),
        queue: parts.queue,
        usage_tracker: parts.usage_tracker,
        runtime: runtime.clone(),
        live: live.clone(),
    };
    let admin = crate::admin::AdminProtocol {
        ctx: crate::admin::AdminContext {
            storage: parts.storage,
            auth: parts.auth,
            config: parts.config,
            runtime,
            live,
            started,
            endpoint_id: endpoint_id_for_admin,
            ticket: ticket_slot.clone(),
        },
    };
    let router = Router::builder(endpoint)
        .accept(ALPN, nntp)
        .accept(crate::admin::ADMIN_ALPN, admin)
        .spawn();

    // Direct addresses show up shortly after bind. Include them in the ticket
    // even when a relay is not reachable.
    let _ = wait_for_ip(router.endpoint(), Duration::from_secs(2)).await;

    if bind.wait_for_relay {
        match tokio::time::timeout(Duration::from_secs(15), router.endpoint().online()).await {
            Ok(()) => tracing::info!("iroh relay connected"),
            Err(_) => tracing::warn!(
                "iroh relay not reached within 15s; ticket may contain only direct addresses"
            ),
        }
    }

    let addr = router.endpoint().addr();
    let endpoint_id = router.endpoint().id();
    let ticket = EndpointTicket::new(addr).encode_string();
    *ticket_slot.write().unwrap_or_else(|err| err.into_inner()) = ticket.clone();
    if let Some(path) = &bind.ticket_path {
        write_ticket_file(path, &ticket)?;
    }
    println!("iroh endpoint id: {endpoint_id}");
    println!("iroh ticket: {ticket}");
    tracing::info!(%endpoint_id, %ticket, "iroh listener ready");

    Ok(IrohServer {
        router,
        endpoint_id,
        ticket,
    })
}

fn write_ticket_file(path: &std::path::Path, ticket: &str) -> anyhow::Result<()> {
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, ticket).with_context(|| format!("writing ticket {}", path.display()))?;
    Ok(())
}

/// Path of the ticket file next to an iroh secret key (`iroh.key` -> `iroh.ticket`).
#[must_use]
pub fn ticket_path_for_key(key_path: &std::path::Path) -> std::path::PathBuf {
    key_path.with_extension("ticket")
}

async fn wait_for_ip(endpoint: &Endpoint, budget: Duration) -> EndpointAddr {
    let start = std::time::Instant::now();
    loop {
        let addr = endpoint.addr();
        if addr.addrs.iter().any(iroh::TransportAddr::is_ip) || start.elapsed() >= budget {
            return addr;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct NntpProtocol {
    storage: DynStorage,
    auth: DynAuth,
    config: Arc<RwLock<Config>>,
    queue: ArticleQueue,
    usage_tracker: Arc<UsageTracker>,
    runtime: crate::admin::RuntimeStore,
    live: Arc<crate::admin::LivePeers>,
}

impl std::fmt::Debug for NntpProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NntpProtocol").finish_non_exhaustive()
    }
}

impl NntpProtocol {
    async fn peer_allowed(&self, remote: EndpointId) -> bool {
        let cfg = self.config.read().await;
        match crate::admin::nntp_peer_allowed(&cfg, &self.runtime, remote).await {
            Ok(allowed) => allowed,
            Err(err) => {
                tracing::warn!(error = %err, "iroh runtime allowlist lookup failed");
                false
            }
        }
    }
}

impl ProtocolHandler for NntpProtocol {
    async fn on_accepting(&self, accepting: Accepting) -> Result<Connection, AcceptError> {
        let connection = accepting.await?;
        let remote = connection.remote_id();
        if !self.peer_allowed(remote).await {
            tracing::warn!(endpoint_id = %remote, "rejected iroh peer not on the allowlist");
            connection.close(1u32.into(), b"endpoint not allowed");
            return Err(AcceptError::from_err(PeerRejected));
        }
        Ok(connection)
    }

    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();
        if !self.peer_allowed(remote).await {
            tracing::warn!(endpoint_id = %remote, "rejected iroh peer not on the allowlist");
            connection.close(1u32.into(), b"endpoint not allowed");
            return Err(AcceptError::from_err(PeerRejected));
        }
        tracing::info!(endpoint_id = %remote, "accepted iroh peer");
        let caller = remote.to_string();
        self.live.nntp.insert(caller.clone());
        let _session = crate::admin::LiveGuard {
            set: &self.live.nntp,
            id: caller,
        };

        // The server opens the stream. `accept_bi` does not resolve until the
        // peer has sent bytes, but NNTP requires the server to send the
        // greeting first. Opening here lets that greeting be the first bytes
        // on the stream. `NntpBiStream` flushes each write; noq's `poll_flush`
        // is currently a no-op, and `poll_write` wakes the connection so the
        // bytes leave the QUIC buffer.
        let (send, recv) = connection.open_bi().await.map_err(AcceptError::from_err)?;
        let stream = NntpBiStream::new(recv, send);

        let storage = self.storage.clone();
        let auth = self.auth.clone();
        let config = self.config.clone();
        let queue = self.queue.clone();
        let usage_tracker = self.usage_tracker.clone();

        if let Err(err) =
            crate::handle_client(stream, storage, auth, config, true, queue, usage_tracker).await
        {
            tracing::debug!(error = %err, "iroh nntp session ended");
        }
        // Dropping the connection sends CONNECTION_CLOSE immediately and can
        // discard the final 205 response. Wait until the peer goes away, or a
        // short grace period, so that response is delivered first.
        let _ = tokio::time::timeout(Duration::from_secs(2), connection.closed()).await;
        Ok(())
    }
}

/// Bi-stream whose writes flush before they are reported complete.
struct NntpBiStream<R, W> {
    recv: R,
    send: Flushing<W>,
}

impl<R, W> NntpBiStream<R, W> {
    fn new(recv: R, send: W) -> Self {
        Self {
            recv,
            send: Flushing::new(send),
        }
    }
}

impl<R, W> AsyncRead for NntpBiStream<R, W>
where
    R: AsyncRead + Unpin,
    W: Unpin,
{
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().recv).poll_read(cx, buf)
    }
}

impl<R, W> AsyncWrite for NntpBiStream<R, W>
where
    R: Unpin,
    W: AsyncWrite + Unpin,
{
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.get_mut().send).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().send).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.get_mut().send).poll_shutdown(cx)
    }
}

/// Calls `poll_flush` before a write is considered finished.
///
/// `poll_write` on a QUIC send stream only copies into a buffer. Callers such
/// as `write_all` then move on to the next read, so the peer never sees the
/// bytes unless something flushes. If the flush itself is pending, the written
/// length is remembered and the next poll finishes the flush without writing
/// the same bytes again.
struct Flushing<W> {
    inner: W,
    pending: Option<usize>,
}

impl<W> Flushing<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            pending: None,
        }
    }
}

impl<W: AsyncWrite + Unpin> AsyncWrite for Flushing<W> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Some(written) = this.pending {
            match Pin::new(&mut this.inner).poll_flush(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Err(err)) => {
                    this.pending = None;
                    return Poll::Ready(Err(err));
                }
                Poll::Ready(Ok(())) => {
                    this.pending = None;
                    return Poll::Ready(Ok(written));
                }
            }
        }

        let written = ready!(Pin::new(&mut this.inner).poll_write(cx, buf))?;
        if written == 0 {
            return Poll::Ready(Ok(0));
        }
        match Pin::new(&mut this.inner).poll_flush(cx) {
            Poll::Ready(Ok(())) => Poll::Ready(Ok(written)),
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => {
                this.pending = Some(written);
                Poll::Pending
            }
        }
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if this.pending.is_some() {
            ready!(Pin::new(&mut this.inner).poll_flush(cx))?;
            this.pending = None;
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        ready!(self.as_mut().poll_flush(cx))?;
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

/// Returned from the accept path when the remote endpoint id is not allowlisted.
#[derive(Debug)]
struct PeerRejected;

impl std::fmt::Display for PeerRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("endpoint not allowed")
    }
}

impl std::error::Error for PeerRejected {}
