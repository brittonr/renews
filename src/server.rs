//! NNTP Server implementation
//!
//! This module contains the main server infrastructure for the NNTP server,
//! including TCP and TLS listeners, peer synchronization, and configuration reloading.
//!
//! ## Architecture
//!
//! The server is organized into several key components:
//!
//! - **Server**: Main server struct that orchestrates all components
//! - **ServerComponents**: Shared resources (storage, auth, config)
//! - **ConfigManager**: Handles configuration loading and TLS setup
//! - **PeerManager**: Manages peer synchronization tasks
//!
//! ## Key Features
//!
//! - Concurrent handling of TCP and TLS connections
//! - Hot configuration reloading via SIGHUP
//! - WebSocket bridge support (optional)
//! - Automatic peer synchronization
//! - Article retention cleanup
//!

use std::fs::File;
use std::io::BufReader;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use std::net::SocketAddr;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_rustls::{TlsAcceptor, rustls};
use tracing::{error, info, warn};

use dashmap::DashMap;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::RwLock;
use tokio_cron_scheduler::JobScheduler;

use crate::auth::{self, AuthProvider};
use crate::config::Config;
use crate::limits::UsageTracker;
use crate::peers::{PeerConfig, PeerDb, add_peer_job};
use crate::queue::{ArticleQueue, WorkerPool};
use crate::retention::cleanup_expired_articles;
use crate::storage::{self, Storage};
#[cfg(feature = "websocket")]
use crate::ws;
use rustls_pemfile::{certs, pkcs8_private_keys};

type ServerResult<T> = anyhow::Result<T>;

/// Tracks active connections for graceful shutdown
pub struct ConnectionTracker {
    active_count: AtomicUsize,
    shutdown_signal: tokio::sync::broadcast::Sender<()>,
}

impl ConnectionTracker {
    pub fn new() -> (Self, tokio::sync::broadcast::Receiver<()>) {
        let (tx, rx) = tokio::sync::broadcast::channel(1);
        (
            Self {
                active_count: AtomicUsize::new(0),
                shutdown_signal: tx,
            },
            rx,
        )
    }

    pub fn connection_started(&self) {
        self.active_count.fetch_add(1, Ordering::SeqCst);
    }

    pub fn connection_ended(&self) {
        self.active_count.fetch_sub(1, Ordering::SeqCst);
    }

    pub fn active_connections(&self) -> usize {
        self.active_count.load(Ordering::SeqCst)
    }

    pub fn signal_shutdown(&self) {
        let _ = self.shutdown_signal.send(());
    }

    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<()> {
        self.shutdown_signal.subscribe()
    }
}

impl Default for ConnectionTracker {
    fn default() -> Self {
        Self::new().0
    }
}

/// Shared server components
#[derive(Clone)]
struct ServerComponents {
    storage: Arc<dyn Storage>,
    auth: Arc<dyn AuthProvider>,
    config: Arc<RwLock<Config>>,
    queue: ArticleQueue,
    usage_tracker: Arc<UsageTracker>,
}

/// Server handles all lifecycle management
pub struct Server {
    components: ServerComponents,
    config_manager: ConfigManager,
    peer_manager: PeerManager,
    worker_pool: WorkerPool,
}

impl Server {
    /// Create a new server instance
    pub async fn new(cfg: Config) -> ServerResult<Self> {
        let components = Self::initialize_components(&cfg).await?;
        let peer_db = Self::initialize_peer_db(&cfg).await?;
        let config_manager = ConfigManager::new(components.config.clone());
        let peer_manager = PeerManager::new(peer_db).await?;

        // Create worker pool
        let worker_pool = WorkerPool::new(
            components.queue.clone(),
            components.storage.clone(),
            components.auth.clone(),
            components.config.clone(),
            cfg.article_worker_count,
        );

        Ok(Self {
            components,
            config_manager,
            peer_manager,
            worker_pool,
        })
    }

    /// Initialize core server components
    async fn initialize_components(cfg: &Config) -> ServerResult<ServerComponents> {
        let config = Arc::new(RwLock::new(cfg.clone()));

        let storage: Arc<dyn Storage> = storage::open(&cfg.db_path).await?;
        let auth: Arc<dyn AuthProvider> = auth::open(&cfg.auth_db_path).await?;

        // Create article queue with configurable capacity
        let queue = ArticleQueue::new(cfg.article_queue_capacity);

        // Create usage tracker with auth provider and default limits
        let usage_tracker = Arc::new(UsageTracker::new(auth.clone(), cfg.user_limits.clone()));

        Ok(ServerComponents {
            storage,
            auth,
            config,
            queue,
            usage_tracker,
        })
    }

    /// Initialize peer database and sync configuration
    async fn initialize_peer_db(cfg: &Config) -> ServerResult<PeerDb> {
        let peer_db = PeerDb::new(&cfg.peer_db_path).await?;
        let names: Vec<String> = cfg.peers.iter().map(|p| p.sitename.clone()).collect();
        peer_db.sync_config(&names).await?;
        Ok(peer_db)
    }

    /// Start all peer synchronization tasks
    async fn start_peer_tasks(&self) -> ServerResult<()> {
        let cfg_guard = self.components.config.read().await;
        self.peer_manager
            .start_peer_tasks(&cfg_guard, self.components.storage.clone())
            .await
    }

    /// Start TCP listener task
    async fn start_tcp_listener(&self) -> ServerResult<tokio::task::JoinHandle<()>> {
        let (addr_config, localhost_default) = {
            let cfg_guard = self.components.config.read().await;
            // iroh deployments have no public ports. A host-less `addr` such as
            // `:119` binds to localhost unless the operator opts out.
            let localhost_default = cfg_guard.iroh.enabled && cfg_guard.iroh.localhost_tcp;
            (cfg_guard.addr.clone(), localhost_default)
        };

        let listener = get_listener(&addr_config, localhost_default).await?;

        // ...existing code...
        let storage = self.components.storage.clone();
        let auth = self.components.auth.clone();
        let config = self.components.config.clone();
        let queue = self.components.queue.clone();
        let usage_tracker = self.components.usage_tracker.clone();

        let handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, _)) => {
                        info!(is_tls = false, "Connection accepted");
                        handle_connection(
                            socket,
                            storage.clone(),
                            auth.clone(),
                            config.clone(),
                            false,
                            queue.clone(),
                            usage_tracker.clone(),
                        )
                        .await;
                    }
                    Err(e) => error!(error = %e, "Failed to accept connection"),
                }
            }
        });

        Ok(handle)
    }

    /// Start TLS listener task if configured
    async fn start_tls_listener(&self) -> ServerResult<Option<tokio::task::JoinHandle<()>>> {
        let cfg_guard = self.components.config.read().await;

        let Some((tls_addr_raw, cert, key)) = (|| {
            Some((
                cfg_guard.tls_addr.as_deref()?,
                cfg_guard.tls_cert.as_ref()?,
                cfg_guard.tls_key.as_ref()?,
            ))
        })() else {
            return Ok(None);
        };

        let tls_listener = get_listener(tls_addr_raw, false).await?;
        let acceptor = TlsAcceptor::from(Arc::new(load_tls_config(cert, key)?));
        *self.config_manager.tls_acceptor.write().await = Some(acceptor.clone());

        let storage = self.components.storage.clone();
        let auth = self.components.auth.clone();
        let config = self.components.config.clone();
        let queue = self.components.queue.clone();
        let usage_tracker = self.components.usage_tracker.clone();

        let handle = tokio::spawn(async move {
            loop {
                match tls_listener.accept().await {
                    Ok((socket, _)) => {
                        info!(is_tls = true, "Connection accepted");
                        let storage_clone = storage.clone();
                        let auth_clone = auth.clone();
                        let config_clone = config.clone();
                        let acceptor_clone = acceptor.clone();
                        let queue_clone = queue.clone();
                        let usage_tracker_clone = usage_tracker.clone();

                        tokio::spawn(async move {
                            match acceptor_clone.accept(socket).await {
                                Ok(stream) => {
                                    handle_connection(
                                        stream,
                                        storage_clone,
                                        auth_clone,
                                        config_clone,
                                        true,
                                        queue_clone,
                                        usage_tracker_clone,
                                    )
                                    .await;
                                }
                                Err(e) => error!(error = %e, "TLS handshake failed"),
                            }
                        });
                    }
                    Err(e) => error!(error = %e, "Failed to accept TLS connection"),
                }
            }
        });

        Ok(Some(handle))
    }

    /// Start WebSocket bridge task if configured
    #[cfg(feature = "websocket")]
    async fn start_websocket_bridge(&self) -> ServerResult<Option<tokio::task::JoinHandle<()>>> {
        let cfg_guard = self.components.config.read().await;

        if let Some(addr_raw) = cfg_guard.ws_addr.as_deref() {
            info!("websocket bridge on {addr_raw}");
            let config = self.components.config.clone();

            let handle = tokio::spawn(async move {
                if let Err(e) = ws::run_ws_bridge(config).await {
                    error!("websocket error: {e}");
                }
            });

            Ok(Some(handle))
        } else {
            Ok(None)
        }
    }

    /// Start WebSocket bridge task (no-op for non-websocket builds)
    #[cfg(not(feature = "websocket"))]
    async fn start_websocket_bridge(&self) -> ServerResult<Option<tokio::task::JoinHandle<()>>> {
        Ok(None)
    }

    /// Start retention cleanup task
    async fn start_retention_cleanup(&self) -> ServerResult<tokio::task::JoinHandle<()>> {
        let storage = self.components.storage.clone();
        let config = self.components.config.clone();

        let handle = tokio::spawn(async move {
            loop {
                let cfg_guard = config.read().await;
                if let Err(e) = cleanup_expired_articles(&*storage, &cfg_guard).await {
                    error!("retention cleanup error: {e}");
                }
                drop(cfg_guard);
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });

        Ok(handle)
    }

    /// Start usage persistence task to periodically save usage data
    async fn start_usage_persistence(&self) -> ServerResult<tokio::task::JoinHandle<()>> {
        let usage_tracker = self.components.usage_tracker.clone();

        let handle = tokio::spawn(async move {
            loop {
                // Persist usage data every 60 seconds
                tokio::time::sleep(std::time::Duration::from_secs(60)).await;

                if let Err(e) = usage_tracker.persist().await {
                    error!("usage persistence error: {e}");
                }
            }
        });

        Ok(handle)
    }

    /// Start configuration reload handler
    async fn start_config_reload_handler(
        &self,
        cfg_path: String,
    ) -> ServerResult<tokio::task::JoinHandle<()>> {
        let config_manager = self.config_manager.clone();
        let peer_manager = self.peer_manager.clone();
        let storage = self.components.storage.clone();

        let handle = tokio::spawn(async move {
            if let Ok(mut hup) = signal(SignalKind::hangup()) {
                while hup.recv().await.is_some() {
                    if let Err(e) = handle_config_reload_with_managers(
                        &config_manager,
                        &peer_manager,
                        &storage,
                        &cfg_path,
                    )
                    .await
                    {
                        error!("config reload failed: {e}");
                    }
                }
            }
        });

        Ok(handle)
    }

    /// Start all server services
    pub async fn run(self, cfg_path: String) -> ServerResult<()> {
        // Create connection tracker for graceful shutdown
        let (tracker, _shutdown_rx) = ConnectionTracker::new();
        let tracker = Arc::new(tracker);

        // Start worker pool first
        let _worker_handles = self.worker_pool.start().await;

        self.start_peer_tasks().await?;

        // Start all listeners and background tasks
        let _tcp_handle = self.start_tcp_listener().await?;
        let _tls_handle = self.start_tls_listener().await?;
        let _ws_handle = self.start_websocket_bridge().await?;
        #[cfg(feature = "iroh")]
        let iroh_server = self.start_iroh_listener().await?;
        #[cfg(not(feature = "iroh"))]
        self.warn_if_iroh_configured().await;
        let _retention_handle = self.start_retention_cleanup().await?;
        let _config_handle = self.start_config_reload_handler(cfg_path).await?;
        let _usage_handle = self.start_usage_persistence().await?;

        // Wait for shutdown signal
        tokio::signal::ctrl_c().await?;
        info!("Shutdown signal received, starting graceful shutdown...");

        // Signal all components to stop accepting new work
        tracker.signal_shutdown();

        // Phase 1: Drain article queue (30 second timeout)
        let drain_start = std::time::Instant::now();
        let drain_timeout = Duration::from_secs(30);
        let queue_len = self.components.queue.len();
        if queue_len > 0 {
            info!("Draining article queue ({} items)...", queue_len);
        }
        while !self.components.queue.is_empty() {
            if drain_start.elapsed() > drain_timeout {
                warn!(
                    "Queue drain timeout exceeded ({} items remaining), proceeding with shutdown",
                    self.components.queue.len()
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if queue_len > 0 && self.components.queue.is_empty() {
            info!("Article queue drained successfully");
        }

        // Phase 2: Wait for active connections (30 second timeout)
        let conn_start = std::time::Instant::now();
        let conn_timeout = Duration::from_secs(30);
        let active = tracker.active_connections();
        if active > 0 {
            info!("Waiting for {} active connections to finish...", active);
        }
        while tracker.active_connections() > 0 {
            if conn_start.elapsed() > conn_timeout {
                warn!(
                    "Connection timeout exceeded ({} connections remaining), forcing shutdown",
                    tracker.active_connections()
                );
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        if active > 0 && tracker.active_connections() == 0 {
            info!("All connections closed gracefully");
        }

        #[cfg(feature = "iroh")]
        if let Some(iroh_server) = iroh_server
            && let Err(err) = iroh_server.shutdown().await
        {
            warn!(error = %err, "iroh shutdown failed");
        }

        info!("Shutdown complete");
        Ok(())
    }

    /// Start the iroh listener when `[iroh] enabled` is set.
    #[cfg(feature = "iroh")]
    async fn start_iroh_listener(&self) -> ServerResult<Option<crate::iroh::IrohServer>> {
        let (enabled, key_path, allowed_peers) = {
            let cfg = self.components.config.read().await;
            (
                cfg.iroh.enabled,
                cfg.iroh.key_path.clone(),
                cfg.iroh.allowed_peers.clone(),
            )
        };
        if !enabled {
            return Ok(None);
        }

        for peer in &allowed_peers {
            if peer.parse::<iroh::EndpointId>().is_err() {
                warn!("ignoring iroh allowlist entry that is not an endpoint id: {peer}");
            }
        }
        if allowed_peers.is_empty() {
            warn!("iroh allowlist is empty; every iroh peer will be rejected");
        }

        let secret_key = crate::iroh::load_or_create_secret_key(std::path::Path::new(&key_path))?;
        let parts = crate::iroh::IrohParts {
            storage: self.components.storage.clone(),
            auth: self.components.auth.clone(),
            config: self.components.config.clone(),
            queue: self.components.queue.clone(),
            usage_tracker: self.components.usage_tracker.clone(),
        };
        let server = crate::iroh::serve(
            parts,
            crate::iroh::IrohBind {
                secret_key,
                relay_mode: iroh::endpoint::RelayMode::Default,
                wait_for_relay: true,
                bind_addr: None,
            },
        )
        .await?;
        Ok(Some(server))
    }

    #[cfg(not(feature = "iroh"))]
    async fn warn_if_iroh_configured(&self) {
        if self.components.config.read().await.iroh.enabled {
            warn!(
                "iroh.enabled is set, but this binary was built without the `iroh` cargo feature"
            );
        }
    }
}

/// Configuration management for the server
#[derive(Clone)]
struct ConfigManager {
    config: Arc<RwLock<Config>>,
    tls_acceptor: Arc<RwLock<Option<TlsAcceptor>>>,
}

impl ConfigManager {
    fn new(config: Arc<RwLock<Config>>) -> Self {
        Self {
            config,
            tls_acceptor: Arc::new(RwLock::new(None)),
        }
    }

    async fn reload(&self, cfg_path: &str) -> ServerResult<()> {
        let new_cfg = Config::from_file(cfg_path)?;

        // Update TLS configuration if present
        if let (Some(cert), Some(key)) = (new_cfg.tls_cert.as_ref(), new_cfg.tls_key.as_ref()) {
            match load_tls_config(cert, key) {
                Ok(conf) => {
                    *self.tls_acceptor.write().await = Some(TlsAcceptor::from(Arc::new(conf)));
                }
                Err(e) => error!("failed to load tls config: {e}"),
            }
        }

        // Update runtime configuration
        self.config.write().await.update_runtime(new_cfg);
        info!("configuration reloaded");

        Ok(())
    }
}

/// Peer management for the server
#[derive(Clone)]
struct PeerManager {
    peer_db: PeerDb,
    scheduler: Arc<JobScheduler>,
    peer_jobs: Arc<DashMap<String, uuid::Uuid>>,
}

impl PeerManager {
    async fn new(peer_db: PeerDb) -> ServerResult<Self> {
        let scheduler = JobScheduler::new().await?;
        scheduler.start().await?;

        Ok(Self {
            peer_db,
            scheduler: Arc::new(scheduler),
            peer_jobs: Arc::new(DashMap::new()),
        })
    }

    async fn start_peer_tasks(
        &self,
        config: &Config,
        storage: Arc<dyn Storage>,
    ) -> ServerResult<()> {
        let default_schedule = config.peer_sync_schedule.clone();

        for peer in &config.peers {
            let pc = PeerConfig::from(peer);
            let name = pc.sitename.clone();

            match add_peer_job(
                &self.scheduler,
                pc,
                default_schedule.clone(),
                self.peer_db.clone(),
                storage.clone(),
                config.site_name.clone(),
            )
            .await
            {
                Ok(job_uuid) => {
                    self.peer_jobs.insert(name, job_uuid);
                }
                Err(e) => {
                    error!("Failed to add peer job for {}: {}", name, e);
                }
            }
        }

        Ok(())
    }

    async fn update_tasks(&self, new_cfg: &Config, storage: &Arc<dyn Storage>) -> ServerResult<()> {
        let names: Vec<String> = new_cfg.peers.iter().map(|p| p.sitename.clone()).collect();
        self.peer_db.sync_config(&names).await?;

        let default_schedule = new_cfg.peer_sync_schedule.clone();

        // Start new peer tasks
        for peer in &new_cfg.peers {
            if !self.peer_jobs.contains_key(&peer.sitename) {
                let pc = PeerConfig::from(peer);
                let name = pc.sitename.clone();

                match add_peer_job(
                    &self.scheduler,
                    pc,
                    default_schedule.clone(),
                    self.peer_db.clone(),
                    storage.clone(),
                    new_cfg.site_name.clone(),
                )
                .await
                {
                    Ok(job_uuid) => {
                        self.peer_jobs.insert(name, job_uuid);
                    }
                    Err(e) => {
                        error!("Failed to add peer job for {}: {}", name, e);
                    }
                }
            }
        }

        // Remove obsolete peer tasks
        let to_remove: Vec<String> = self
            .peer_jobs
            .iter()
            .filter(|entry| !new_cfg.peers.iter().any(|p| &p.sitename == entry.key()))
            .map(|entry| entry.key().clone())
            .collect();

        for name in to_remove {
            if let Some((_, job_uuid)) = self.peer_jobs.remove(&name)
                && let Err(e) = self.scheduler.remove(&job_uuid).await
            {
                error!("Failed to remove peer job for {}: {}", name, e);
            }
        }

        Ok(())
    }
}

/// Load TLS configuration from certificate and key files
///
/// # Arguments
/// * `cert_path` - Path to the certificate file in PEM format
/// * `key_path` - Path to the private key file in PKCS#8 format
///
/// # Errors
/// Returns an error if the files cannot be read or contain invalid data
fn load_tls_config(cert_path: &str, key_path: &str) -> ServerResult<rustls::ServerConfig> {
    let cert_file = &mut BufReader::new(File::open(cert_path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            anyhow::anyhow!(
                "TLS certificate file not found: '{cert_path}'

Please ensure the certificate file exists at the specified path.
For Let's Encrypt certificates, this is typically '/etc/letsencrypt/live/domain/fullchain.pem'."
            )
        }
        std::io::ErrorKind::PermissionDenied => {
            anyhow::anyhow!(
                "Permission denied reading TLS certificate file: '{cert_path}'

Please ensure the file is readable by the current user.
You may need to run as root or adjust file permissions."
            )
        }
        _ => anyhow::anyhow!("Failed to open TLS certificate file '{cert_path}': {e}"),
    })?);
    let key_file = &mut BufReader::new(File::open(key_path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => {
            anyhow::anyhow!(
                "TLS private key file not found: '{key_path}'

Please ensure the private key file exists at the specified path.
For Let's Encrypt certificates, this is typically '/etc/letsencrypt/live/domain/privkey.pem'."
            )
        }
        std::io::ErrorKind::PermissionDenied => {
            anyhow::anyhow!(
                "Permission denied reading TLS private key file: '{key_path}'

Please ensure the file is readable by the current user.
You may need to run as root or adjust file permissions.
Note: Private key files should be protected (mode 600)."
            )
        }
        _ => anyhow::anyhow!("Failed to open TLS private key file '{key_path}': {e}"),
    })?);

    let certs = certs(cert_file)
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to parse TLS certificate file '{cert_path}': {e}

Please ensure the certificate file is in valid PEM format.
The file should contain one or more certificates starting with '-----BEGIN CERTIFICATE-----'."
            )
        })?
        .into_iter()
        .map(rustls::Certificate)
        .collect();

    let mut keys = pkcs8_private_keys(key_file).map_err(|e| {
        anyhow::anyhow!(
            "Failed to parse TLS private key file '{key_path}': {e}

Please ensure the private key file is in valid PKCS#8 PEM format.
The file should contain a private key starting with '-----BEGIN PRIVATE KEY-----'.
If your key is in a different format, you may need to convert it:
- For RSA keys: openssl rsa -in old_key.pem -out new_key.pem
- For EC keys: openssl ec -in old_key.pem -out new_key.pem"
        )
    })?;

    if keys.is_empty() {
        return Err(anyhow::anyhow!(
            "No valid private key found in TLS key file '{key_path}'

Please ensure the file contains a valid PKCS#8 private key.
The file should have content starting with '-----BEGIN PRIVATE KEY-----'."
        ));
    }

    let key = rustls::PrivateKey(keys.remove(0));
    let config = rustls::ServerConfig::builder()
        .with_safe_defaults()
        .with_no_client_auth()
        .with_single_cert(certs, key)
        .map_err(|e| {
            anyhow::anyhow!(
                "Failed to create TLS configuration: {e}

This error typically occurs when:
- The certificate and private key don't match
- The certificate chain is incomplete
- The certificate has expired
- The certificate format is invalid

Please verify that your certificate and key files are correct and match each other."
            )
        })?;

    Ok(config)
}

/// Convert raw address string to a proper listen address
///
/// # Arguments
/// * `raw` - Raw address string (can be just port, :port, or full address)
///
/// # Returns
/// A properly formatted address string suitable for binding
fn listen_addr(raw: &str, localhost_default: bool) -> String {
    if raw.parse::<SocketAddr>().is_ok() {
        raw.to_string()
    } else if let Some(port) = raw.strip_prefix(':') {
        let host = if localhost_default {
            "127.0.0.1"
        } else {
            "0.0.0.0"
        };
        format!("{host}:{port}")
    } else {
        let host = if localhost_default {
            "127.0.0.1"
        } else {
            "0.0.0.0"
        };
        format!("{host}:{raw}")
    }
}

/// Try to get a systemd socket by name or bind directly to an address
///
/// # Arguments
/// * `addr_config` - Address configuration (can be socket name, systemd:// URL, or regular address)
///
/// # Returns
/// A TcpListener bound to the specified address or systemd socket
async fn get_listener(addr_config: &str, localhost_default: bool) -> ServerResult<TcpListener> {
    // First check for systemd:// URLs
    if addr_config.starts_with("systemd://") {
        match addr_config.parse::<systemd_socket::SocketAddr>() {
            Ok(socket_addr) => {
                match socket_addr.bind() {
                    Ok(std_listener) => {
                        // Convert std::net::TcpListener to tokio::net::TcpListener
                        match std_listener.set_nonblocking(true) {
                            Ok(()) => match TcpListener::from_std(std_listener) {
                                Ok(listener) => {
                                    info!("using systemd socket: {addr_config}");
                                    Ok(listener)
                                }
                                Err(e) => {
                                    Err(anyhow::anyhow!("failed to convert socket to tokio: {e}"))
                                }
                            },
                            Err(e) => {
                                Err(anyhow::anyhow!("failed to set socket to non-blocking: {e}"))
                            }
                        }
                    }
                    Err(e) => Err(anyhow::anyhow!(
                        "Failed to bind to address '{addr_config}': {e}

This error typically occurs when:
- Another process is already using this port (try: lsof -i :<port> or netstat -tlnp | grep :<port>)
- The port number is invalid (must be 1-65535)
- Permission denied for privileged ports (<1024) - try running as root or use a port ≥1024
- The address format is incorrect (should be 'host:port', ':port', or just 'port')
- For systemd socket activation, the socket is not available

You can use 'systemd://socket_name' format for systemd socket activation."
                    )),
                }
            }
            Err(e) => Err(anyhow::anyhow!(
                "Invalid systemd socket address '{addr_config}': {e}"
            )),
        }
    } else {
        // For regular addresses, use our own parsing logic
        let addr = listen_addr(addr_config, localhost_default);
        info!("listening on {addr}");
        match TcpListener::bind(&addr).await {
            Ok(listener) => Ok(listener),
            Err(e) => Err(anyhow::anyhow!(
                "Failed to bind to address '{}': {}

This error typically occurs when:
- Another process is already using this port (try: lsof -i :{} or netstat -tlnp | grep :{})
- The port number is invalid (must be 1-65535)
- Permission denied for privileged ports (<1024) - try running as root or use a port ≥1024
- The address format is incorrect (should be 'host:port', ':port', or just 'port')

You can use 'systemd://socket_name' format for systemd socket activation.",
                addr_config,
                e,
                addr.split(':').next_back().unwrap_or("119"),
                addr.split(':').next_back().unwrap_or("119")
            )),
        }
    }
}

/// Handle an incoming client connection
async fn handle_connection<S>(
    socket: S,
    storage: Arc<dyn Storage>,
    auth: Arc<dyn AuthProvider>,
    config: Arc<RwLock<Config>>,
    is_tls: bool,
    queue: ArticleQueue,
    usage_tracker: Arc<UsageTracker>,
) where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        if let Err(e) =
            crate::handle_client(socket, storage, auth, config, is_tls, queue, usage_tracker).await
        {
            error!("client error: {e}");
        }
    });
}

/// Main server entry point
///
/// This function initializes the server and starts all necessary components:
/// - TCP and TLS listeners for NNTP connections
/// - WebSocket bridge (if enabled)
/// - Peer synchronization tasks
/// - Retention cleanup task
/// - Configuration reload handler
///
/// # Arguments
/// * `cfg_initial` - Initial server configuration
/// * `cfg_path` - Path to configuration file for reloading
///
/// # Errors
/// Returns an error if server initialization or startup fails
pub async fn run(cfg_initial: Config, cfg_path: String) -> ServerResult<()> {
    let server = Server::new(cfg_initial).await?;
    server.run(cfg_path).await
}

/// Handle a single configuration reload using managers
///
/// # Arguments
/// * `config_manager` - Configuration manager
/// * `peer_manager` - Peer manager
/// * `storage` - Storage backend
/// * `cfg_path` - Path to configuration file
///
/// # Errors
/// Returns an error if configuration reload fails
async fn handle_config_reload_with_managers(
    config_manager: &ConfigManager,
    peer_manager: &PeerManager,
    storage: &Arc<dyn Storage>,
    cfg_path: &str,
) -> ServerResult<()> {
    let new_cfg = Config::from_file(cfg_path)?;

    // Update configuration using manager
    config_manager.reload(cfg_path).await?;

    // Update peer configuration using manager
    peer_manager.update_tasks(&new_cfg, storage).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::listen_addr;

    #[test]
    fn hostless_addr_binds_localhost_for_iroh() {
        assert_eq!(listen_addr(":119", true), "127.0.0.1:119");
        assert_eq!(listen_addr("119", true), "127.0.0.1:119");
        assert_eq!(listen_addr(":119", false), "0.0.0.0:119");
        assert_eq!(listen_addr("0.0.0.0:119", true), "0.0.0.0:119");
        assert_eq!(listen_addr("127.0.0.1:1119", false), "127.0.0.1:1119");
    }
}
