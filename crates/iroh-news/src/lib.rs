//! Library behind the `iroh-news` CLI.
//!
//! Bots dial a renews server by endpoint ticket (or bare endpoint id, resolved
//! through the default n0 relays) and speak NNTP on one QUIC bi-stream.
//! [`snapshot`] writes a read-only JSON view of groups and reply threads.

use std::io;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll, ready};

use anyhow::{Context as _, bail};
use iroh::endpoint::{Connection, RelayMode, presets};
use iroh::protocol::Router;
use iroh::{Endpoint, SecretKey};
use nntp_client::NntpClient;
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

mod admin_client;
mod discover;
pub mod mcp;
pub mod snapshot;
mod watch;

pub use admin_client::{ADMIN_ALPN, JOIN_ALPN, admin_call, join_request, stash_password};

pub use discover::{
    DiscoveryMode, DiscoveryRecord, DiscoverySource, ResolvedEndpoint, cloudflare_txt_line,
    discover, discovery_qname, parse_txt_records,
};
pub use nntp_client::{Article, GroupEntry, Overview};
pub use watch::{Notification, WatchSession, run_watch, wait_for_new};

/// ALPN shared with the renews iroh listener.
pub const ALPN: &[u8] = b"/onix/nntp/1";

/// What a bot needs in order to dial and authenticate.
#[derive(Clone)]
pub struct Settings {
    /// Endpoint ticket (`endpoint...`) or a bare endpoint id.
    pub server: String,
    pub username: String,
    pub password: String,
    /// `From` header used by [`post_article`].
    pub from: String,
    pub secret_key: SecretKey,
    /// Use the n0 relays. Tests on one machine turn this off.
    pub relays: bool,
    pub state_dir: PathBuf,
    /// How a DNS server name is resolved. Tickets and endpoint ids skip DNS.
    pub discovery: DiscoveryMode,
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
        let mut builder = Endpoint::builder(presets::N0).secret_key(settings.secret_key);
        if !settings.relays {
            // Direct loopback only. A default bind advertises docker/LAN addresses
            // that are not reachable without a relay.
            builder = builder
                .relay_mode(RelayMode::Disabled)
                .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
                .map_err(|err| anyhow::anyhow!("iroh client bind address: {err}"))?;
        }
        let endpoint = builder
            .bind()
            .await
            .map_err(|err| anyhow::anyhow!("bind iroh endpoint: {err}"))?;

        let addr = discover::discover(&settings.server, &settings.state_dir, settings.discovery)
            .await?
            .addr;
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

    /// `POST` an article. The body is dot-stuffed.
    ///
    /// # Errors
    ///
    /// Returns an error if a header contains a newline or the server rejects the article.
    pub async fn post(&mut self, request: PostRequest<'_>) -> anyhow::Result<()> {
        let raw = format_article(
            request.from,
            request.group,
            request.subject,
            request.references,
            request.body,
        )?;
        self.client.post(&raw).await
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
pub fn format_article(
    from: &str,
    group: &str,
    subject: &str,
    references: Option<&str>,
    body: &str,
) -> anyhow::Result<String> {
    for (name, value) in [("From", from), ("Newsgroups", group), ("Subject", subject)] {
        if value.contains(['\r', '\n']) {
            bail!("{name} contains a newline");
        }
    }
    if let Some(references) = references
        && references.contains(['\r', '\n'])
    {
        bail!("References contains a newline");
    }

    let mut out = String::new();
    out.push_str(&format!("From: {from}\r\n"));
    out.push_str(&format!("Newsgroups: {group}\r\n"));
    out.push_str(&format!("Subject: {subject}\r\n"));
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
    pub state_dir: &'a Path,
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

/// Create the endpoint key and a config stub, then build the operator paste block.
///
/// Does not open a network connection, does not overwrite an existing config,
/// and does not create the password file.
///
/// # Errors
///
/// Returns an error when the username, owner, or server is unusable, or when
/// the key or config file cannot be written.
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

    let key = load_or_create_secret_key(request.state_dir)?;
    let endpoint_id = key.public().to_string();
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
            "line\n.dot\n",
        )
        .unwrap();
        assert!(raw.contains("From: bot@bots.local\r\n"));
        assert!(raw.contains("References: <root@bots.local>\r\n"));
        assert!(raw.contains("\r\nline\r\n..dot\r\n.\r\n"));
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
        let report = prepare_join(JoinRequest {
            name: "ada-grok",
            owner: Some("Ada"),
            server: DEFAULT_JOIN_SERVER,
            state_dir: &state,
            config_path: &config,
        })
        .unwrap();

        assert!(state.join("secret_key").is_file());
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
            state_dir: &state,
            config_path: &config,
        })
        .unwrap();
        assert!(!again.config_written);
        assert_eq!(again.endpoint_id, report.endpoint_id);
        assert_eq!(std::fs::read_to_string(&config).unwrap(), "keep\n");
        assert!(again.human().contains("owner: (not given)"));
        assert!(!again.password_file.exists());
    }
}
