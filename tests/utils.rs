#![allow(dead_code)]

use rcgen::{CertifiedKey, generate_simple_self_signed};
use renews::auth::AuthProvider;
use renews::config::Config;
use renews::handle_client;
use renews::limits::UsageTracker;
use renews::queue::ArticleQueue;
use renews::storage::Storage;
use std::sync::Arc;
use tokio::io::BufReader;
use tokio::io::{self, ReadHalf, WriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::RwLock;
use tokio_rustls::{TlsAcceptor, TlsConnector, rustls};

/// Create an in-memory storage and auth provider pair for tests.
pub async fn setup() -> (Arc<dyn Storage>, Arc<dyn AuthProvider>) {
    let storage = create_test_storage().await;
    let auth = create_test_auth().await;
    (storage, auth)
}

/// Create a test storage instance
pub async fn create_test_storage() -> renews::storage::DynStorage {
    use renews::storage::sqlite::SqliteStorage;
    let storage = SqliteStorage::new("sqlite::memory:").await.unwrap();
    std::sync::Arc::new(storage)
}

/// Create a test auth instance
pub async fn create_test_auth() -> renews::auth::DynAuth {
    use renews::auth::sqlite::SqliteAuth;
    let auth = SqliteAuth::new("sqlite::memory:").await.unwrap();
    std::sync::Arc::new(auth)
}

/// Create a test article queue (legacy function - does not start workers)
pub fn create_test_queue() -> ArticleQueue {
    ArticleQueue::new(10) // Small capacity for tests
}

/// Create a test usage tracker
pub fn create_test_usage_tracker(
    auth: renews::auth::DynAuth,
    config: &Config,
) -> Arc<UsageTracker> {
    Arc::new(UsageTracker::new(auth, config.user_limits.clone()))
}

/// Lines returned by the CAPABILITIES command.
pub fn capabilities_lines() -> Vec<String> {
    vec![
        "101 Capability list follows".into(),
        "VERSION 2".into(),
        format!("IMPLEMENTATION Renews {}", env!("CARGO_PKG_VERSION")),
        "READER".into(),
        "NEWNEWS".into(),
        "IHAVE".into(),
        "STREAMING".into(),
        "OVER MSGID".into(),
        "HDR".into(),
        "LIST ACTIVE NEWSGROUPS ACTIVE.TIMES OVERVIEW.FMT HEADERS".into(),
        ".".into(),
    ]
}

/// Split a request string into individual lines.
pub fn request_lines(text: &str) -> Vec<String> {
    text.split("\r\n").map(|l| l.to_string()).collect()
}

/// Build a detached PGP signature for the provided data.
pub fn build_sig(data: &str) -> (String, Vec<String>) {
    use pgp::native::crypto::hash::HashAlgorithm;
    use pgp::native::packet::{SignatureConfig, SignatureType, SignatureVersion};
    use pgp::native::types::{KeyTrait, SecretKeyTrait};
    use pgp::native::{Deserializable, SignedSecretKey, StandaloneSignature};
    use std::io::Cursor;

    const ADMIN_SEC: &str = include_str!("integration/../data/admin.sec.asc");

    let (key, _) = SignedSecretKey::from_string(ADMIN_SEC).unwrap();
    let cfg = SignatureConfig::new_v4(
        SignatureVersion::V4,
        SignatureType::Binary,
        key.primary_key.public_key().algorithm(),
        HashAlgorithm::SHA2_256,
        Vec::new(),
        Vec::new(),
    );
    let sig = cfg
        .sign(&key.primary_key, String::new, Cursor::new(data.as_bytes()))
        .unwrap();
    let armored = StandaloneSignature::new(sig)
        .to_armored_string(Default::default())
        .unwrap();
    let version = "1".to_string();
    let mut lines = Vec::new();
    for line in armored.lines() {
        if line.starts_with("-----BEGIN") || line.starts_with("Version") || line.is_empty() {
            continue;
        }
        if line.starts_with("-----END") {
            break;
        }
        lines.push(line.to_string());
    }
    (version, lines)
}

/// Generate a self-signed TLS certificate for use in tests.
pub fn generate_self_signed_cert() -> (rustls::Certificate, rustls::PrivateKey, String) {
    let CertifiedKey { cert, signing_key } =
        generate_simple_self_signed(["localhost".to_string()]).unwrap();
    let cert_der = cert.der().to_vec();
    let key_der = signing_key.serialize_der();
    let pem = cert.pem();
    (
        rustls::Certificate(cert_der),
        rustls::PrivateKey(key_der),
        pem,
    )
}

pub async fn setup_server(
    storage: Arc<dyn Storage>,
    auth: Arc<dyn AuthProvider>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store_clone = storage.clone();
    let auth_clone = auth.clone();
    let cfg: Arc<RwLock<Config>> = Arc::new(RwLock::new(toml::from_str("addr=\":119\"").unwrap()));
    let queue = create_test_queue();
    let cfg_read = cfg.read().await;
    let usage_tracker = create_test_usage_tracker(auth.clone(), &cfg_read);
    drop(cfg_read);

    // Start worker pool for queue processing
    let worker_pool = renews::queue::WorkerPool::new(
        queue.clone(),
        storage.clone(),
        auth.clone(),
        cfg.clone(),
        2, // Use 2 workers for tests
    );
    let _worker_handles = worker_pool.start().await;

    let handle = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        handle_client(
            sock,
            store_clone,
            auth_clone,
            cfg,
            false,
            queue,
            usage_tracker,
        )
        .await
        .unwrap();
    });
    (addr, handle)
}

pub async fn setup_server_with_cfg(
    storage: Arc<dyn Storage>,
    auth: Arc<dyn AuthProvider>,
    cfg: Arc<RwLock<Config>>,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store_clone = storage.clone();
    let auth_clone = auth.clone();
    let queue = create_test_queue();
    let cfg_read = cfg.read().await;
    let usage_tracker = create_test_usage_tracker(auth.clone(), &cfg_read);
    drop(cfg_read);

    // Start worker pool for queue processing
    let worker_pool = renews::queue::WorkerPool::new(
        queue.clone(),
        storage.clone(),
        auth.clone(),
        cfg.clone(),
        2, // Use 2 workers for tests
    );
    let _worker_handles = worker_pool.start().await;

    let handle = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        handle_client(
            sock,
            store_clone,
            auth_clone,
            cfg,
            false,
            queue,
            usage_tracker,
        )
        .await
        .unwrap();
    });
    (addr, handle)
}

pub async fn connect(
    addr: std::net::SocketAddr,
) -> (
    BufReader<tokio::net::tcp::OwnedReadHalf>,
    tokio::net::tcp::OwnedWriteHalf,
) {
    let stream = TcpStream::connect(addr).await.unwrap();
    let (r, w) = stream.into_split();
    (BufReader::new(r), w)
}

pub async fn setup_tls_server_with_cert(
    storage: Arc<dyn Storage>,
    auth: Arc<dyn AuthProvider>,
    cert: rustls::Certificate,
    key: rustls::PrivateKey,
) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
    let tls_config = rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(vec![cert.clone()], key)
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(tls_config));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store_clone = storage.clone();
    let auth_clone = auth.clone();
    let cfg: Arc<RwLock<Config>> = Arc::new(RwLock::new(toml::from_str("addr=\":119\"").unwrap()));
    let queue = create_test_queue();
    let cfg_read = cfg.read().await;
    let usage_tracker = create_test_usage_tracker(auth.clone(), &cfg_read);
    drop(cfg_read);

    // Start worker pool for queue processing
    let worker_pool = renews::queue::WorkerPool::new(
        queue.clone(),
        storage.clone(),
        auth.clone(),
        cfg.clone(),
        2, // Use 2 workers for tests
    );
    let _worker_handles = worker_pool.start().await;

    let handle = tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let stream = acceptor.accept(sock).await.unwrap();
        handle_client(
            stream,
            store_clone,
            auth_clone,
            cfg,
            true,
            queue,
            usage_tracker,
        )
        .await
        .unwrap();
    });
    (addr, handle)
}

pub async fn setup_tls_server(
    storage: Arc<dyn Storage>,
    auth: Arc<dyn AuthProvider>,
) -> (
    std::net::SocketAddr,
    rustls::Certificate,
    String,
    tokio::task::JoinHandle<()>,
) {
    let (cert, key, pem) = generate_self_signed_cert();
    let (addr, handle) = setup_tls_server_with_cert(storage, auth, cert.clone(), key).await;
    (addr, cert, pem, handle)
}

pub async fn connect_tls(
    addr: std::net::SocketAddr,
    cert: rustls::Certificate,
) -> (
    BufReader<ReadHalf<tokio_rustls::client::TlsStream<TcpStream>>>,
    WriteHalf<tokio_rustls::client::TlsStream<TcpStream>>,
) {
    let mut roots = rustls::RootCertStore::empty();
    roots.add(&cert).unwrap();
    let config = rustls::ClientConfig::builder()
        .with_safe_defaults()
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(config));
    let stream = TcpStream::connect(addr).await.unwrap();
    let server_name = rustls::ServerName::try_from("localhost").unwrap();
    let tls_stream = connector.connect(server_name, stream).await.unwrap();
    let (r, w) = io::split(tls_stream);
    (BufReader::new(r), w)
}

/// Start a new NNTP server for testing.
pub async fn start_server(
    storage: Arc<dyn Storage>,
    auth: Arc<dyn AuthProvider>,
    cfg: Config,
    tls: bool,
) -> (
    std::net::SocketAddr,
    Option<(rustls::Certificate, String)>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = Arc::new(RwLock::new(cfg));
    let store_clone = storage.clone();
    let auth_clone = auth.clone();
    if tls {
        let (cert, key, pem) = generate_self_signed_cert();
        let tls_config = rustls::ServerConfig::builder()
            .with_safe_defaults()
            .with_no_client_auth()
            .with_single_cert(vec![cert.clone()], key)
            .unwrap();
        let acceptor = TlsAcceptor::from(Arc::new(tls_config));
        let queue = create_test_queue();
        let cfg_read = cfg.read().await;
        let usage_tracker = create_test_usage_tracker(auth.clone(), &cfg_read);
        drop(cfg_read);

        // Start worker pool for queue processing
        let worker_pool = renews::queue::WorkerPool::new(
            queue.clone(),
            storage.clone(),
            auth.clone(),
            cfg.clone(),
            2, // Use 2 workers for tests
        );
        let _worker_handles = worker_pool.start().await;

        let handle = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let stream = acceptor.accept(sock).await.unwrap();
            handle_client(
                stream,
                store_clone,
                auth_clone,
                cfg,
                true,
                queue,
                usage_tracker,
            )
            .await
            .unwrap();
        });
        (addr, Some((cert, pem)), handle)
    } else {
        let queue = create_test_queue();
        let cfg_read = cfg.read().await;
        let usage_tracker = create_test_usage_tracker(auth.clone(), &cfg_read);
        drop(cfg_read);

        // Start worker pool for queue processing
        let worker_pool = renews::queue::WorkerPool::new(
            queue.clone(),
            storage.clone(),
            auth.clone(),
            cfg.clone(),
            2, // Use 2 workers for tests
        );
        let _worker_handles = worker_pool.start().await;

        let handle = tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            handle_client(
                sock,
                store_clone,
                auth_clone,
                cfg,
                false,
                queue,
                usage_tracker,
            )
            .await
            .unwrap();
        });
        (addr, None, handle)
    }
}

pub async fn run_client(
    client: ClientMock,
    storage: Arc<dyn Storage>,
    auth: Arc<dyn AuthProvider>,
) {
    run_client_with_cfg(
        client,
        toml::from_str("addr=\":119\"").unwrap(),
        storage,
        auth,
        false,
    )
    .await;
}

pub async fn run_client_tls(
    client: ClientMock,
    storage: Arc<dyn Storage>,
    auth: Arc<dyn AuthProvider>,
) {
    run_client_with_cfg(
        client,
        toml::from_str("addr=\":119\"").unwrap(),
        storage,
        auth,
        true,
    )
    .await;
}

pub async fn run_client_with_cfg(
    client: ClientMock,
    cfg: Config,
    storage: Arc<dyn Storage>,
    auth: Arc<dyn AuthProvider>,
    tls: bool,
) {
    let (addr, cert, handle) = start_server(storage, auth, cfg, tls).await;
    if let Some((c, _)) = cert {
        client.run_tls_at(addr, c).await;
    } else {
        client.run_tcp_at(addr).await;
    }
    handle.await.unwrap();
}

pub async fn run_client_with_cfg_tls(
    client: ClientMock,
    cfg: Config,
    storage: Arc<dyn Storage>,
    auth: Arc<dyn AuthProvider>,
) {
    run_client_with_cfg(client, cfg, storage, auth, true).await;
}

impl ClientMock {
    pub async fn run(self, storage: Arc<dyn Storage>, auth: Arc<dyn AuthProvider>) {
        run_client(self, storage, auth).await;
    }

    pub async fn run_tls(self, storage: Arc<dyn Storage>, auth: Arc<dyn AuthProvider>) {
        run_client_tls(self, storage, auth).await;
    }

    pub async fn run_with_cfg(
        self,
        cfg: Config,
        storage: Arc<dyn Storage>,
        auth: Arc<dyn AuthProvider>,
    ) {
        run_client_with_cfg(self, cfg, storage, auth, false).await;
    }

    pub async fn run_with_cfg_tls(
        self,
        cfg: Config,
        storage: Arc<dyn Storage>,
        auth: Arc<dyn AuthProvider>,
    ) {
        run_client_with_cfg_tls(self, cfg, storage, auth).await;
    }
}

/// Builder to mock a client connection using `tokio_test::io`.
pub struct ClientMock {
    steps: Vec<(Vec<String>, Vec<String>)>,
}

impl Default for ClientMock {
    fn default() -> Self {
        Self::new()
    }
}

impl ClientMock {
    pub fn new() -> Self {
        Self { steps: Vec::new() }
    }

    /// Create a ClientMock that sends AUTHINFO USER/PASS commands first.
    ///
    /// This is a convenience constructor for tests that require authentication.
    pub fn with_auth(username: &str, password: &str) -> Self {
        Self::new()
            .expect(
                &format!("AUTHINFO USER {username}"),
                "381 password required",
            )
            .expect(
                &format!("AUTHINFO PASS {password}"),
                "281 authentication accepted",
            )
    }

    /// Create a ClientMock that sends MODE READER first.
    ///
    /// This is a convenience constructor for tests that use reader mode.
    /// The `can_post` parameter determines the expected response.
    pub fn with_mode_reader(can_post: bool) -> Self {
        let response = if can_post {
            "200 Posting allowed"
        } else {
            "201 Posting prohibited"
        };
        Self::new().expect("MODE READER", response)
    }

    /// Create a ClientMock that sends MODE READER followed by GROUP.
    ///
    /// This is a convenience constructor for tests that need to select a group.
    pub fn with_group(group: &str, count: u64, low: u64, high: u64) -> Self {
        Self::with_mode_reader(false).expect(
            &format!("GROUP {group}"),
            &format!("211 {count} {low} {high} {group}"),
        )
    }

    /// Expect a command with a single-line response.
    pub fn expect(mut self, cmd: &str, resp: &str) -> Self {
        self.steps
            .push((vec![cmd.to_string()], vec![resp.to_string()]));
        self
    }

    /// Expect a command that should fail with a specific error code.
    pub fn expect_failure(mut self, cmd: &str, error_code: u16) -> Self {
        let error_msg = format!("{error_code} command failed");
        self.steps.push((vec![cmd.to_string()], vec![error_msg]));
        self
    }

    /// Expect a command with a multi-line response.
    pub fn expect_multi<S: Into<String>>(mut self, cmd: &str, resp: Vec<S>) -> Self {
        self.steps.push((
            vec![cmd.to_string()],
            resp.into_iter().map(Into::into).collect(),
        ));
        self
    }

    /// Expect a multi-line request with optional multi-line response.
    pub fn expect_request_multi<R, S>(mut self, cmds: Vec<R>, resp: Vec<S>) -> Self
    where
        R: Into<String>,
        S: Into<String>,
    {
        self.steps.push((
            cmds.into_iter().map(Into::into).collect(),
            resp.into_iter().map(Into::into).collect(),
        ));
        self
    }

    pub async fn drive<R, W>(self, mut reader: R, mut writer: W)
    where
        R: tokio::io::AsyncBufRead + Unpin,
        W: tokio::io::AsyncWrite + Unpin,
    {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

        let mut line = String::new();
        reader.read_line(&mut line).await.unwrap();
        for (cmds, resps) in self.steps {
            for cmd in cmds {
                writer
                    .write_all(format!("{cmd}\r\n").as_bytes())
                    .await
                    .unwrap();
            }
            for resp in resps {
                line.clear();
                reader.read_line(&mut line).await.unwrap();
                assert_eq!(line.trim_end_matches(['\r', '\n']), resp);
            }
        }
        let _ = writer.shutdown().await;
    }

    pub async fn run_tcp_at(self, addr: std::net::SocketAddr) {
        let (reader, writer) = connect(addr).await;
        self.drive(reader, writer).await;
    }

    pub async fn run_tls_at(self, addr: std::net::SocketAddr, cert: rustls::Certificate) {
        let (reader, writer) = connect_tls(addr, cert).await;
        self.drive(reader, writer).await;
    }
}

/// Create a malformed article for testing parser failures
pub fn create_malformed_article(malformation_type: &str) -> String {
    match malformation_type {
        "no_headers" => "\r\nBody without headers\r\n.\r\n".to_string(),
        "missing_from" => "Subject: Test\r\nNewsgroups: test.group\r\n\r\nBody\r\n.\r\n".to_string(),
        "missing_subject" => "From: test@example.com\r\nNewsgroups: test.group\r\n\r\nBody\r\n.\r\n".to_string(),
        "missing_newsgroups" => "From: test@example.com\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_string(),
        "invalid_header" => "InvalidHeader\r\nFrom: test@example.com\r\nSubject: Test\r\n\r\nBody\r\n.\r\n".to_string(),
        "extremely_long" => {
            let long_subject = "x".repeat(10000);
            format!("From: test@example.com\r\nSubject: {long_subject}\r\nNewsgroups: test.group\r\n\r\nBody\r\n.\r\n")
        },
        "binary_content" => "From: test@example.com\r\nSubject: Test\r\nNewsgroups: test.group\r\n\r\nBody with \0 binary\r\n.\r\n".to_string(),
        _ => "From: test@example.com\r\nSubject: Test\r\nNewsgroups: test.group\r\n\r\nNormal body\r\n.\r\n".to_string(),
    }
}

/// Create a large article that exceeds size limits
pub fn create_large_article(size_kb: usize) -> String {
    let body = "x".repeat(size_kb * 1024);
    format!(
        "From: test@example.com\r\nSubject: Large Article\r\nNewsgroups: test.group\r\nMessage-ID: <large@example.com>\r\n\r\n{body}\r\n.\r\n"
    )
}

/// Create a test article queue with workers
pub async fn create_test_queue_with_workers(
    storage: Arc<dyn Storage>,
    auth: Arc<dyn AuthProvider>,
    config: Arc<RwLock<Config>>,
) -> ArticleQueue {
    let queue = ArticleQueue::new(10); // Small capacity for tests

    // Start worker pool
    let worker_pool = renews::queue::WorkerPool::new(
        queue.clone(),
        storage,
        auth,
        config,
        2, // Use 2 workers for tests
    );

    let _worker_handles = worker_pool.start().await;

    queue
}

/// Create a test configuration with minimal settings
pub fn create_minimal_config() -> Config {
    Config {
        addr: "127.0.0.1:0".to_string(),
        site_name: "test".to_string(),
        db_path: "sqlite::memory:".to_string(),
        auth_db_path: "sqlite::memory:".to_string(),
        peer_db_path: "sqlite::memory:".to_string(),
        peer_sync_schedule: "0 0 * * * *".to_string(),
        idle_timeout_secs: 600,
        peers: vec![],
        tls_addr: None,
        tls_cert: None,
        tls_key: None,
        ws_addr: None,
        article_queue_capacity: 10,
        article_worker_count: 2,
        group_settings: vec![],
        filters: vec![],
        pgp_key_servers: renews::config::default_pgp_key_servers(),
        allow_auth_insecure_connections: false,
        allow_anonymous_posting: false,
        runtime_threads: 4,
        logging: Default::default(),
        user_limits: Default::default(),
        iroh: Default::default(),
        bots_can_create_groups: true,
        group_prefixes: vec!["bots.".into()],
        group_creates_per_day: 3,
        max_groups: 200,
    }
}

/// Create a test configuration with specific limits for failure testing
pub fn create_failure_test_config(
    _max_article_bytes: Option<u64>,
    queue_capacity: usize,
    idle_timeout_secs: u64,
) -> Config {
    let mut config = create_minimal_config();
    config.idle_timeout_secs = idle_timeout_secs;
    config.article_queue_capacity = queue_capacity;
    config.article_worker_count = 1;
    config
}

// =============================================================================
// Test Article Helpers
// =============================================================================

/// Store a test article from raw text and return the parsed message.
///
/// This is a convenience wrapper around `parse_message` + `store_article` that
/// is commonly used in tests.
pub async fn store_test_article(storage: &dyn Storage, text: &str) -> renews::Message {
    let (_, msg) = renews::parse_message(text).expect("parse_message");
    storage.store_article(&msg).await.expect("store_article");
    msg
}

/// Create a queued article for testing queue-based processing.
///
/// Returns a `QueuedArticle` ready to be submitted to an `ArticleQueue`.
pub fn create_test_queued_article(
    message_id: &str,
    newsgroups: &str,
    body: &str,
) -> renews::queue::QueuedArticle {
    let text = format!(
        "Message-ID: {message_id}\r\nNewsgroups: {newsgroups}\r\nFrom: test@example.com\r\nSubject: Test\r\n\r\n{body}"
    );
    let (_, msg) = renews::parse_message(&text).expect("parse_message");
    let size = text.len() as u64;
    renews::queue::QueuedArticle {
        message: msg,
        size,
        is_control: false,
        already_validated: false,
    }
}

/// Collect all groups from storage into a Vec.
///
/// This is a convenience wrapper for the common pattern of collecting
/// stream results into a Vec.
pub async fn collect_groups(storage: &dyn Storage) -> Vec<String> {
    use futures_util::StreamExt;
    let mut groups = Vec::new();
    let mut stream = storage.list_groups();
    while let Some(result) = stream.next().await {
        groups.push(result.expect("list_groups"));
    }
    groups
}

/// Collect all article numbers from a group into a Vec.
///
/// This is a convenience wrapper for the common pattern of collecting
/// stream results into a Vec.
pub async fn collect_article_numbers(storage: &dyn Storage, group: &str) -> Vec<u64> {
    use futures_util::StreamExt;
    let mut numbers = Vec::new();
    let mut stream = storage.list_article_numbers(group);
    while let Some(result) = stream.next().await {
        numbers.push(result.expect("list_article_numbers"));
    }
    numbers
}

/// Extract Message-ID header value from a message.
///
/// Convenience function for tests that need to verify Message-ID values.
pub fn get_message_id(msg: &renews::Message) -> Option<String> {
    msg.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("Message-ID"))
        .map(|(_, v)| v.clone())
}

/// Extract a header value from a message (case-insensitive).
///
/// Convenience function for tests that need to verify header values.
pub fn get_header(msg: &renews::Message, name: &str) -> Option<String> {
    msg.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}
