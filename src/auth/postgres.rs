use super::{AuthProvider, async_trait};
use crate::limits::{UserLimits, UserUsage};
use anyhow::Result;
use argon2::password_hash::{SaltString, rand_core::OsRng};
use argon2::{Argon2, PasswordHash, PasswordHasher, PasswordVerifier};
use sqlx::{
    PgPool, Row,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use std::str::FromStr;

#[derive(Clone)]
pub struct PostgresAuth {
    pool: PgPool,
}

impl PostgresAuth {
    /// Create a new Postgres authentication provider.
    pub async fn new(uri: &str) -> Result<Self> {
        let opts = PgConnectOptions::from_str(uri).map_err(|e| {
            anyhow::anyhow!(
                "Invalid PostgreSQL authentication database URI '{}': {}

Please ensure the URI is in the correct format:
- Standard connection: postgresql://user:password@host:port/database
- Local connection: postgresql:///database_name
- With SSL: postgresql://user:password@host:port/database?sslmode=require

Required connection components:
- host: PostgreSQL server hostname or IP
- port: PostgreSQL server port (default: 5432)
- database: Target database name
- user: PostgreSQL username
- password: User password (if required)",
                uri,
                e
            )
        })?;

        let pool = PgPoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to connect to PostgreSQL authentication database '{}': {}

Possible causes:
- PostgreSQL server is not running or unreachable
- Incorrect hostname, port, username, or password
- Database does not exist
- Connection refused due to pg_hba.conf configuration
- SSL/TLS connection required but not configured
- Network firewall blocking the connection
- PostgreSQL server not accepting connections

Please verify:
1. PostgreSQL server is running: systemctl status postgresql
2. Database exists: psql -l
3. User has access privileges: GRANT CONNECT ON DATABASE dbname TO username;
4. Connection settings in pg_hba.conf allow your connection method",
                    uri,
                    e
                )
            })?;

        // Run migrations using sqlx's built-in migration system
        sqlx::migrate!("src/auth/migrations/postgres")
            .run(&pool)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to run auth migrations for PostgreSQL database '{}': {}",
                    uri,
                    e
                )
            })?;

        tracing::info!("PostgreSQL authentication database ready at '{}'", uri);

        Ok(Self { pool })
    }
}

#[async_trait]
impl AuthProvider for PostgresAuth {
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
        sqlx::query(
            "INSERT INTO users (username, password_hash, key) VALUES ($1, $2, $3)\
            ON CONFLICT (username) DO UPDATE SET password_hash = EXCLUDED.password_hash, key = EXCLUDED.key",
        )
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
        sqlx::query("UPDATE users SET password_hash = $1 WHERE username = $2")
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
        sqlx::query("DELETE FROM users WHERE username = $1")
            .bind(username)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM admins WHERE username = $1")
            .bind(username)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM moderators WHERE username = $1")
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn verify_user(&self, username: &str, password: &str) -> Result<bool> {
        if let Some(row) = sqlx::query("SELECT password_hash FROM users WHERE username = $1")
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
        let row = sqlx::query("SELECT 1 FROM admins WHERE username = $1")
            .bind(username)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    async fn add_admin(&self, username: &str, key: &str) -> Result<()> {
        sqlx::query("INSERT INTO admins (username) VALUES ($1) ON CONFLICT (username) DO NOTHING")
            .bind(username)
            .execute(&self.pool)
            .await?;
        sqlx::query("UPDATE users SET key = $1 WHERE username = $2")
            .bind(key)
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn add_admin_without_key(&self, username: &str) -> Result<()> {
        sqlx::query("INSERT INTO admins (username) VALUES ($1) ON CONFLICT (username) DO NOTHING")
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn remove_admin(&self, username: &str) -> Result<()> {
        sqlx::query("DELETE FROM admins WHERE username = $1")
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn update_pgp_key(&self, username: &str, key: &str) -> Result<()> {
        sqlx::query("UPDATE users SET key = $1 WHERE username = $2")
            .bind(key)
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn get_pgp_key(&self, username: &str) -> Result<Option<String>> {
        if let Some(row) = sqlx::query("SELECT key FROM users WHERE username = $1")
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
        sqlx::query(
            "INSERT INTO moderators (username, pattern) VALUES ($1, $2)\
            ON CONFLICT (username, pattern) DO UPDATE SET pattern = EXCLUDED.pattern",
        )
        .bind(username)
        .bind(pattern)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn remove_moderator(&self, username: &str, pattern: &str) -> Result<()> {
        sqlx::query("DELETE FROM moderators WHERE username = $1 AND pattern = $2")
            .bind(username)
            .bind(pattern)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    async fn is_moderator(&self, username: &str, group: &str) -> Result<bool> {
        let rows = sqlx::query("SELECT pattern FROM moderators WHERE username = $1")
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
             FROM user_limits WHERE username = $1",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = row {
            let can_post: Option<bool> = row.try_get("can_post")?;
            let max_connections: Option<i32> = row.try_get("max_connections")?;
            let bandwidth_limit: Option<i64> = row.try_get("bandwidth_limit_bytes")?;
            let bandwidth_period: Option<i64> = row.try_get("bandwidth_period_secs")?;

            Ok(Some(UserLimits {
                can_post: can_post.unwrap_or(true),
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
             VALUES ($1, $2, $3, $4, $5, NOW())
             ON CONFLICT(username) DO UPDATE SET
                can_post = EXCLUDED.can_post,
                max_connections = EXCLUDED.max_connections,
                bandwidth_limit_bytes = EXCLUDED.bandwidth_limit_bytes,
                bandwidth_period_secs = EXCLUDED.bandwidth_period_secs,
                updated_at = NOW()"
        )
        .bind(username)
        .bind(limits.can_post)
        .bind(limits.max_connections.map(|v| v as i32))
        .bind(limits.bandwidth_limit.map(|v| v as i64))
        .bind(limits.bandwidth_period_secs.map(|v| v as i64))
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn clear_user_limits(&self, username: &str) -> Result<()> {
        sqlx::query("DELETE FROM user_limits WHERE username = $1")
            .bind(username)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    // User usage methods

    async fn get_user_usage(&self, username: &str) -> Result<UserUsage> {
        let row = sqlx::query(
            "SELECT bytes_uploaded, bytes_downloaded, 
                    to_char(window_start_at, 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"') as window_start_str
             FROM user_usage WHERE username = $1",
        )
        .bind(username)
        .fetch_optional(&self.pool)
        .await?;

        if let Some(row) = row {
            let bytes_uploaded: i64 = row.try_get("bytes_uploaded")?;
            let bytes_downloaded: i64 = row.try_get("bytes_downloaded")?;
            let window_start_str: Option<String> = row.try_get("window_start_str")?;

            let window_start = window_start_str
                .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
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
        let window_start_str = usage
            .window_start
            .map(|dt| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
            .unwrap_or_else(|| chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string());

        sqlx::query(
            "INSERT INTO user_usage (username, bytes_uploaded, bytes_downloaded, window_start_at, updated_at)
             VALUES ($1, $2, $3, $4::timestamptz, NOW())
             ON CONFLICT(username) DO UPDATE SET
                bytes_uploaded = EXCLUDED.bytes_uploaded,
                bytes_downloaded = EXCLUDED.bytes_downloaded,
                window_start_at = EXCLUDED.window_start_at,
                updated_at = NOW()"
        )
        .bind(username)
        .bind(usage.bytes_uploaded as i64)
        .bind(usage.bytes_downloaded as i64)
        .bind(&window_start_str)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn reset_user_usage(&self, username: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO user_usage (username, bytes_uploaded, bytes_downloaded, window_start_at, updated_at)
             VALUES ($1, 0, 0, NOW(), NOW())
             ON CONFLICT(username) DO UPDATE SET
                bytes_uploaded = 0,
                bytes_downloaded = 0,
                window_start_at = NOW(),
                updated_at = NOW()"
        )
        .bind(username)
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
