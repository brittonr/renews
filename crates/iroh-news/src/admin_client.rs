//! Client for the renews admin channel (`/onix/nntp-admin/1`).
//!
//! One length-prefixed JSON request per connection. Generated passwords are
//! returned by the server once; [`stash_password`] moves them into a mode
//! `0600` file and strips them from the value callers print or return.

use std::path::{Path, PathBuf};

use anyhow::{Context as _, bail};
use iroh::SecretKey;
use iroh::endpoint::{RelayMode, presets};
use serde_json::{Value, json};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

use crate::discover::{self, DiscoveryMode};

/// ALPN for the admin channel. Must match `renews::admin::ADMIN_ALPN`.
pub const ADMIN_ALPN: &[u8] = b"/onix/nntp-admin/1";

/// ALPN for join requests. Must match `renews::join::JOIN_ALPN`.
pub const JOIN_ALPN: &[u8] = b"/onix/nntp-join/1";

const MAX_FRAME: usize = 1024 * 1024;

/// Call one admin operation. `args` is the operation object.
///
/// # Errors
///
/// Returns an error if the peer rejects the endpoint, the frame fails, or the
/// server reports `ok: false`.
/// Submit one join request and return the server's JSON object.
///
/// # Errors
///
/// Returns an error if the server is unreachable or replies `status: error`.
#[allow(clippy::too_many_arguments)]
pub async fn join_request(
    server: &str,
    state_dir: &Path,
    secret_key: SecretKey,
    relays: bool,
    name: &str,
    owner: &str,
    note: &str,
    discovery: DiscoveryMode,
) -> anyhow::Result<Value> {
    let response = framed_call(
        server,
        state_dir,
        secret_key,
        relays,
        JOIN_ALPN,
        &json!({"name": name, "owner": owner, "note": note}),
        discovery,
    )
    .await?;
    if response.get("status").and_then(Value::as_str) == Some("error") {
        bail!(
            "{}",
            response
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("join request failed")
        );
    }
    Ok(response)
}

async fn framed_call(
    server: &str,
    state_dir: &Path,
    secret_key: SecretKey,
    relays: bool,
    alpn: &[u8],
    body: &Value,
    discovery: DiscoveryMode,
) -> anyhow::Result<Value> {
    let mut builder = iroh::Endpoint::builder(presets::N0).secret_key(secret_key);
    if !relays {
        builder = builder
            .relay_mode(RelayMode::Disabled)
            .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
            .map_err(|err| anyhow::anyhow!("iroh client bind address: {err}"))?;
    }
    let endpoint = builder
        .bind()
        .await
        .map_err(|err| anyhow::anyhow!("bind iroh endpoint: {err}"))?;
    let addr = discover::discover(server, state_dir, discovery).await?.addr;
    let connection = endpoint
        .connect(addr, alpn)
        .await
        .map_err(|err| anyhow::anyhow!("iroh connect: {err:#}"))?;
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|err| anyhow::anyhow!("open stream: {err:#}"))?;
    let response = async {
        write_frame(&mut send, &serde_json::to_vec(body)?).await?;
        let bytes = match read_frame(&mut recv).await {
            Ok(bytes) => bytes,
            Err(err) => {
                let closed = connection.closed().await;
                bail!("connection closed: {closed} ({err:#})");
            }
        };
        serde_json::from_slice(&bytes).context("response is not JSON")
    }
    .await;
    endpoint.close().await;
    response
}

pub async fn admin_call(
    server: &str,
    state_dir: &Path,
    secret_key: SecretKey,
    relays: bool,
    op: &str,
    args: Value,
    discovery: DiscoveryMode,
) -> anyhow::Result<Value> {
    let mut builder = iroh::Endpoint::builder(presets::N0).secret_key(secret_key);
    if !relays {
        builder = builder
            .relay_mode(RelayMode::Disabled)
            .bind_addr((std::net::Ipv4Addr::LOCALHOST, 0))
            .map_err(|err| anyhow::anyhow!("iroh client bind address: {err}"))?;
    }
    let endpoint = builder
        .bind()
        .await
        .map_err(|err| anyhow::anyhow!("bind iroh endpoint: {err}"))?;
    let addr = discover::discover(server, state_dir, discovery).await?.addr;
    let connection = endpoint
        .connect(addr, ADMIN_ALPN)
        .await
        .map_err(|err| anyhow::anyhow!("iroh admin connect: {err:#}"))?;
    let (mut send, mut recv) = connection
        .open_bi()
        .await
        .map_err(|err| anyhow::anyhow!("open admin stream: {err:#}"))?;
    let request = json!({"id": 1, "op": op, "args": args});
    let response = async {
        write_frame(&mut send, &serde_json::to_vec(&request)?).await?;
        let bytes = match read_frame(&mut recv).await {
            Ok(bytes) => bytes,
            Err(err) => {
                let closed = connection.closed().await;
                bail!("admin connection closed: {closed} ({err:#})");
            }
        };
        let response: Value =
            serde_json::from_slice(&bytes).context("admin response is not JSON")?;
        Ok(response)
    }
    .await;
    endpoint.close().await;
    let response = response?;
    if response.get("ok").and_then(Value::as_bool) != Some(true) {
        let message = response
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("admin request failed");
        bail!("{message}");
    }
    Ok(response.get("result").cloned().unwrap_or(Value::Null))
}

/// If `result` contains a `password` string, write it under `secrets_dir` and
/// replace it with `password_file`. The password is not left in `result`.
///
/// # Errors
///
/// Returns an error if the secrets directory or file cannot be created.
pub fn stash_password(result: &mut Value, secrets_dir: &Path) -> anyhow::Result<Option<PathBuf>> {
    let Some(password) = result
        .get("password")
        .and_then(Value::as_str)
        .map(str::to_string)
    else {
        return Ok(None);
    };
    let username = result
        .get("username")
        .and_then(Value::as_str)
        .unwrap_or("account");
    if username.is_empty()
        || !username
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        bail!("refusing to write a password file for username {username:?}");
    }
    std::fs::create_dir_all(secrets_dir)
        .with_context(|| format!("creating {}", secrets_dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(secrets_dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let path = secrets_dir.join(format!("{username}.password"));
    write_private(&path, format!("{password}\n").as_bytes())?;
    if let Some(object) = result.as_object_mut() {
        object.remove("password");
        object.insert(
            "password_file".to_string(),
            Value::String(path.display().to_string()),
        );
    }
    Ok(Some(path))
}

fn write_private(path: &Path, bytes: &[u8]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

async fn write_frame<W: AsyncWrite + Unpin>(writer: &mut W, bytes: &[u8]) -> anyhow::Result<()> {
    if bytes.len() > MAX_FRAME {
        bail!("frame exceeds {MAX_FRAME} bytes");
    }
    let len = u32::try_from(bytes.len())?;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(bytes).await?;
    writer.flush().await?;
    Ok(())
}

async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<Vec<u8>> {
    let mut len_buf = [0u8; 4];
    reader
        .read_exact(&mut len_buf)
        .await
        .map_err(|err| anyhow::anyhow!("reading admin frame: {err:#}"))?;
    let len = usize::try_from(u32::from_be_bytes(len_buf))?;
    if len > MAX_FRAME {
        bail!("frame exceeds {MAX_FRAME} bytes");
    }
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stash_removes_password_and_writes_mode_600() {
        let dir = tempfile::tempdir().unwrap();
        let mut result = json!({"username": "ada-grok", "password": "s3cret-value"});
        let path = stash_password(&mut result, dir.path()).unwrap().unwrap();
        assert!(result.get("password").is_none());
        assert_eq!(result["password_file"], path.display().to_string());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "s3cret-value\n");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(!result.to_string().contains("s3cret-value"));
    }
}
