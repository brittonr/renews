use super::{AuthProvider, async_trait};
use crate::limits::{UserLimits, UserUsage};
use anyhow::Result;
use argon2::password_hash::{SaltString, rand_core::OsRng};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::str::FromStr;

#[derive(Clone)]
pub struct SqliteAuth {
    pool: SqlitePool,
}

impl SqliteAuth {
    /// Create a new `SQLite` authentication provider.
    ///
    /// # Errors
    ///
    /// Returns an error if the database connection fails or schema creation fails.
    pub async fn new(path: &str) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(path)
            .map_err(|e| {
                anyhow::anyhow!(
                    "Invalid SQLite authentication database URI '{path}': {e}

Please ensure the URI is in the correct format:
- File database: sqlite:///path/to/auth.db
- In-memory database: sqlite::memory:
- Relative path: sqlite://relative/path.db"
                )
            })?
            .create_if_missing(true);

        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(options)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to connect to SQLite authentication database '{path}': {e}

Possible causes:
- Parent directory does not exist and cannot be created
- Permission denied accessing the database file or directory
- Database file is corrupted or not a valid SQLite database
- Path contains invalid characters for the filesystem
- Disk space is full
- Database is locked by another process"
                )
            })?;

        // Run migrations using sqlx's built-in migration system
        sqlx::migrate!("src/auth/migrations/sqlite")
            .run(&pool)
            .await
            .map_err(|e| {
                anyhow::anyhow!("Failed to run auth migrations for SQLite database '{path}': {e}")
            })?;

        tracing::info!("SQLite authentication database ready at '{}'", path);

        Ok(Self { pool })
    }
}

#[async_trait]
impl AuthProvider for SqliteAuth {
    async fn add_user(&self, username: &str, password: &str) -> Result<()> {
        self.add_user_with_key(username, password, None).await
    }

    async fn add_user_with_key(
        &self,
        username: &str,
        password: &str,
        key: Option<&str>,
    ) -> Result<()> {
        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default()
            .hash_password(password.as_bytes(), &salt)?
            .to_string();
        sqlx::query("INSERT OR REPLACE INTO users (username, password_hash, key) VALUES (?, ?, ?)")
            .bind(username)
            .bind(hash)
            .bind(key)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn update_password(&self, username: &str, new_password: &str) -> Result<()> {
        let salt = SaltString::generate(&mut OsRng);
        let hash = Argon2::default()
            .hash_password(new_password.as_bytes(), &salt)?
            .to_string();
        sqlx::query("UPDATE users SET password_hash = ? WHERE username = ?")
            .bind(hash)
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn list_usernames(&self) -> Result<Vec<String>> {
        let rows = sqlx::query("SELECT username FROM users ORDER BY username")
            .fetch_all(&self.pool)
            .await?;
        let mut names = Vec::with_capacity(rows.len());
        for row in rows {
            names.push(row.try_get("username")?);
        }
        Ok(names)
    }

    async fn remove_user(&self, username: &str) -> Result<()> {
        sqlx::query("DELETE FROM users WHERE username = ?")
            .bind(username)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM admins WHERE username = ?")
            .bind(username)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM moderators WHERE username = ?")
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn verify_user(&self, username: &str, password: &str) -> Result<bool> {
        if let Some(row) = sqlx::query("SELECT password_hash FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?
        {
            let stored: String = row.get(0);
            let parsed = PasswordHash::new(&stored)?;
            Ok(Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok())
        } else {
            Ok(false)
        }
    }

    async fn is_admin(&self, username: &str) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM admins WHERE username = ?")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    async fn add_admin(&self, username: &str, key: &str) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO admins (username) VALUES (?)")
            .bind(username)
            .execute(&self.pool)
            .await?;
        sqlx::query("UPDATE users SET key = ? WHERE username = ?")
            .bind(key)
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn add_admin_without_key(&self, username: &str) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO admins (username) VALUES (?)")
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn remove_admin(&self, username: &str) -> Result<()> {
        sqlx::query("DELETE FROM admins WHERE username = ?")
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn update_pgp_key(&self, username: &str, key: &str) -> Result<()> {
        sqlx::query("UPDATE users SET key = ? WHERE username = ?")
            .bind(key)
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get_pgp_key(&self, username: &str) -> Result<Option<String>> {
        if let Some(row) = sqlx::query("SELECT key FROM users WHERE username = ?")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?
        {
            let k: Option<String> = row.try_get("key")?;
            Ok(k)
        } else {
            Ok(None)
        }
    }

    async fn add_moderator(&self, username: &str, pattern: &str) -> Result<()> {
        sqlx::query("INSERT OR REPLACE INTO moderators (username, pattern) VALUES (?, ?)")
            .bind(username)
            .bind(pattern)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn remove_moderator(&self, username: &str, pattern: &str) -> Result<()> {
        sqlx::query("DELETE FROM moderators WHERE username = ? AND pattern = ?")
            .bind(username)
            .bind(pattern)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn is_moderator(&self, username: &str, group: &str) -> Result<bool> {
        let rows = sqlx::query("SELECT pattern FROM moderators WHERE username = ?")
            .bind(username)
            .fetch_all(&self.pool)
            .await?;
        for row in rows {
            let pat: String = row.try_get("pattern")?;
            if crate::wildmat::wildmat(&pat, group) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    // User limits methods

    async fn get_user_limits(&self, username: &str) -> Result<Option<UserLimits>> {
        let row = sqlx::query(
            "SELECT can_post, max_connections, bandwidth_limit_bytes, bandwidth_period_secs 
             FROM user_limits WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = row {
            let can_post: Option<i64> = row.try_get("can_post")?;
            let max_connections: Option<i64> = row.try_get("max_connections")?;
            let bandwidth_limit: Option<i64> = row.try_get("bandwidth_limit_bytes")?;
            let bandwidth_period: Option<i64> = row.try_get("bandwidth_period_secs")?;

            Ok(Some(UserLimits {
                can_post: can_post.map(|v| v != 0).unwrap_or(true),
                max_connections: max_connections.map(|v| v as u32),
                bandwidth_limit: bandwidth_limit.map(|v| v as u64),
                bandwidth_period_secs: bandwidth_period.map(|v| v as u64),
            }))
        } else {
            Ok(None)
        }
    }

    async fn set_user_limits(&self, username: &str, limits: &UserLimits) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_limits (username, can_post, max_connections, bandwidth_limit_bytes, bandwidth_period_secs, updated_at)
             VALUES (?, ?, ?, ?, ?, datetime('now'))
             ON CONFLICT(username) DO UPDATE SET
                can_post = excluded.can_post,
                max_connections = excluded.max_connections,
                bandwidth_limit_bytes = excluded.bandwidth_limit_bytes,
                bandwidth_period_secs = excluded.bandwidth_period_secs,
                updated_at = datetime('now')"
        )
        .bind(username)
        .bind(if limits.can_post { 1i64 } else { 0i64 })
        .bind(limits.max_connections.map(|v| v as i64))
        .bind(limits.bandwidth_limit.map(|v| v as i64))
        .bind(limits.bandwidth_period_secs.map(|v| v as i64))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn clear_user_limits(&self, username: &str) -> Result<()> {
        sqlx::query("DELETE FROM user_limits WHERE username = ?")
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // User usage methods

    async fn get_user_usage(&self, username: &str) -> Result<UserUsage> {
        let row = sqlx::query(
            "SELECT bytes_uploaded, bytes_downloaded, window_start_at 
             FROM user_usage WHERE username = ?",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = row {
            let bytes_uploaded: i64 = row.try_get("bytes_uploaded")?;
            let bytes_downloaded: i64 = row.try_get("bytes_downloaded")?;
            let window_start_str: String = row.try_get("window_start_at")?;

            // Parse the datetime string
            let window_start = chrono::DateTime::parse_from_rfc3339(&window_start_str)
                .or_else(|_| {
                    // Try SQLite datetime format
                    chrono::NaiveDateTime::parse_from_str(&window_start_str, "%Y-%m-%d %H:%M:%S")
                        .map(|dt| dt.and_utc().fixed_offset())
                })
                .ok()
                .map(|dt| dt.with_timezone(&chrono::Utc));

            Ok(UserUsage {
                bytes_uploaded: bytes_uploaded as u64,
                bytes_downloaded: bytes_downloaded as u64,
                window_start,
            })
        } else {
            Ok(UserUsage::default())
        }
    }

    async fn set_user_usage(&self, username: &str, usage: &UserUsage) -> Result<()> {
        let window_start = usage
            .window_start
            .map(|dt| dt.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%d %H:%M:%S").to_string());

        sqlx::query(
            "INSERT INTO user_usage (username, bytes_uploaded, bytes_downloaded, window_start_at, updated_at)
             VALUES (?, ?, ?, ?, datetime('now'))
             ON CONFLICT(username) DO UPDATE SET
                bytes_uploaded = excluded.bytes_uploaded,
                bytes_downloaded = excluded.bytes_downloaded,
                window_start_at = excluded.window_start_at,
                updated_at = datetime('now')"
        )
        .bind(username)
        .bind(usage.bytes_uploaded as i64)
        .bind(usage.bytes_downloaded as i64)
        .bind(&window_start)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn reset_user_usage(&self, username: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_usage (username, bytes_uploaded, bytes_downloaded, window_start_at, updated_at)
             VALUES (?, 0, 0, datetime('now'), datetime('now'))
             ON CONFLICT(username) DO UPDATE SET
                bytes_uploaded = 0,
                bytes_downloaded = 0,
                window_start_at = datetime('now'),
                updated_at = datetime('now')"
        )
        .bind(username)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
