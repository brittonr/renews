//! Admin channel for the iroh listener.
//!
//! Peers listed in `[iroh] admin_peers` may open `/onix/nntp-admin/1` and send
//! one length-prefixed JSON request per bi-stream. Bot `allowed_peers` are not
//! admins. Peers added here are stored in `iroh-runtime.db` and are allowed to
//! speak NNTP alongside the declarative allowlist. A peer that came from the
//! config file cannot be removed by this channel.

use std::path::Path;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context as _, bail};
use base64::Engine;
use dashmap::DashSet;
use futures_util::StreamExt;
use iroh::EndpointId;
use iroh::endpoint::Connection;
use iroh::protocol::{AcceptError, ProtocolHandler};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::auth::DynAuth;
use crate::config::Config;
use crate::handlers::utils::get_header_value;
use crate::storage::DynStorage;

/// ALPN for the admin channel. Distinct from NNTP (`/onix/nntp/1`).
pub const ADMIN_ALPN: &[u8] = b"/onix/nntp-admin/1";

const MAX_FRAME: usize = 1024 * 1024;

/// Endpoint ids with an open stream, for `status`.
#[derive(Debug, Default)]
pub struct LivePeers {
    pub nntp: DashSet<String>,
    pub admin: DashSet<String>,
}

/// Runtime peers and disabled accounts. Survives reload; not part of the Nix file.
#[derive(Clone)]
pub struct RuntimeStore {
    pool: sqlx::SqlitePool,
}

impl RuntimeStore {
    /// Open or create the runtime database.
    ///
    /// # Errors
    ///
    /// Returns an error if SQLite cannot open the file or create the tables.
    pub async fn open(uri: &str) -> anyhow::Result<Self> {
        let options = sqlx::sqlite::SqliteConnectOptions::from_str(uri)
            .map_err(|err| anyhow::anyhow!("iroh runtime db {uri}: {err}"))?
            .create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(4)
            .connect_with(options)
            .await
            .with_context(|| format!("connecting to iroh runtime db {uri}"))?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS runtime_peers (
                endpoint_id TEXT PRIMARY KEY,
                label TEXT NOT NULL,
                added_at TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS disabled_accounts (
                username TEXT PRIMARY KEY,
                disabled_at TEXT NOT NULL
            )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS join_requests (
                endpoint_id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                owner TEXT NOT NULL,
                note TEXT NOT NULL DEFAULT '',
                status TEXT NOT NULL,
                created_at INTEGER NOT NULL,
                updated_at INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS endpoint_accounts (
                endpoint_id TEXT PRIMARY KEY,
                username TEXT NOT NULL UNIQUE
            )",
        )
        .execute(&pool)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS join_hits (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scope TEXT NOT NULL,
                at INTEGER NOT NULL
            )",
        )
        .execute(&pool)
        .await?;
        Ok(Self { pool })
    }

    async fn peers(&self) -> anyhow::Result<Vec<(String, String)>> {
        let rows = sqlx::query("SELECT endpoint_id, label FROM runtime_peers ORDER BY endpoint_id")
            .fetch_all(&self.pool)
            .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push((row.try_get("endpoint_id")?, row.try_get("label")?));
        }
        Ok(out)
    }

    async fn contains(&self, endpoint_id: &str) -> anyhow::Result<bool> {
        let row = sqlx::query("SELECT 1 FROM runtime_peers WHERE endpoint_id = ?")
            .bind(endpoint_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    pub(crate) async fn upsert(&self, endpoint_id: &str, label: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO runtime_peers (endpoint_id, label, added_at) VALUES (?, ?, ?)
             ON CONFLICT(endpoint_id) DO UPDATE SET label = excluded.label",
        )
        .bind(endpoint_id)
        .bind(label)
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn delete_peer(&self, endpoint_id: &str) -> anyhow::Result<bool> {
        let result = sqlx::query("DELETE FROM runtime_peers WHERE endpoint_id = ?")
            .bind(endpoint_id)
            .execute(&self.pool)
            .await?;
        Ok(result.rows_affected() > 0)
    }

    async fn disable(&self, username: &str) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO disabled_accounts (username, disabled_at) VALUES (?, ?)
             ON CONFLICT(username) DO UPDATE SET disabled_at = excluded.disabled_at",
        )
        .bind(username)
        .bind(chrono::Utc::now().to_rfc3339())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn enable(&self, username: &str) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM disabled_accounts WHERE username = ?")
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn is_disabled(&self, username: &str) -> anyhow::Result<bool> {
        let row = sqlx::query("SELECT 1 FROM disabled_accounts WHERE username = ?")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    pub(crate) async fn expire_pending(&self, before: i64) -> anyhow::Result<u64> {
        let result =
            sqlx::query("DELETE FROM join_requests WHERE status = 'pending' AND created_at < ?")
                .bind(before)
                .execute(&self.pool)
                .await?;
        Ok(result.rows_affected())
    }

    pub(crate) async fn get_join(
        &self,
        endpoint_id: &str,
    ) -> anyhow::Result<Option<crate::join::JoinRow>> {
        let row = sqlx::query(
            "SELECT endpoint_id, name, owner, note, status, created_at, updated_at FROM join_requests WHERE endpoint_id = ?",
        )
        .bind(endpoint_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| crate::join::JoinRow {
            endpoint_id: row.get("endpoint_id"),
            name: row.get("name"),
            owner: row.get("owner"),
            note: row.get("note"),
            status: row.get("status"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }))
    }

    pub(crate) async fn upsert_join(&self, row: &crate::join::JoinRow) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO join_requests (endpoint_id, name, owner, note, status, created_at, updated_at)
             VALUES (?, ?, ?, ?, ?, ?, ?)
             ON CONFLICT(endpoint_id) DO UPDATE SET
               name = excluded.name,
               owner = excluded.owner,
               note = excluded.note,
               status = excluded.status,
               updated_at = excluded.updated_at",
        )
        .bind(&row.endpoint_id)
        .bind(&row.name)
        .bind(&row.owner)
        .bind(&row.note)
        .bind(&row.status)
        .bind(row.created_at)
        .bind(row.updated_at)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    pub(crate) async fn count_pending(&self) -> anyhow::Result<u64> {
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM join_requests WHERE status = 'pending'")
                .fetch_one(&self.pool)
                .await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    pub(crate) async fn name_in_use(
        &self,
        name: &str,
        except_endpoint: &str,
    ) -> anyhow::Result<bool> {
        let row = sqlx::query(
            "SELECT 1 FROM join_requests WHERE name = ? AND endpoint_id != ? AND status IN ('pending', 'approved')",
        )
        .bind(name)
        .bind(except_endpoint)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    pub(crate) async fn list_joins(
        &self,
        status: &str,
    ) -> anyhow::Result<Vec<crate::join::JoinRow>> {
        let rows = sqlx::query(
            "SELECT endpoint_id, name, owner, note, status, created_at, updated_at FROM join_requests WHERE status = ? ORDER BY created_at",
        )
        .bind(status)
        .fetch_all(&self.pool)
        .await?;
        Ok(rows
            .into_iter()
            .map(|row| crate::join::JoinRow {
                endpoint_id: row.get("endpoint_id"),
                name: row.get("name"),
                owner: row.get("owner"),
                note: row.get("note"),
                status: row.get("status"),
                created_at: row.get("created_at"),
                updated_at: row.get("updated_at"),
            })
            .collect())
    }

    pub(crate) async fn find_join_by_name(
        &self,
        name: &str,
    ) -> anyhow::Result<Option<crate::join::JoinRow>> {
        let row = sqlx::query(
            "SELECT endpoint_id, name, owner, note, status, created_at, updated_at FROM join_requests WHERE name = ? ORDER BY updated_at DESC",
        )
        .bind(name)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|row| crate::join::JoinRow {
            endpoint_id: row.get("endpoint_id"),
            name: row.get("name"),
            owner: row.get("owner"),
            note: row.get("note"),
            status: row.get("status"),
            created_at: row.get("created_at"),
            updated_at: row.get("updated_at"),
        }))
    }

    pub(crate) async fn prune_hits(&self, before: i64) -> anyhow::Result<()> {
        sqlx::query("DELETE FROM join_hits WHERE at < ?")
            .bind(before)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub(crate) async fn count_hits(&self, scope: &str, since: i64) -> anyhow::Result<u64> {
        let n: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM join_hits WHERE scope = ? AND at >= ?")
                .bind(scope)
                .bind(since)
                .fetch_one(&self.pool)
                .await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    pub(crate) async fn add_hit(&self, scope: &str, at: i64) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO join_hits (scope, at) VALUES (?, ?)")
            .bind(scope)
            .bind(at)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    pub async fn user_for_endpoint(&self, endpoint_id: &str) -> anyhow::Result<Option<String>> {
        let row = sqlx::query("SELECT username FROM endpoint_accounts WHERE endpoint_id = ?")
            .bind(endpoint_id)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| row.get("username")))
    }

    pub async fn bound_endpoint(&self, username: &str) -> anyhow::Result<Option<String>> {
        let row = sqlx::query("SELECT endpoint_id FROM endpoint_accounts WHERE username = ?")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.map(|row| row.get("endpoint_id")))
    }

    pub(crate) async fn bind_endpoint(
        &self,
        endpoint_id: &str,
        username: &str,
    ) -> anyhow::Result<()> {
        sqlx::query(
            "INSERT INTO endpoint_accounts (endpoint_id, username) VALUES (?, ?)
             ON CONFLICT(endpoint_id) DO UPDATE SET username = excluded.username",
        )
        .bind(endpoint_id)
        .bind(username)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}

#[async_trait::async_trait]
impl crate::identity::UserEndpoint for RuntimeStore {
    async fn endpoint_for_user(&self, username: &str) -> anyhow::Result<Option<String>> {
        self.bound_endpoint(username).await
    }
}

/// SQLite URI for the runtime database next to an iroh secret key.
#[must_use]
pub fn runtime_db_uri(key_path: &Path) -> String {
    let db = key_path.with_file_name("iroh-runtime.db");
    format!("sqlite://{}", db.display())
}

/// True when `remote` parses equal to any id in `list`.
#[must_use]
pub fn endpoint_in_list(list: &[String], remote: EndpointId) -> bool {
    list.iter().any(|entry| {
        entry
            .parse::<EndpointId>()
            .is_ok_and(|allowed| allowed == remote)
    })
}

/// Canonical hex form of an endpoint id.
///
/// # Errors
///
/// Returns an error when `raw` is not an endpoint id.
pub fn canonical_endpoint(raw: &str) -> anyhow::Result<String> {
    let id: EndpointId = raw
        .parse()
        .map_err(|_| anyhow::anyhow!("endpoint id is not 64 hex characters"))?;
    Ok(id.to_string())
}

pub(crate) fn validate_username(name: &str) -> anyhow::Result<()> {
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

fn validate_label(label: &str) -> anyhow::Result<()> {
    if label.is_empty() || label.len() > 64 || label.chars().any(char::is_control) {
        bail!("label must be a single line of 1-64 characters");
    }
    Ok(())
}

fn validate_group(name: &str) -> anyhow::Result<()> {
    let bytes = name.as_bytes();
    let ok = !bytes.is_empty()
        && bytes.len() <= 128
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(*b, b'.' | b'+' | b'-' | b'_'));
    if !ok {
        bail!("group name must look like bots.general");
    }
    Ok(())
}

pub(crate) fn generate_password() -> String {
    use rand_core::RngCore;
    let mut bytes = [0u8; 24];
    rand_core::OsRng.fill_bytes(&mut bytes);
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

fn require_confirm(args: &Value) -> anyhow::Result<()> {
    if args.get("confirm").and_then(Value::as_bool) != Some(true) {
        bail!("this operation needs confirm: true");
    }
    Ok(())
}

/// Refuse to delete an endpoint that the config file (Nix) declared.
///
/// # Errors
///
/// Returns an error when `endpoint_id` is in `declared`.
pub fn refuse_declarative_remove(declared: &[String], endpoint_id: &str) -> anyhow::Result<()> {
    let id: EndpointId = endpoint_id
        .parse()
        .map_err(|_| anyhow::anyhow!("endpoint id is not 64 hex characters"))?;
    if endpoint_in_list(declared, id) {
        bail!(
            "endpoint is declared in allowed_peers and cannot be removed at runtime; edit the NixOS allowedPeers option and reload"
        );
    }
    Ok(())
}

#[derive(Debug, Serialize, Deserialize)]
struct AdminRequest {
    id: u64,
    op: String,
    #[serde(default)]
    args: Value,
}

#[derive(Debug, Serialize, Deserialize)]
struct AdminResponse {
    id: u64,
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// Write a `u32` length prefix and `bytes`.
///
/// # Errors
///
/// Returns an error if the frame is too large or the write fails.
pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    bytes: &[u8],
) -> anyhow::Result<()> {
    let len = u32::try_from(bytes.len()).context("frame longer than u32")?;
    if bytes.len() > MAX_FRAME {
        bail!("frame exceeds {MAX_FRAME} bytes");
    }
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}

/// Read one length-prefixed frame.
///
/// # Errors
///
/// Returns an error on EOF, an oversized length, or a short read.
pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader.read_exact(&mut len_buf).await?;
    let len = usize::try_from(u32::from_be_bytes(len_buf))?;
    if len > MAX_FRAME {
        bail!("frame exceeds {MAX_FRAME} bytes");
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

/// Shared pieces an admin request can touch.
pub struct AdminContext {
    pub storage: DynStorage,
    pub auth: DynAuth,
    pub config: Arc<tokio::sync::RwLock<Config>>,
    pub runtime: RuntimeStore,
    pub live: Arc<LivePeers>,
    pub started: Instant,
    pub endpoint_id: String,
    /// Filled in after the endpoint ticket is known. Shared with startup.
    pub ticket: Arc<std::sync::RwLock<String>>,
}

impl AdminContext {
    /// Run one admin operation. `caller` is the remote endpoint id.
    ///
    /// # Errors
    ///
    /// Returns an error the caller should send back as `error`. Passwords are
    /// included only in `create_account`, `rotate_password`, and `approve_join`
    /// results so the admin client can store them. They are not logged.
    pub async fn call(&self, caller: &str, op: &str, args: &Value) -> anyhow::Result<Value> {
        tracing::info!(caller, op, "admin action");
        let result = match op {
            "status" => self.status().await?,
            "discovery_txt" => self.discovery_txt(args).await?,
            "list_peers" => self.list_peers().await?,
            "add_peer" => self.add_peer(args).await?,
            "remove_peer" => self.remove_peer(args).await?,
            "list_accounts" => self.list_accounts().await?,
            "create_account" => self.create_account(args).await?,
            "rotate_password" => self.rotate_password(args).await?,
            "disable_account" => self.disable_account(args).await?,
            "delete_account" => self.delete_account(args).await?,
            "list_groups" => self.list_groups().await?,
            "create_group" => self.create_group(args).await?,
            "recent_articles" => self.recent_articles(args).await?,
            "delete_article" => self.delete_article(args).await?,
            "rate_limits" => self.rate_limits().await?,
            "approve_join" => self.approve_join(args).await?,
            "list_join_requests" => self.list_join_requests().await?,
            "approve_join_request" => self.approve_join_request(args).await?,
            "reject_join_request" => self.reject_join_request(args).await?,
            other => bail!("unknown admin op {other}"),
        };
        tracing::info!(caller, op, "admin action ok");
        Ok(result)
    }

    async fn status(&self) -> anyhow::Result<Value> {
        let groups = self.group_rows().await?;
        let mut nntp: Vec<String> = self.live.nntp.iter().map(|id| id.clone()).collect();
        let mut admin: Vec<String> = self.live.admin.iter().map(|id| id.clone()).collect();
        nntp.sort();
        admin.sort();
        Ok(json!({
            "version": env!("CARGO_PKG_VERSION"),
            "endpoint_id": self.endpoint_id,
            "ticket": self.ticket.read().unwrap_or_else(|err| err.into_inner()).clone(),
            "uptime_secs": self.started.elapsed().as_secs(),
            "connected_peers": { "nntp": nntp, "admin": admin },
            "groups": groups,
            "pending_joins": self.runtime.count_pending().await?,
        }))
    }

    async fn discovery_txt(&self, args: &Value) -> anyhow::Result<Value> {
        let name = arg_str(args, "name")?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        {
            bail!("discovery name must be a DNS name");
        }
        let ticket_guard = self.ticket.read().unwrap_or_else(|err| err.into_inner());
        let ticket = ticket_guard.trim();
        if !ticket.starts_with("endpoint") {
            bail!("server has no endpoint ticket yet");
        }
        Ok(json!({
            "name": name,
            "record": format!("_iroh-news.{name}. IN TXT \"ticket={ticket}\""),
            "ticket": format!("ticket={ticket}"),
        }))
    }

    async fn list_peers(&self) -> anyhow::Result<Value> {
        let cfg = self.config.read().await;
        let mut peers = Vec::new();
        for raw in &cfg.iroh.allowed_peers {
            if let Ok(id) = canonical_endpoint(raw) {
                peers.push(json!({"endpoint_id": id, "source": "config", "label": Value::Null}));
            }
        }
        for (id, label) in self.runtime.peers().await? {
            peers.push(json!({"endpoint_id": id, "source": "runtime", "label": label}));
        }
        Ok(json!(peers))
    }

    async fn add_peer(&self, args: &Value) -> anyhow::Result<Value> {
        let endpoint_id = canonical_endpoint(arg_str(args, "endpoint_id")?)?;
        let label = arg_str(args, "label").unwrap_or("runtime");
        validate_label(label)?;
        self.runtime.upsert(&endpoint_id, label).await?;
        Ok(json!({"endpoint_id": endpoint_id, "label": label, "source": "runtime"}))
    }

    async fn remove_peer(&self, args: &Value) -> anyhow::Result<Value> {
        require_confirm(args)?;
        let endpoint_id = canonical_endpoint(arg_str(args, "endpoint_id")?)?;
        let declared = self.config.read().await.iroh.allowed_peers.clone();
        refuse_declarative_remove(&declared, &endpoint_id)?;
        let removed = self.runtime.delete_peer(&endpoint_id).await?;
        if !removed {
            bail!("runtime peer {endpoint_id} was not found");
        }
        Ok(json!({"removed": endpoint_id}))
    }

    async fn list_accounts(&self) -> anyhow::Result<Value> {
        let mut accounts = Vec::new();
        for username in self.auth.list_usernames().await? {
            let disabled = self.runtime.is_disabled(&username).await?;
            accounts.push(json!({"username": username, "disabled": disabled}));
        }
        Ok(json!(accounts))
    }

    async fn create_account(&self, args: &Value) -> anyhow::Result<Value> {
        let username = arg_str(args, "username")?;
        validate_username(username)?;
        if self
            .auth
            .list_usernames()
            .await?
            .iter()
            .any(|name| name == username)
        {
            bail!("account {username} already exists");
        }
        let password = generate_password();
        self.auth.add_user(username, &password).await?;
        self.runtime.enable(username).await?;
        Ok(json!({"username": username, "password": password}))
    }

    async fn rotate_password(&self, args: &Value) -> anyhow::Result<Value> {
        let username = arg_str(args, "username")?;
        validate_username(username)?;
        self.require_account(username).await?;
        let password = generate_password();
        self.auth.update_password(username, &password).await?;
        self.runtime.enable(username).await?;
        Ok(json!({"username": username, "password": password}))
    }

    async fn disable_account(&self, args: &Value) -> anyhow::Result<Value> {
        require_confirm(args)?;
        let username = arg_str(args, "username")?;
        validate_username(username)?;
        self.require_account(username).await?;
        let password = generate_password();
        self.auth.update_password(username, &password).await?;
        self.runtime.disable(username).await?;
        Ok(json!({"username": username, "disabled": true}))
    }

    async fn delete_account(&self, args: &Value) -> anyhow::Result<Value> {
        require_confirm(args)?;
        let username = arg_str(args, "username")?;
        validate_username(username)?;
        self.require_account(username).await?;
        self.auth.remove_user(username).await?;
        self.runtime.enable(username).await?;
        Ok(json!({"deleted": username}))
    }

    async fn list_groups(&self) -> anyhow::Result<Value> {
        Ok(json!(self.group_rows().await?))
    }

    async fn create_group(&self, args: &Value) -> anyhow::Result<Value> {
        let name = arg_str(args, "name")?;
        validate_group(name)?;
        let description = args
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("");
        if description.len() > 200 || description.chars().any(char::is_control) {
            bail!("description must be a single line of at most 200 characters");
        }
        let moderated = args
            .get("moderated")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        self.storage
            .add_group_with_description(name, moderated, description)
            .await?;
        Ok(json!({"name": name, "description": description, "moderated": moderated}))
    }

    async fn recent_articles(&self, args: &Value) -> anyhow::Result<Value> {
        let group = arg_str(args, "group")?;
        validate_group(group)?;
        if !self.storage.group_exists(group).await? {
            bail!("no such group {group}");
        }
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(10)
            .clamp(1, 20);
        let mut numbers = Vec::new();
        let mut stream = self.storage.list_article_numbers(group);
        while let Some(item) = stream.next().await {
            numbers.push(item?);
        }
        numbers.sort_unstable();
        let selected: Vec<u64> = numbers.into_iter().rev().take(limit as usize).collect();
        let mut articles = Vec::new();
        for number in selected.into_iter().rev() {
            let Some(message) = self.storage.get_article_by_number(group, number).await? else {
                continue;
            };
            let mut body = message.body.clone();
            if body.len() > 4000 {
                body.truncate(4000);
                body.push_str("\n…");
            }
            articles.push(json!({
                "number": number,
                "message_id": get_header_value(&message, "Message-ID").unwrap_or_default(),
                "from": get_header_value(&message, "From").unwrap_or_default(),
                "subject": get_header_value(&message, "Subject").unwrap_or_default(),
                "date": get_header_value(&message, "Date").unwrap_or_default(),
                "body": body,
            }));
        }
        Ok(json!({"group": group, "articles": articles}))
    }

    async fn delete_article(&self, args: &Value) -> anyhow::Result<Value> {
        require_confirm(args)?;
        let message_id = arg_str(args, "message_id")?;
        if message_id.is_empty()
            || message_id.chars().any(char::is_control)
            || message_id.len() > 250
        {
            bail!("message_id is not usable");
        }
        self.storage.delete_article_by_id(message_id).await?;
        Ok(json!({"deleted": message_id}))
    }

    async fn rate_limits(&self) -> anyhow::Result<Value> {
        let limits = self.config.read().await.user_limits.clone();
        Ok(json!({
            "post_rate_per_minute": limits.post_rate_per_minute,
            "post_burst": limits.post_burst,
        }))
    }

    async fn approve_join(&self, args: &Value) -> anyhow::Result<Value> {
        let block = args.get("block").unwrap_or(args);
        let username = block
            .get("username")
            .and_then(Value::as_str)
            .context("join block missing username")?;
        let endpoint_raw = block
            .get("endpoint_id")
            .and_then(Value::as_str)
            .context("join block missing endpoint_id")?;
        validate_username(username)?;
        let endpoint_id = canonical_endpoint(endpoint_raw)?;
        let label = block
            .get("owner")
            .and_then(Value::as_str)
            .filter(|owner| !owner.is_empty() && !owner.chars().any(char::is_control))
            .unwrap_or(username);
        validate_label(label)?;
        if self
            .auth
            .list_usernames()
            .await?
            .iter()
            .any(|name| name == username)
        {
            bail!("account {username} already exists");
        }
        self.runtime.upsert(&endpoint_id, label).await?;
        let password = generate_password();
        self.auth.add_user(username, &password).await?;
        self.runtime.bind_endpoint(&endpoint_id, username).await?;
        self.runtime.enable(username).await?;
        Ok(json!({
            "username": username,
            "endpoint_id": endpoint_id,
            "label": label,
            "password": password,
        }))
    }

    async fn list_join_requests(&self) -> anyhow::Result<Value> {
        let limits = crate::join::JoinLimits::from(&self.config.read().await.iroh.join_requests);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_secs()).unwrap_or(0))
            .unwrap_or(0);
        self.runtime
            .expire_pending(now.saturating_sub(limits.ttl_secs))
            .await?;
        let rows = self.runtime.list_joins("pending").await?;
        Ok(json!(
            rows.into_iter()
                .map(|row| json!({
                    "endpoint_id": row.endpoint_id,
                    "name": row.name,
                    "owner": row.owner,
                    "note": row.note,
                    "status": row.status,
                    "created_at": row.created_at,
                }))
                .collect::<Vec<_>>()
        ))
    }

    async fn approve_join_request(&self, args: &Value) -> anyhow::Result<Value> {
        let endpoint_id = self.resolve_join_target(args).await?;
        let rename = args.get("rename").and_then(Value::as_str);
        let row = crate::join::approve_request(
            &self.runtime,
            &self.auth,
            &endpoint_id,
            rename,
            crate::join::now_secs(),
        )
        .await?;
        Ok(json!({
            "status": row.status,
            "endpoint_id": row.endpoint_id,
            "name": row.name,
            "owner": row.owner,
        }))
    }

    async fn reject_join_request(&self, args: &Value) -> anyhow::Result<Value> {
        require_confirm(args)?;
        let endpoint_id = self.resolve_join_target(args).await?;
        let row = crate::join::reject_request(&self.runtime, &endpoint_id, crate::join::now_secs())
            .await?;
        Ok(json!({
            "status": row.status,
            "endpoint_id": row.endpoint_id,
            "name": row.name,
        }))
    }

    async fn resolve_join_target(&self, args: &Value) -> anyhow::Result<String> {
        if let Some(endpoint_id) = args.get("endpoint_id").and_then(Value::as_str) {
            return canonical_endpoint(endpoint_id);
        }
        if let Some(name) = args.get("name").and_then(Value::as_str) {
            if let Some(row) = self.runtime.find_join_by_name(name).await? {
                return Ok(row.endpoint_id);
            }
            bail!("no join request named {name}");
        }
        bail!("pass endpoint_id or name");
    }

    async fn require_account(&self, username: &str) -> anyhow::Result<()> {
        if self
            .auth
            .list_usernames()
            .await?
            .iter()
            .any(|name| name == username)
        {
            Ok(())
        } else {
            bail!("no such account {username}")
        }
    }

    async fn group_rows(&self) -> anyhow::Result<Vec<Value>> {
        let mut rows = Vec::new();
        let mut stream = self.storage.list_groups_with_descriptions();
        while let Some(item) = stream.next().await {
            let (name, description) = item?;
            let moderated = self.storage.is_group_moderated(&name).await?;
            let mut articles = 0u64;
            let mut numbers = self.storage.list_article_numbers(&name);
            while let Some(item) = numbers.next().await {
                item?;
                articles += 1;
            }
            rows.push(json!({
                "name": name,
                "description": description,
                "moderated": moderated,
                "articles": articles,
            }));
        }
        Ok(rows)
    }
}

fn arg_str<'a>(args: &'a Value, key: &str) -> anyhow::Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("missing string argument {key}"))
}

/// iroh protocol handler for the admin ALPN.
pub struct AdminProtocol {
    pub ctx: AdminContext,
}

impl std::fmt::Debug for AdminProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AdminProtocol").finish_non_exhaustive()
    }
}

impl AdminProtocol {
    async fn allowed(&self, remote: EndpointId) -> bool {
        let cfg = self.ctx.config.read().await;
        !cfg.iroh.admin_peers.is_empty() && endpoint_in_list(&cfg.iroh.admin_peers, remote)
    }
}

impl ProtocolHandler for AdminProtocol {
    async fn on_accepting(
        &self,
        accepting: iroh::endpoint::Accepting,
    ) -> Result<Connection, AcceptError> {
        let connection = accepting.await?;
        let remote = connection.remote_id();
        if !self.allowed(remote).await {
            tracing::warn!(endpoint_id = %remote, "rejected admin connection");
            connection.close(1u32.into(), b"admin endpoint not allowed");
            return Err(AcceptError::from_err(AdminRejected));
        }
        Ok(connection)
    }

    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();
        if !self.allowed(remote).await {
            tracing::warn!(endpoint_id = %remote, "rejected admin connection");
            connection.close(1u32.into(), b"admin endpoint not allowed");
            return Err(AcceptError::from_err(AdminRejected));
        }
        let caller = remote.to_string();
        self.ctx.live.admin.insert(caller.clone());
        let _guard = LiveGuard {
            set: &self.ctx.live.admin,
            id: caller.clone(),
        };
        tracing::info!(endpoint_id = %caller, "accepted admin peer");
        let (send, mut recv) = connection
            .accept_bi()
            .await
            .map_err(AcceptError::from_err)?;
        let mut send = send;
        let request_bytes = read_frame(&mut recv).await.map_err(accept_anyhow)?;
        let request: AdminRequest =
            serde_json::from_slice(&request_bytes).map_err(AcceptError::from_err)?;
        let response = match self.ctx.call(&caller, &request.op, &request.args).await {
            Ok(result) => AdminResponse {
                id: request.id,
                ok: true,
                result: Some(result),
                error: None,
            },
            Err(err) => {
                tracing::warn!(endpoint_id = %caller, op = %request.op, error = %err, "admin action failed");
                AdminResponse {
                    id: request.id,
                    ok: false,
                    result: None,
                    error: Some(err.to_string()),
                }
            }
        };
        let bytes = serde_json::to_vec(&response).map_err(AcceptError::from_err)?;
        write_frame(&mut send, &bytes)
            .await
            .map_err(accept_anyhow)?;
        // Dropping the connection immediately discards the response still in
        // the QUIC buffer. Wait until the client finishes reading, or 2s.
        let _ = tokio::time::timeout(std::time::Duration::from_secs(2), connection.closed()).await;
        Ok(())
    }
}

pub(crate) struct LiveGuard<'a> {
    pub(crate) set: &'a DashSet<String>,
    pub(crate) id: String,
}

impl Drop for LiveGuard<'_> {
    fn drop(&mut self) {
        self.set.remove(&self.id);
    }
}

fn accept_anyhow(err: anyhow::Error) -> AcceptError {
    AcceptError::from_err(FrameError(err.to_string()))
}

#[derive(Debug)]
struct FrameError(String);

impl std::fmt::Display for FrameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for FrameError {}

#[derive(Debug)]
struct AdminRejected;

impl std::fmt::Display for AdminRejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("admin endpoint not allowed")
    }
}

impl std::error::Error for AdminRejected {}

/// NNTP allowlist: declarative peers plus runtime peers.
pub async fn nntp_peer_allowed(
    config: &Config,
    runtime: &RuntimeStore,
    remote: EndpointId,
) -> anyhow::Result<bool> {
    if endpoint_in_list(&config.iroh.allowed_peers, remote) {
        return Ok(true);
    }
    runtime.contains(&remote.to_string()).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use iroh::SecretKey;

    #[tokio::test]
    async fn frames_round_trip() {
        let (client, mut server) = tokio::io::duplex(64);
        let (_reader, mut writer) = tokio::io::split(client);
        let payload = br#"{"id":1,"op":"status"}"#;
        let send = tokio::spawn(async move { write_frame(&mut writer, payload).await.unwrap() });
        let got = read_frame(&mut server).await.unwrap();
        send.await.unwrap();
        assert_eq!(got, payload);
    }

    #[test]
    fn declarative_peer_cannot_be_removed() {
        let id = SecretKey::generate().public().to_string();
        let declared = vec![id.clone()];
        let err = refuse_declarative_remove(&declared, &id).unwrap_err();
        assert!(err.to_string().contains("cannot be removed at runtime"));
        let other = SecretKey::generate().public().to_string();
        assert!(refuse_declarative_remove(&declared, &other).is_ok());
        let _ = declared;
    }

    #[tokio::test]
    async fn runtime_peer_is_allowed_and_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let uri = format!("sqlite://{}", dir.path().join("runtime.db").display());
        let store = RuntimeStore::open(&uri).await.unwrap();
        let id = SecretKey::generate().public().to_string();
        assert!(!store.contains(&id).await.unwrap());
        store.upsert(&id, "ada").await.unwrap();
        assert!(store.contains(&id).await.unwrap());
        let cfg: Config = toml::from_str(
            r#"
            addr = "127.0.0.1:119"
            site_name = "bots.local"
            db_path = "sqlite::memory:"
            auth_db_path = "sqlite::memory:"
            "#,
        )
        .unwrap();
        let parsed: EndpointId = id.parse().unwrap();
        assert!(nntp_peer_allowed(&cfg, &store, parsed).await.unwrap());
        assert!(store.delete_peer(&id).await.unwrap());
        assert!(!nntp_peer_allowed(&cfg, &store, parsed).await.unwrap());
    }

    #[test]
    fn admin_list_ignores_bot_allowlist() {
        let bot = SecretKey::generate().public().to_string();
        let admin = SecretKey::generate().public().to_string();
        let bot_id: EndpointId = bot.parse().unwrap();
        let admin_id: EndpointId = admin.parse().unwrap();
        let admins = vec![admin.clone()];
        assert!(!endpoint_in_list(&admins, bot_id));
        assert!(endpoint_in_list(&admins, admin_id));
        assert!(!endpoint_in_list(&[], admin_id));
    }
}
