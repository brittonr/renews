//! Library behind the `iroh-news` CLI.
//!
//! Bots dial a renews server by endpoint ticket (or bare endpoint id, resolved
//! through the default n0 relays) and speak NNTP on one QUIC bi-stream.
//! [`snapshot`] writes a read-only JSON view of groups and reply threads.
//! [`render`] turns that file into a static HTML site.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll, ready};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context as _, bail};
use iroh::endpoint::{Connection, PortmapperConfig, RelayMode, presets};
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointId, SecretKey};
use nntp_client::NntpClient;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

mod admin_client;
pub mod credentials;
mod discover;
pub mod mcp;
pub mod render;
pub mod snapshot;
mod watch;

pub use admin_client::{ADMIN_ALPN, JOIN_ALPN, admin_call, join_request};
pub use credentials::{CredentialStore, KeySource, SecretSpecSetting};

pub use discover::{
    DiscoveryMode, DiscoveryRecord, DiscoverySource, ResolvedEndpoint, cloudflare_txt_line,
    discover, discovery_qname, parse_txt_records,
};
pub use nntp_client::{Article, GroupEntry, Overview};
pub use watch::{Notification, WatchSession, run_watch, wait_for_new};

/// ALPN shared with the renews iroh listener.
pub const ALPN: &[u8] = b"/onix/nntp/1";

/// Bind a client endpoint. With relays it uses the n0 relays and address
/// lookup. Without, it uses neither and publishes nothing, so it can only dial
/// the direct addresses in a server ticket, such as a tailnet address.
pub(crate) async fn bind_client_endpoint(
    secret_key: SecretKey,
    relays: bool,
) -> anyhow::Result<Endpoint> {
    let builder = if relays {
        Endpoint::builder(presets::N0)
    } else {
        Endpoint::builder(presets::Minimal)
            .relay_mode(RelayMode::Disabled)
            .portmapper_config(PortmapperConfig::Disabled)
    };
    builder
        .secret_key(secret_key)
        .bind()
        .await
        .map_err(|err| anyhow::anyhow!("bind iroh endpoint: {err}"))
}

/// Resolve `server` to the address to dial. Without relays nothing can look up
/// a bare endpoint id, so the server must come with direct addresses. With
/// `server_id`, the address must be that endpoint's.
pub(crate) async fn dial_addr(
    server: &str,
    state_dir: &Path,
    discovery: DiscoveryMode,
    relays: bool,
    server_id: Option<EndpointId>,
) -> anyhow::Result<iroh::EndpointAddr> {
    let addr = discover::discover(server, state_dir, discovery, server_id)
        .await?
        .addr;
    if !relays && addr.ip_addrs().next().is_none() {
        bail!(
            "{server} gives no direct address to dial without relays; use an endpoint \
             ticket, or publish ticket= instead of id= in its _iroh-news TXT record"
        );
    }
    Ok(addr)
}

/// What a bot needs in order to dial and authenticate.
#[derive(Clone)]
pub struct Settings {
    /// Endpoint ticket, endpoint id, DNS name, or `<endpoint id>@<host>:<port>`.
    pub server: String,
    pub username: String,
    pub password: String,
    /// `From` header used by [`post_article`].
    pub from: String,
    pub secret_key: SecretKey,
    /// Use the n0 relays and address lookup. Off, only the direct addresses
    /// in the server's ticket are dialed (a tailnet, or one machine in tests).
    pub relays: bool,
    pub state_dir: PathBuf,
    /// How a DNS server name is resolved. Tickets and endpoint ids skip DNS.
    pub discovery: DiscoveryMode,
    /// Pinned server endpoint id. Whatever `server` resolves to must carry it.
    pub server_id: Option<EndpointId>,
}

/// A connected, authenticated NNTP session.
pub struct Session {
    client: NntpClient<tokio::io::ReadHalf<BoxStream>, tokio::io::WriteHalf<BoxStream>>,
    endpoint: Endpoint,
    /// Kept alive so the QUIC connection is not dropped with the streams.
    _connection: Connection,
    state_dir: PathBuf,
    /// Present when this session also serves iroh-gossip on the same endpoint.
    router: Option<Router>,
}

/// Arguments for [`Session::read`].
pub struct ReadRequest {
    pub group: String,
    /// Return articles with numbers strictly greater than this.
    pub since: Option<u64>,
    pub limit: Option<usize>,
    /// Persist the highest returned article number as this group's cursor.
    pub update_cursor: bool,
}

/// Arguments for [`post_article`].
pub struct PostRequest<'a> {
    pub group: &'a str,
    pub subject: &'a str,
    pub references: Option<&'a str>,
    pub from: &'a str,
    /// `Message-ID` to send, `<...>` included. `None` makes [`Session::post`]
    /// generate one and return it.
    pub message_id: Option<&'a str>,
    pub body: &'a str,
}

impl Session {
    /// Dial `settings.server`, read the greeting, and authenticate.
    ///
    /// # Errors
    ///
    /// Returns an error if the peer cannot be reached, the allowlist rejects
    /// us, or `AUTHINFO` fails.
    pub async fn connect(settings: Settings) -> anyhow::Result<Self> {
        let endpoint = bind_client_endpoint(settings.secret_key, settings.relays).await?;
        let addr = dial_addr(
            &settings.server,
            &settings.state_dir,
            settings.discovery,
            settings.relays,
            settings.server_id,
        )
        .await?;
        let connection = endpoint
            .connect(addr, ALPN)
            .await
            .map_err(|err| anyhow::anyhow!("iroh connect: {err}"))?;
        // The server opens the bi-stream and writes the greeting. Accepting
        // waits until those bytes arrive, which is what unblocks this read.
        let (send, recv) = connection
            .accept_bi()
            .await
            .map_err(|err| anyhow::anyhow!("accept NNTP stream: {err}"))?;
        let stream = BoxStream::new(BiStream {
            recv,
            send: Flushing::new(send),
        });
        let mut client = NntpClient::connect(stream)
            .await
            .context("reading NNTP greeting")?;
        if !settings.password.is_empty() {
            client
                .authinfo(&settings.username, &settings.password)
                .await
                .context("AUTHINFO")?;
        }

        Ok(Self {
            client,
            endpoint,
            _connection: connection,
            state_dir: settings.state_dir,
            router: None,
        })
    }

    /// Measure the round trip to the server's endpoint with the iroh-ping
    /// protocol. Opens no NNTP session and sends no credentials, so it also
    /// works before an endpoint is on the allowlist.
    ///
    /// # Errors
    ///
    /// Returns an error if the endpoint cannot be resolved or the server does
    /// not answer within the ping timeout.
    pub async fn ping(settings: Settings) -> anyhow::Result<std::time::Duration> {
        let endpoint = bind_client_endpoint(settings.secret_key, settings.relays).await?;
        let addr = dial_addr(
            &settings.server,
            &settings.state_dir,
            settings.discovery,
            settings.relays,
            settings.server_id,
        )
        .await?;
        let rtt = iroh_ping::Ping::new()
            .ping(&endpoint, addr)
            .await
            .map_err(|err| anyhow::anyhow!("iroh ping: {err}"))?;
        endpoint.close().await;
        Ok(rtt)
    }

    /// `LIST ACTIVE`.
    ///
    /// # Errors
    ///
    /// Returns an error if the server rejects the command.
    pub async fn groups(&mut self) -> anyhow::Result<Vec<GroupEntry>> {
        let mut groups = self.client.list_active().await?;
        if let Ok(catalog) = self.client.list_bot_groups().await {
            for group in &mut groups {
                if let Some((_, description, creator)) =
                    catalog.iter().find(|(name, _, _)| name == &group.name)
                {
                    group.description.clone_from(description);
                    group.creator.clone_from(creator);
                }
            }
        }
        Ok(groups)
    }

    /// Create a group. The account must already be authenticated.
    ///
    /// # Errors
    ///
    /// Returns an error if the server rejects the name or the rate limit.
    pub async fn create_group(&mut self, name: &str, description: &str) -> anyhow::Result<()> {
        self.client.create_group(name, description).await
    }

    /// Read articles from `request.group`.
    ///
    /// # Errors
    ///
    /// Returns an error if the group is missing or an article cannot be read.
    pub async fn read(&mut self, request: ReadRequest) -> anyhow::Result<Vec<Article>> {
        let status = self.client.group(&request.group).await?;
        if status.count == 0 || status.high == 0 || status.low == 0 {
            return Ok(Vec::new());
        }
        let start = request
            .since
            .map(|n| n.saturating_add(1))
            .unwrap_or(status.low)
            .max(status.low);
        if start > status.high {
            return Ok(Vec::new());
        }
        let range = format!("{start}-{}", status.high);
        let mut overview = self.client.over(Some(&range)).await?;
        if let Some(limit) = request.limit {
            overview.truncate(limit);
        }

        let mut articles = Vec::with_capacity(overview.len());
        for item in &overview {
            articles.push(self.client.article(&item.number.to_string()).await?);
        }
        if request.update_cursor
            && let Some(last) = articles.last()
        {
            write_cursor(&self.state_dir, &request.group, last.number)?;
        }
        Ok(articles)
    }

    /// Read the newest `last` articles from `group`, in ascending article order.
    ///
    /// Does not modify any saved cursor.
    ///
    /// # Errors
    ///
    /// Returns an error if the group is missing or an article cannot be read.
    pub async fn read_last(&mut self, group: &str, last: usize) -> anyhow::Result<Vec<Article>> {
        if last == 0 {
            return Ok(Vec::new());
        }
        let status = self.client.group(group).await?;
        if status.count == 0 || status.high == 0 || status.low == 0 {
            return Ok(Vec::new());
        }
        let range = format!("{}-{}", status.low, status.high);
        let overview = self.client.over(Some(&range)).await?;
        let skip = overview.len().saturating_sub(last);
        let tail = &overview[skip..];
        let mut articles = Vec::with_capacity(tail.len());
        for item in tail {
            articles.push(self.client.article(&item.number.to_string()).await?);
        }
        Ok(articles)
    }

    /// `POST` an article. The body is dot-stuffed.
    ///
    /// Returns the article's `Message-ID`, `<...>` included: the one in
    /// `request.message_id`, or a generated one. Callers need it to reference
    /// the article in a later post without reading the group back.
    ///
    /// # Errors
    ///
    /// Returns an error if a header contains a newline or the server rejects the article.
    pub async fn post(&mut self, request: PostRequest<'_>) -> anyhow::Result<String> {
        let message_id = match request.message_id {
            Some(id) => id.to_string(),
            None => new_message_id(request.from),
        };
        let raw = format_article(
            request.from,
            request.group,
            request.subject,
            request.references,
            Some(&message_id),
            request.body,
        )?;
        self.client.post(&raw).await?;
        Ok(message_id)
    }

    /// Article number last consumed by `--new` / `watch` for `group`.
    ///
    /// # Errors
    ///
    /// Returns an error if the cursor file exists but is not a number.
    pub fn cursor(&self, group: &str) -> anyhow::Result<Option<u64>> {
        read_cursor(&self.state_dir, group)
    }

    /// Send `QUIT` and close the endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error if `QUIT` is not acknowledged. The endpoint is still closed.
    pub async fn close(mut self) -> anyhow::Result<()> {
        let quit = self.client.quit().await;
        if let Some(router) = self.router.take() {
            let _ = router.shutdown().await;
        }
        self.endpoint.close().await;
        quit
    }
}

/// Build a CRLF article terminated by `.\r\n`.
///
/// # Errors
///
/// Returns an error if a header value contains CR or LF.
/// A fresh `<...>` `Message-ID` for a post that names none.
///
/// The right-hand side is the domain of `from` when it has one, so a swarm
/// article reads `<...@swarm.local>` like the server's own ids do. The
/// counter separates posts that land in the same nanosecond.
pub fn new_message_id(from: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let domain = from
        .rsplit_once('@')
        .map(|(_, rest)| rest.trim_end_matches('>'))
        .filter(|domain| !domain.is_empty() && !domain.contains([' ', '<', '>']))
        .unwrap_or("iroh-news.local");
    format!("<{nanos:x}.{}.{seq:x}@{domain}>", std::process::id())
}

fn message_id_is_bracketed(id: &str) -> anyhow::Result<()> {
    let inner = id.strip_prefix('<').and_then(|rest| rest.strip_suffix('>'));
    match inner {
        Some(inner) if !inner.is_empty() && !inner.contains(['<', '>', ' ']) => Ok(()),
        _ => bail!("Message-ID must be <id@domain> without spaces"),
    }
}

pub fn format_article(
    from: &str,
    group: &str,
    subject: &str,
    references: Option<&str>,
    message_id: Option<&str>,
    body: &str,
) -> anyhow::Result<String> {
    for (name, value) in [("From", from), ("Newsgroups", group), ("Subject", subject)] {
        if value.contains(['\r', '\n']) {
            bail!("{name} contains a newline");
        }
    }
    for value in [references, message_id].into_iter().flatten() {
        if value.contains(['\r', '\n']) {
            bail!("References or Message-ID contains a newline");
        }
    }
    if let Some(message_id) = message_id {
        message_id_is_bracketed(message_id)?;
    }

    let mut out = String::new();
    out.push_str(&format!("From: {from}\r\n"));
    out.push_str(&format!("Newsgroups: {group}\r\n"));
    out.push_str(&format!("Subject: {subject}\r\n"));
    if let Some(message_id) = message_id {
        out.push_str(&format!("Message-ID: {message_id}\r\n"));
    }
    if let Some(references) = references {
        out.push_str(&format!("References: {references}\r\n"));
    }
    out.push_str("\r\n");
    for line in body_lines(body) {
        if line.starts_with('.') {
            out.push('.');
        }
        out.push_str(line);
        out.push_str("\r\n");
    }
    out.push_str(".\r\n");
    Ok(out)
}

fn body_lines(body: &str) -> Vec<&str> {
    if body.is_empty() {
        return Vec::new();
    }
    let trimmed = body.strip_suffix('\n').unwrap_or(body);
    trimmed
        .split('\n')
        .map(|line| line.strip_suffix('\r').unwrap_or(line))
        .collect()
}

/// JSON object printed by `iroh-news id`.
#[derive(Debug, Serialize)]
pub struct Identity {
    pub endpoint_id: String,
}

/// Default `server` written by `iroh-news join`.
pub const DEFAULT_JOIN_SERVER: &str = "slime.onix.computer";

/// Inputs for [`prepare_join`]. Nothing here is sent over the network.
pub struct JoinRequest<'a> {
    pub name: &'a str,
    pub owner: Option<&'a str>,
    pub server: &'a str,
    /// From [`CredentialStore::secret_key`], so the key's store is the
    /// caller's choice.
    pub secret_key: &'a SecretKey,
    pub config_path: &'a Path,
}

/// Local files and the paste block produced by `iroh-news join`.
#[derive(Debug, Clone, Serialize)]
pub struct JoinReport {
    pub username: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    pub endpoint_id: String,
    pub server: String,
    pub config_path: PathBuf,
    pub password_file: PathBuf,
    pub config_written: bool,
    /// `services.renews-iroh.allowedPeers."<name>" = "<endpoint id>";`
    pub nix_allowed_peers: String,
    /// `services.renews-iroh.bots."<name>".passwordFile = "/run/secrets/slime/<name>";`
    pub nix_bot: String,
    pub next: Vec<String>,
}

impl JoinReport {
    /// Human block a bot pastes to the operator. No password is included.
    pub fn human(&self) -> String {
        let owner = self.owner.as_deref().unwrap_or("(not given)");
        let config_note = if self.config_written {
            format!(
                "Wrote {} (no password in that file).\n",
                self.config_path.display()
            )
        } else {
            format!("Left existing {} unchanged.\n", self.config_path.display())
        };
        format!(
            "\
Join request.

username: {username}
owner: {owner}
endpoint_id: {endpoint_id}
server: {server}

{config_note}\
Next:
- Wait for Britton to approve the request.
- Then run: iroh-news groups
- Or poll with: iroh-news join --name {username} --wait
",
            username = self.username,
            endpoint_id = self.endpoint_id,
            server = self.server,
        )
    }
}

/// Check a bot username. The whole string must match `[a-z0-9-]`, and it
/// cannot start or end with a hyphen.
///
/// # Errors
///
/// Returns an error when the name is empty or contains any other character.
pub fn validate_username(name: &str) -> anyhow::Result<()> {
    let bytes = name.as_bytes();
    let ok = !bytes.is_empty()
        && bytes.len() <= 64
        && bytes
            .iter()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || *b == b'-')
        && !name.starts_with('-')
        && !name.ends_with('-');
    if !ok {
        bail!("username must match [a-z0-9-] (1-64 chars, no leading or trailing hyphen)");
    }
    Ok(())
}

/// Write a config stub, then build the operator paste block.
///
/// Does not open a network connection, does not overwrite an existing config,
/// and does not create the password file.
///
/// # Errors
///
/// Returns an error when the username, owner, or server is unusable, or when
/// the config file cannot be written.
pub fn prepare_join(request: JoinRequest<'_>) -> anyhow::Result<JoinReport> {
    validate_username(request.name)?;
    if let Some(owner) = request.owner
        && (owner.is_empty() || owner.chars().any(char::is_control))
    {
        bail!("--owner must be a single non-empty line");
    }
    if request.server.is_empty()
        || request
            .server
            .chars()
            .any(|c| c.is_control() || c == '"' || c == '\\')
    {
        bail!("--server must be a single line without quotes or backslashes");
    }

    let endpoint_id = request.secret_key.public().to_string();
    let config_dir = request
        .config_path
        .parent()
        .context("config path has no directory")?;
    ensure_private_dir(config_dir)?;
    let password_file = config_dir.join("password");
    let config_written = if request.config_path.exists() {
        false
    } else {
        let body = format!(
            "server = {}\nusername = {}\n",
            toml_basic_string(request.server),
            toml_basic_string(request.name),
        );
        write_private_new(request.config_path, body.as_bytes())?;
        true
    };

    let nix_allowed_peers = format!(
        "services.renews-iroh.allowedPeers.\"{}\" = \"{}\";",
        request.name, endpoint_id
    );
    let nix_bot = format!(
        "services.renews-iroh.bots.\"{}\".passwordFile = \"/run/secrets/slime/{}\";",
        request.name, request.name
    );
    let next = vec![
        "Wait for Britton to approve the request.".to_string(),
        "Run: iroh-news groups".to_string(),
    ];
    Ok(JoinReport {
        username: request.name.to_string(),
        owner: request.owner.map(str::to_string),
        endpoint_id,
        server: request.server.to_string(),
        config_path: request.config_path.to_path_buf(),
        password_file,
        config_written,
        nix_allowed_peers,
        nix_bot,
        next,
    })
}

fn toml_basic_string(value: &str) -> String {
    format!("\"{value}\"")
}

fn ensure_private_dir(path: &Path) -> anyhow::Result<()> {
    if path.exists() {
        if !path.is_dir() {
            bail!("{} exists and is not a directory", path.display());
        }
        return Ok(());
    }
    std::fs::create_dir_all(path).with_context(|| format!("creating {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .with_context(|| format!("setting mode on {}", path.display()))?;
    }
    Ok(())
}

fn write_private_new(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(path)
        .with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}

/// Load or create the bot's secret key in `state_dir/secret_key`.
///
/// # Errors
///
/// Returns an error if the key file cannot be read or created.
pub fn load_or_create_secret_key(state_dir: &Path) -> anyhow::Result<SecretKey> {
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    let path = state_dir.join("secret_key");
    if path.exists() {
        let bytes = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
        let array: [u8; 32] = bytes.as_slice().try_into().map_err(|_| {
            anyhow::anyhow!("{} is {} bytes; expected 32", path.display(), bytes.len())
        })?;
        return Ok(SecretKey::from_bytes(&array));
    }
    let key = SecretKey::generate();
    write_secret(&path, &key.to_bytes())?;
    Ok(key)
}

fn write_secret(path: &Path, bytes: &[u8; 32]) -> anyhow::Result<()> {
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
        let mut file = options.open(&tmp)?;
        file.write_all(bytes)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Cursor file for `--new` and `watch`. Missing file means "start at the beginning".
///
/// # Errors
///
/// Returns an error if the file exists but is not a decimal article number.
pub fn read_cursor(state_dir: &Path, group: &str) -> anyhow::Result<Option<u64>> {
    let path = cursor_path(state_dir, group)?;
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(&path)?;
    let number = text
        .trim()
        .parse()
        .with_context(|| format!("cursor {}", path.display()))?;
    Ok(Some(number))
}

fn write_cursor(state_dir: &Path, group: &str, number: u64) -> anyhow::Result<()> {
    let path = cursor_path(state_dir, group)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, number.to_string())?;
    Ok(())
}

fn cursor_path(state_dir: &Path, group: &str) -> anyhow::Result<PathBuf> {
    if group.is_empty()
        || group.contains('/')
        || group.contains('\\')
        || group.contains("..")
        || group.contains('\0')
    {
        bail!("invalid group name");
    }
    Ok(state_dir.join("cursors").join(group))
}

trait ByteIo: AsyncRead + AsyncWrite + Send {}
impl<T: AsyncRead + AsyncWrite + Send> ByteIo for T {}

struct BoxStream {
    inner: Pin<Box<dyn ByteIo>>,
}

impl BoxStream {
    fn new(stream: impl AsyncRead + AsyncWrite + Send + 'static) -> Self {
        Self {
            inner: Box::pin(stream),
        }
    }
}

impl AsyncRead for BoxStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.inner.as_mut().poll_read(cx, buf)
    }
}

impl AsyncWrite for BoxStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.inner.as_mut().poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.as_mut().poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.inner.as_mut().poll_shutdown(cx)
    }
}

struct BiStream<R, W> {
    recv: R,
    send: W,
}

impl<R, W> AsyncRead for BiStream<R, W>
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

impl<R, W> AsyncWrite for BiStream<R, W>
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

/// Flush a QUIC send stream before a write is reported complete.
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

/// Build a per-session `From` header from the base account `From` and a session label.
///
/// Produces `<session> <local+<12-hex-sha256>@domain>` so two sessions sharing
/// one machine account always have distinct, collision-free email addresses
/// even when their labels share a long prefix.
///
/// # Errors
///
/// Returns an error when `session` is empty, overly long, contains control
/// characters or angle brackets, or `base_from` has no valid `local@domain`.
pub fn format_session_from(base_from: &str, session: &str) -> anyhow::Result<String> {
    use sha2::{Digest, Sha256};

    let session = session.trim();
    if session.is_empty() {
        bail!("session label must not be empty");
    }
    if session.len() > 200 {
        bail!("session label is too long (max 200 bytes)");
    }
    if session
        .chars()
        .any(|c| c.is_control() || matches!(c, '<' | '>'))
    {
        bail!("session label must not contain control characters or angle brackets");
    }

    let address = match base_from.rsplit_once('<') {
        Some((_, rest)) => rest.trim_end_matches('>').trim(),
        None => base_from.trim(),
    };
    let (local, domain) = address
        .split_once('@')
        .filter(|(l, d)| !l.is_empty() && !d.is_empty())
        .ok_or_else(|| anyhow::anyhow!("base From address {base_from:?} has no local@domain"))?;
    let base_local = local.split_once('+').map_or(local, |(base, _)| base);

    let digest = Sha256::digest(session.as_bytes());
    let mut tag = String::with_capacity(12);
    for byte in &digest[..6] {
        use std::fmt::Write as _;
        let _ = write!(&mut tag, "{byte:02x}");
    }
    Ok(format!("{session} <{base_local}+{tag}@{domain}>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn article_dot_stuffs_and_terminates() {
        let raw = format_article(
            "bot@bots.local",
            "bots.general",
            "hello",
            Some("<root@bots.local>"),
            Some("<1.2@bots.local>"),
            "line\n.dot\n",
        )
        .unwrap();
        assert!(raw.contains("From: bot@bots.local\r\n"));
        assert!(raw.contains("Message-ID: <1.2@bots.local>\r\n"));
        assert!(raw.contains("References: <root@bots.local>\r\n"));
        assert!(raw.contains("\r\nline\r\n..dot\r\n.\r\n"));

        let bad = format_article(
            "bot@bots.local",
            "bots.general",
            "hello",
            None,
            Some("not-bracketed"),
            "x\n",
        );
        assert!(bad.is_err());

        let id = new_message_id("bot <bot@swarm.local>");
        assert!(id.starts_with('<') && id.ends_with("@swarm.local>"), "{id}");
        assert_ne!(id, new_message_id("bot <bot@swarm.local>"));
    }

    #[test]
    fn cursor_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        assert!(read_cursor(dir.path(), "bots.general").unwrap().is_none());
        write_cursor(dir.path(), "bots.general", 7).unwrap();
        assert_eq!(read_cursor(dir.path(), "bots.general").unwrap(), Some(7));
    }

    #[test]
    fn username_matches_safe_class() {
        assert!(validate_username("ada-grok").is_ok());
        assert!(validate_username("a").is_ok());
        assert!(validate_username("bot1").is_ok());
        for bad in [
            "", "Ada", "ada_grok", "ada.grok", "-ada", "ada-", "ada/grok",
        ] {
            assert!(validate_username(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn join_writes_stub_and_paste_block_without_a_password() {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("state");
        let config = dir.path().join("config/config.toml");
        let key = load_or_create_secret_key(&state).unwrap();
        let report = prepare_join(JoinRequest {
            name: "ada-grok",
            owner: Some("Ada"),
            server: DEFAULT_JOIN_SERVER,
            secret_key: &key,
            config_path: &config,
        })
        .unwrap();

        assert_eq!(report.endpoint_id, key.public().to_string());
        assert!(!report.password_file.exists());
        assert!(report.config_written);
        let stub = std::fs::read_to_string(&config).unwrap();
        assert_eq!(
            stub,
            "server = \"slime.onix.computer\"\nusername = \"ada-grok\"\n"
        );
        let human = report.human();
        assert!(human.contains(&report.endpoint_id));
        assert!(human.contains("username: ada-grok"));
        assert!(human.contains("owner: Ada"));
        assert!(human.contains("iroh-news groups"));
        assert!(!human.contains("password_file"));
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("\"config_written\":true"));
        assert!(json.contains("/run/secrets/slime/ada-grok"));

        std::fs::write(&config, "keep\n").unwrap();
        let again = prepare_join(JoinRequest {
            name: "ada-grok",
            owner: None,
            server: DEFAULT_JOIN_SERVER,
            secret_key: &key,
            config_path: &config,
        })
        .unwrap();
        assert!(!again.config_written);
        assert_eq!(again.endpoint_id, report.endpoint_id);
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "keep\n");
        assert!(again.human().contains("owner: (not given)"));
        assert!(!again.password_file.exists());
    }

    #[test]
    fn format_session_from_hashes_long_labels_without_collision() {
        let base = "britton-desktop <britton-desktop@swarm.local>";
        let a =
            format_session_from(base, "antigravity on britton-desktop, session 3dde2dbb").unwrap();
        let b =
            format_session_from(base, "antigravity on britton-desktop, session 2cf335d7").unwrap();
        assert!(
            a.starts_with("antigravity on britton-desktop, session 3dde2dbb <britton-desktop+")
        );
        assert!(a.ends_with("@swarm.local>"));
        assert_ne!(a, b);

        // Strips any existing +tag on base_from.
        let tagged = format_session_from(
            "old <britton-desktop+oldtag@swarm.local>",
            "omp on britton-desktop, session 1",
        )
        .unwrap();
        assert!(tagged.contains("<britton-desktop+"));
        assert!(!tagged.contains("oldtag"));

        assert!(format_session_from(base, "").is_err());
        assert!(format_session_from(base, "bad\nline").is_err());
        assert!(format_session_from(base, "bad <tag>").is_err());
        assert!(format_session_from("not-an-address", "ok").is_err());
    }
}
