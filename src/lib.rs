pub mod parse;
pub use parse::{
    Command, Message, Response, ensure_date, ensure_message_id, parse_command, parse_datetime,
    parse_message, parse_range, parse_response,
};

#[cfg(feature = "iroh")]
pub mod admin;
pub mod auth;
pub mod config;
pub mod control;
pub mod error;
pub mod filters;
#[cfg(feature = "iroh")]
pub mod gossip;
pub mod groups;
pub mod handlers;
pub mod identity;
#[cfg(feature = "iroh")]
pub mod iroh;
#[cfg(feature = "iroh")]
pub mod join;
pub mod limits;
pub mod notify;
pub mod overview;
pub mod peers;
pub mod prelude;
pub mod queue;
pub mod responses;
pub mod retention;
pub mod server;
pub mod session;
pub mod storage;
pub mod wildmat;
#[cfg(feature = "websocket")]
pub mod ws;

use crate::auth::DynAuth;
use crate::config::Config;
use crate::handlers::{HandlerContext, dispatch_command};
use crate::limits::UsageTracker;
use crate::queue::ArticleQueue;
use crate::session::Session;
use crate::storage::DynStorage;
use anyhow::Result;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{self, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::sync::RwLock;
use tracing::{Instrument, debug, info_span};

/// Per-connection cached configuration values.
/// These are read once at connection start and not updated mid-connection.
struct ConnectionConfig {
    #[allow(dead_code)]
    site_name: String,
    idle_timeout: Duration,
}

/// Handle a client connection.
///
/// # Errors
///
/// Returns an error if there's a problem handling the client connection,
/// such as network I/O errors or protocol violations.
/// Key-bound identity for an iroh NNTP connection.
pub struct KeyAuth {
    pub endpoint_id: String,
    /// Set when this endpoint is already bound to an account.
    pub username: Option<String>,
    pub bindings: std::sync::Arc<dyn crate::identity::UserEndpoint>,
}

pub async fn handle_client<S>(
    socket: S,
    storage: DynStorage,
    auth: DynAuth,
    cfg: Arc<RwLock<Config>>,
    is_tls: bool,
    queue: ArticleQueue,
    usage_tracker: Arc<UsageTracker>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    handle_client_key(
        socket,
        storage,
        auth,
        cfg,
        is_tls,
        queue,
        usage_tracker,
        None,
    )
    .await
}

/// Same as [`handle_client`], with optional iroh key authentication.
///
/// # Errors
///
/// Returns an error if there's a problem handling the client connection.
#[allow(clippy::too_many_arguments)]
pub async fn handle_client_key<S>(
    socket: S,
    storage: DynStorage,
    auth: DynAuth,
    cfg: Arc<RwLock<Config>>,
    is_tls: bool,
    queue: ArticleQueue,
    usage_tracker: Arc<UsageTracker>,
    key_auth: Option<KeyAuth>,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    use crate::responses::*;

    let (read_half, write_half) = io::split(socket);
    let reader = BufReader::new(read_half);

    // Cache configuration values at connection start so they don't change mid-connection
    let (connection_config, allow_auth_insecure, allow_anonymous_posting) = {
        let cfg_guard = cfg.read().await;
        (
            ConnectionConfig {
                site_name: cfg_guard.site_name.clone(),
                idle_timeout: Duration::from_secs(cfg_guard.idle_timeout_secs),
            },
            cfg_guard.allow_auth_insecure_connections,
            cfg_guard.allow_anonymous_posting,
        )
    };

    let mut session = Session::new(is_tls, allow_auth_insecure, allow_anonymous_posting);
    if let Some(key) = key_auth {
        session.set_peer(key.endpoint_id, key.bindings);
        if let Some(username) = key.username {
            let is_admin = auth.is_admin(&username).await.unwrap_or(false);
            session.authenticate_with_admin(username, is_admin);
        }
    }
    let session_id = session.session_id();

    // Create session span - NO client_addr for GDPR compliance
    let session_span = info_span!(
        "session",
        session_id = %session_id,
        is_tls = is_tls,
        commands_processed = tracing::field::Empty,
        duration_ms = tracing::field::Empty,
    );

    // Run the connection handling within the session span
    async move {
        let start = Instant::now();
        let mut commands_processed: u64 = 0;

        let mut ctx = HandlerContext {
            reader: Box::pin(reader),
            writer: Box::pin(write_half),
            storage,
            auth,
            config: cfg,
            session,
            queue,
            usage_tracker,
        };

        // Send greeting - reflects current posting ability
        if ctx.session.can_post() {
            ctx.writer.write_all(RESP_200_READY.as_bytes()).await?;
        } else {
            ctx.writer
                .write_all(RESP_201_READY_NO_POST.as_bytes())
                .await?;
        }

        let mut line = String::new();
        loop {
            line.clear();

            // Apply timeout to the read operation using cached idle_timeout
            let read_result = tokio::time::timeout(
                connection_config.idle_timeout,
                ctx.reader.read_line(&mut line),
            )
            .await;

            let n = match read_result {
                Ok(Ok(n)) => n,
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {
                    // Timeout occurred
                    debug!(
                        timeout_secs = connection_config.idle_timeout.as_secs(),
                        "Connection timed out"
                    );
                    break;
                }
            };

            if n == 0 {
                break;
            }

            let trimmed = line.trim_end_matches(['\r', '\n']);
            let Ok((_, cmd)) = parse_command(trimmed) else {
                ctx.writer.write_all(RESP_500_SYNTAX.as_bytes()).await?;
                continue;
            };

            commands_processed += 1;

            // Create command span with timing
            let cmd_span = info_span!(
                "command",
                name = %cmd.name,
                duration_ms = tracing::field::Empty,
            );
            let cmd_start = Instant::now();

            // Handle QUIT specially since it needs to break the loop
            if cmd.name.as_str() == "QUIT" {
                async {
                    ctx.writer.write_all(RESP_205_CLOSING.as_bytes()).await?;
                    ctx.writer.flush().await
                }
                .instrument(cmd_span.clone())
                .await?;
                cmd_span.record("duration_ms", cmd_start.elapsed().as_millis() as u64);
                break;
            }

            // Dispatch command within span
            let result = async { dispatch_command(&mut ctx, &cmd).await }
                .instrument(cmd_span.clone())
                .await;

            cmd_span.record("duration_ms", cmd_start.elapsed().as_millis() as u64);

            if let Err(e) = result {
                // Log the error but continue processing other commands
                debug!(command = %cmd.name, error = %e, "Command failed");
            }
        }

        // Record final session metrics
        tracing::Span::current().record("commands_processed", commands_processed);
        tracing::Span::current().record("duration_ms", start.elapsed().as_millis() as u64);

        // Clean up connection tracking for authenticated users (admins are not tracked)
        if ctx.session.is_authenticated() && !ctx.session.is_admin() {
            if let Some(username) = ctx.session.username() {
                ctx.usage_tracker.disconnect(username);
            }
        }

        tracing::info!("Session ended");

        Ok(())
    }
    .instrument(session_span)
    .await
}
