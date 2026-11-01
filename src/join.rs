//! Public join channel (`/onix/nntp-join/1`).
//!
//! When `[iroh.join_requests] enable` is true, any endpoint may submit one
//! small request. The endpoint id is taken from the iroh handshake, not from
//! the JSON. The channel cannot read or post articles.

use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::bail;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::admin::{self, RuntimeStore};
use crate::auth::DynAuth;
use crate::config::{Config, JoinRequestsConfig};

/// ALPN for join requests. Distinct from NNTP and admin.
pub const JOIN_ALPN: &[u8] = b"/onix/nntp-join/1";

const MAX_FRAME: usize = 2048;
const HOUR: i64 = 3600;

/// One stored join request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JoinRow {
    pub endpoint_id: String,
    pub name: String,
    pub owner: String,
    pub note: String,
    pub status: String,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Caps read from config.
#[derive(Debug, Clone)]
pub struct JoinLimits {
    pub per_endpoint_per_hour: u32,
    pub global_per_hour: u32,
    pub max_pending: u32,
    pub ttl_secs: i64,
}

impl From<&JoinRequestsConfig> for JoinLimits {
    fn from(cfg: &JoinRequestsConfig) -> Self {
        Self {
            per_endpoint_per_hour: cfg.per_endpoint_per_hour,
            global_per_hour: cfg.global_per_hour,
            max_pending: cfg.max_pending,
            ttl_secs: i64::from(cfg.ttl_days).saturating_mul(86_400),
        }
    }
}

pub(crate) fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

fn validate_owner(owner: &str) -> anyhow::Result<()> {
    if owner.is_empty() || owner.len() > 64 || owner.chars().any(char::is_control) {
        bail!("owner must be a single line of 1-64 characters");
    }
    Ok(())
}

fn validate_note(note: &str) -> anyhow::Result<()> {
    if note.len() > 160 || note.chars().any(char::is_control) {
        bail!("note must be a single line of at most 160 characters");
    }
    Ok(())
}

/// Record or refresh a join request.
///
/// # Errors
///
/// Returns an error for a bad name, a taken name, a full queue, or a rate limit.
/// An existing approved or rejected row is returned without those checks when
/// the caller is only polling the same endpoint.
/// Fields the peer is allowed to send. The endpoint id is not one of them.
pub struct JoinInput<'a> {
    pub name: &'a str,
    pub owner: &'a str,
    pub note: &'a str,
}

pub async fn submit_join(
    store: &RuntimeStore,
    auth: &DynAuth,
    limits: &JoinLimits,
    endpoint_id: &str,
    input: JoinInput<'_>,
    now: i64,
) -> anyhow::Result<JoinRow> {
    let JoinInput { name, owner, note } = input;
    admin::validate_username(name)?;
    validate_owner(owner)?;
    validate_note(note)?;
    store
        .expire_pending(now.saturating_sub(limits.ttl_secs))
        .await?;

    if let Some(existing) = store.get_join(endpoint_id).await? {
        if existing.status == "approved" {
            return Ok(existing);
        }
        if existing.status == "pending"
            && existing.name == name
            && existing.owner == owner
            && existing.note == note
        {
            return Ok(existing);
        }
    }

    store.prune_hits(now.saturating_sub(HOUR)).await?;
    let endpoint_hits = store
        .count_hits(endpoint_id, now.saturating_sub(HOUR))
        .await?;
    if endpoint_hits >= u64::from(limits.per_endpoint_per_hour) {
        bail!("join rate limit for this endpoint");
    }
    let global_hits = store.count_hits("global", now.saturating_sub(HOUR)).await?;
    if global_hits >= u64::from(limits.global_per_hour) {
        bail!("join rate limit");
    }

    let names = auth.list_usernames().await?;
    if names.iter().any(|existing| existing == name) || store.name_in_use(name, endpoint_id).await?
    {
        bail!("name {name} is already taken");
    }

    let existing = store.get_join(endpoint_id).await?;
    let is_new_pending = !matches!(
        existing.as_ref().map(|row| row.status.as_str()),
        Some("pending")
    );
    if is_new_pending && store.count_pending().await? >= u64::from(limits.max_pending) {
        bail!("join queue is full");
    }

    let created_at = existing
        .as_ref()
        .filter(|row| row.status != "rejected")
        .map(|row| row.created_at)
        .unwrap_or(now);
    let row = JoinRow {
        endpoint_id: endpoint_id.to_string(),
        name: name.to_string(),
        owner: owner.to_string(),
        note: note.to_string(),
        status: "pending".to_string(),
        created_at,
        updated_at: now,
    };
    store.upsert_join(&row).await?;
    store.add_hit(endpoint_id, now).await?;
    store.add_hit("global", now).await?;
    tracing::info!(endpoint_id, name, owner, "join request");
    Ok(row)
}

/// Approve a pending request: allowlist the endpoint and bind the account.
///
/// # Errors
///
/// Returns an error when the request is missing or the name is taken.
pub async fn approve_request(
    store: &RuntimeStore,
    auth: &DynAuth,
    endpoint_id: &str,
    rename: Option<&str>,
    now: i64,
) -> anyhow::Result<JoinRow> {
    let Some(mut row) = store.get_join(endpoint_id).await? else {
        bail!("no join request for {endpoint_id}");
    };
    if row.status == "approved" {
        return Ok(row);
    }
    if let Some(name) = rename {
        admin::validate_username(name)?;
        row.name = name.to_string();
    }
    let names = auth.list_usernames().await?;
    if names.iter().any(|existing| existing == &row.name)
        || store.name_in_use(&row.name, endpoint_id).await?
    {
        bail!("name {} is already taken", row.name);
    }
    let password = admin::generate_password();
    auth.add_user(&row.name, &password).await?;
    store.bind_endpoint(endpoint_id, &row.name).await?;
    store.upsert(endpoint_id, &row.owner).await?;
    row.status = "approved".to_string();
    row.updated_at = now;
    store.upsert_join(&row).await?;
    tracing::info!(endpoint_id, name = %row.name, "join request approved");
    Ok(row)
}

/// Mark a pending request rejected.
///
/// # Errors
///
/// Returns an error when no pending request matches.
pub async fn reject_request(
    store: &RuntimeStore,
    endpoint_id: &str,
    now: i64,
) -> anyhow::Result<JoinRow> {
    let Some(mut row) = store.get_join(endpoint_id).await? else {
        bail!("no join request for {endpoint_id}");
    };
    row.status = "rejected".to_string();
    row.updated_at = now;
    store.upsert_join(&row).await?;
    tracing::info!(endpoint_id, name = %row.name, "join request rejected");
    Ok(row)
}

pub struct JoinContext {
    pub auth: DynAuth,
    pub config: Arc<tokio::sync::RwLock<Config>>,
    pub runtime: RuntimeStore,
}

pub struct JoinProtocol {
    pub ctx: JoinContext,
}

impl std::fmt::Debug for JoinProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JoinProtocol").finish_non_exhaustive()
    }
}

impl JoinProtocol {
    async fn enabled(&self) -> bool {
        self.ctx.config.read().await.iroh.join_requests.enable
    }
}

impl ProtocolHandler for JoinProtocol {
    async fn on_accepting(
        &self,
        accepting: iroh::endpoint::Accepting,
    ) -> Result<Connection, AcceptError> {
        let connection = accepting.await?;
        if !self.enabled().await {
            tracing::warn!(
                endpoint_id = %connection.remote_id(),
                "rejected join request; join_requests.enable is false"
            );
            connection.close(1u32.into(), b"join requests disabled");
            return Err(AcceptError::from_err(JoinDisabled));
        }
        Ok(connection)
    }

    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        if !self.enabled().await {
            connection.close(1u32.into(), b"join requests disabled");
            return Err(AcceptError::from_err(JoinDisabled));
        }
        let endpoint_id = connection.remote_id().to_string();
        tracing::info!(endpoint_id, "join connection");
        let (send, mut recv) = connection
            .accept_bi()
            .await
            .map_err(AcceptError::from_err)?;
        let mut send = send;
        let bytes = read_frame(&mut recv).await.map_err(accept_err)?;
        let request: Value = serde_json::from_slice(&bytes).map_err(AcceptError::from_err)?;
        let name = request.get("name").and_then(Value::as_str).unwrap_or("");
        let owner = request.get("owner").and_then(Value::as_str).unwrap_or("");
        let note = request.get("note").and_then(Value::as_str).unwrap_or("");
        let limits = JoinLimits::from(&self.ctx.config.read().await.iroh.join_requests);
        let response = match submit_join(
            &self.ctx.runtime,
            &self.ctx.auth,
            &limits,
            &endpoint_id,
            JoinInput { name, owner, note },
            now_secs(),
        )
        .await
        {
            Ok(row) => json!({
                "status": row.status,
                "endpoint_id": row.endpoint_id,
                "name": row.name,
                "username": row.name,
                "owner": row.owner,
            }),
            Err(err) => {
                tracing::warn!(endpoint_id, error = %err, "join request failed");
                json!({"status": "error", "error": err.to_string(), "endpoint_id": endpoint_id})
            }
        };
        let encoded = serde_json::to_vec(&response).map_err(AcceptError::from_err)?;
        write_frame(&mut send, &encoded).await.map_err(accept_err)?;
        let _ = tokio::time::timeout(Duration::from_secs(2), connection.closed()).await;
        Ok(())
    }
}

fn accept_err(err: anyhow::Error) -> AcceptError {
    AcceptError::from_err(JoinIo(err.to_string()))
}

#[derive(Debug)]
struct JoinIo(String);

impl std::fmt::Display for JoinIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for JoinIo {}

#[derive(Debug)]
struct JoinDisabled;

impl std::fmt::Display for JoinDisabled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("join requests disabled")
    }
}

impl std::error::Error for JoinDisabled {}

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = usize::try_from(u32::from_be_bytes(len_buf))?;
    if len > MAX_FRAME {
        bail!("join request exceeds {MAX_FRAME} bytes");
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, bytes: &[u8]) -> anyhow::Result<()> {
    if bytes.len() > MAX_FRAME {
        bail!("join response exceeds {MAX_FRAME} bytes");
    }
    let len = u32::try_from(bytes.len())?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn limits() -> JoinLimits {
        JoinLimits {
            per_endpoint_per_hour: 1,
            global_per_hour: 3,
            max_pending: 2,
            ttl_secs: 100,
        }
    }

    async fn store() -> (tempfile::TempDir, RuntimeStore, DynAuth) {
        let dir = tempfile::tempdir().unwrap();
        let uri = format!("sqlite://{}", dir.path().join("runtime.db").display());
        let store = RuntimeStore::open(&uri).await.unwrap();
        let auth = crate::auth::open("sqlite::memory:").await.unwrap();
        (dir, store, auth)
    }

    #[tokio::test]
    async fn dedupe_rate_limit_expiry_and_taken_name() {
        let (_dir, store, auth) = store().await;
        let limits = limits();
        let first = submit_join(
            &store,
            &auth,
            &limits,
            "ep-a",
            JoinInput {
                name: "ada",
                owner: "Ada",
                note: "",
            },
            1_000,
        )
        .await
        .unwrap();
        assert_eq!(first.status, "pending");
        let again = submit_join(
            &store,
            &auth,
            &limits,
            "ep-a",
            JoinInput {
                name: "ada",
                owner: "Ada",
                note: "",
            },
            1_010,
        )
        .await
        .unwrap();
        assert_eq!(again.created_at, first.created_at);
        assert_eq!(store.count_hits("ep-a", 0).await.unwrap(), 1);

        let err = submit_join(
            &store,
            &auth,
            &limits,
            "ep-b",
            JoinInput {
                name: "ada",
                owner: "Bea",
                note: "",
            },
            1_020,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("already taken"), "{err}");

        let err = submit_join(
            &store,
            &auth,
            &limits,
            "ep-a",
            JoinInput {
                name: "ada2",
                owner: "Ada",
                note: "n",
            },
            1_040,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("rate limit"), "{err}");

        submit_join(
            &store,
            &auth,
            &limits,
            "ep-b",
            JoinInput {
                name: "bea",
                owner: "Bea",
                note: "",
            },
            1_020,
        )
        .await
        .unwrap();
        let err = submit_join(
            &store,
            &auth,
            &limits,
            "ep-c",
            JoinInput {
                name: "cy",
                owner: "Cy",
                note: "",
            },
            1_030,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("queue is full"), "{err}");

        store
            .expire_pending(1_000 + limits.ttl_secs + 1)
            .await
            .unwrap();
        assert_eq!(store.count_pending().await.unwrap(), 0);

        auth.add_user("ada", "secret").await.unwrap();
        let err = submit_join(
            &store,
            &auth,
            &limits,
            "ep-d",
            JoinInput {
                name: "ada",
                owner: "Ada",
                note: "",
            },
            10_000,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("already taken"), "{err}");
    }

    #[test]
    fn rejects_bad_username() {
        assert!(admin::validate_username("Ada").is_err());
        assert!(admin::validate_username("ok-name").is_ok());
    }
}
