use super::{
    ArticleStream, GroupDescriptionStream, Message, Storage, StringStream, StringTimestampStream,
    U64Stream,
    common::{Headers, extract_message_id, parse_newsgroups_from_message},
};
use anyhow::Result;
use async_stream::stream;
use async_trait::async_trait;
use futures_util::StreamExt;
use sqlx::{
    Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::str::FromStr;

#[derive(Clone)]
pub struct SqliteStorage {
    pool: SqlitePool,
}

impl SqliteStorage {
    #[tracing::instrument(skip_all)]
    /// Create a new SQLite storage backend.
    ///
    /// # Errors
    ///
    /// Returns an error if the database connection fails or schema creation fails.
    pub async fn new(path: &str) -> Result<Self> {
        let options = SqliteConnectOptions::from_str(path)
            .map_err(|e| {
                anyhow::anyhow!(
                    "Invalid SQLite connection URI '{path}': {e}

Please ensure the URI is in the correct format:
- File database: sqlite:///path/to/database.db
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
                    "Failed to connect to SQLite database '{path}': {e}

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
        sqlx::migrate!("src/storage/migrations/sqlite")
            .run(&pool)
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "Failed to run storage migrations for SQLite database '{path}': {e}"
                )
            })?;

        tracing::info!("SQLite storage database ready at '{}'", path);

        Ok(Self { pool })
    }
}

#[async_trait]
impl Storage for SqliteStorage {
    #[tracing::instrument(skip_all)]
    async fn store_article(&self, article: &Message) -> Result<Vec<(String, u64)>> {
        let msg_id =
            extract_message_id(article).ok_or_else(|| anyhow::anyhow!("missing Message-ID"))?;
        let headers = serde_json::to_string(&Headers(article.headers.clone()))?;

        // Store the message once
        sqlx::query(
            "INSERT OR IGNORE INTO messages (message_id, headers, body, size) VALUES (?, ?, ?, ?)",
        )
        .bind(&msg_id)
        .bind(&headers)
        .bind(&article.body)
        .bind(i64::try_from(article.body.len()).unwrap_or(i64::MAX))
        .execute(&self.pool)
        .await?;

        // Extract newsgroups from headers
        let newsgroups = parse_newsgroups_from_message(article);

        // Associate with each group and create overview data
        let now = chrono::Utc::now().timestamp();
        let mut placed = Vec::new();
        for group in newsgroups {
            let next: i64 = sqlx::query_scalar(
                "SELECT COALESCE(MAX(number),0)+1 FROM group_articles WHERE group_name = ?",
            )
            .bind(&group)
            .fetch_one(&self.pool)
            .await?;

            sqlx::query(
                "INSERT INTO group_articles (group_name, number, message_id, inserted_at) VALUES (?, ?, ?, ?)",
            )
            .bind(&group)
            .bind(next)
            .bind(&msg_id)
            .bind(now)
            .execute(&self.pool)
            .await?;

            // Generate and store overview data
            let overview_data = {
                use crate::overview::generate_overview_line;
                generate_overview_line(self, next as u64, article).await?
            };

            sqlx::query(
                "INSERT OR REPLACE INTO overview (group_name, article_number, overview_data) VALUES (?, ?, ?)",
            )
            .bind(&group)
            .bind(next)
            .bind(&overview_data)
            .execute(&self.pool)
            .await?;
            placed.push((group, u64::try_from(next).unwrap_or(0)));
        }

        Ok(placed)
    }

    #[tracing::instrument(skip_all)]
    async fn get_article_by_number(&self, group: &str, number: u64) -> Result<Option<Message>> {
        if let Some(row) = sqlx::query(
            "SELECT m.headers, m.body FROM messages m \
             JOIN group_articles g ON m.message_id = g.message_id \
             WHERE g.group_name = ? AND g.number = ?",
        )
        .bind(group)
        .bind(i64::try_from(number).unwrap_or(-1))
        .fetch_optional(&self.pool)
        .await?
        {
            let headers_str: String = row.try_get("headers")?;
            let body: String = row.try_get("body")?;
            Ok(Some(crate::storage::common::reconstruct_message_from_row(
                &headers_str,
                &body,
            )?))
        } else {
            Ok(None)
        }
    }

    #[tracing::instrument(skip_all)]
    async fn get_article_by_id(&self, message_id: &str) -> Result<Option<Message>> {
        if let Some(row) = sqlx::query("SELECT headers, body FROM messages WHERE message_id = ?")
            .bind(message_id)
            .fetch_optional(&self.pool)
            .await?
        {
            let headers_str: String = row.try_get("headers")?;
            let body: String = row.try_get("body")?;
            Ok(Some(crate::storage::common::reconstruct_message_from_row(
                &headers_str,
                &body,
            )?))
        } else {
            Ok(None)
        }
    }

    #[tracing::instrument(skip_all)]
    fn get_articles_by_ids<'a>(&'a self, message_ids: &'a [String]) -> ArticleStream<'a> {
        let pool = self.pool.clone();

        Box::pin(stream! {
            if message_ids.is_empty() {
                return;
            }

            // Build a parameterized query with the right number of placeholders
            let placeholders = message_ids.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
            let query = format!("SELECT message_id, headers, body FROM messages WHERE message_id IN ({placeholders})");

            let mut query_builder = sqlx::query(&query);
            for message_id in message_ids {
                query_builder = query_builder.bind(message_id);
            }

            let mut rows = query_builder.fetch(&pool);

            while let Some(row) = rows.next().await {
                match row {
                    Ok(r) => {
                        match (
                            r.try_get::<String, _>("message_id"),
                            r.try_get::<String, _>("headers"),
                            r.try_get::<String, _>("body")
                        ) {
                            (Ok(message_id), Ok(headers_str), Ok(body)) => {
                                match crate::storage::common::reconstruct_message_from_row(&headers_str, &body) {
                                    Ok(message) => yield Ok((message_id, message)),
                                    Err(e) => yield Err(e),
                                }
                            },
                            (Err(e), _, _) | (_, Err(e), _) | (_, _, Err(e)) => {
                                yield Err(anyhow::Error::from(e))
                            }
                        }
                    },
                    Err(e) => yield Err(anyhow::Error::from(e)),
                }
            }
        })
    }

    #[tracing::instrument(skip_all)]
    async fn add_group(&self, group: &str, moderated: bool) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        sqlx::query("INSERT OR IGNORE INTO groups (name, created_at, moderated) VALUES (?, ?, ?)")
            .bind(group)
            .bind(now)
            .bind(i32::from(moderated))
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    #[tracing::instrument(skip_all)]
    async fn set_group_moderated(&self, group: &str, moderated: bool) -> Result<()> {
        sqlx::query("UPDATE groups SET moderated = ? WHERE name = ?")
            .bind(i32::from(moderated))
            .bind(group)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    #[tracing::instrument(skip_all)]
    async fn remove_group(&self, group: &str) -> Result<()> {
        sqlx::query("DELETE FROM group_articles WHERE group_name = ?")
            .bind(group)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM groups WHERE name = ?")
            .bind(group)
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "DELETE FROM messages WHERE message_id NOT IN (SELECT DISTINCT message_id FROM group_articles)"
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    #[tracing::instrument(skip_all)]
    async fn remove_groups_by_pattern(&self, pattern: &str) -> Result<()> {
        // Get all group names that match the pattern
        let rows = sqlx::query("SELECT name FROM groups")
            .fetch_all(&self.pool)
            .await?;

        let mut matching_groups = Vec::new();
        for row in rows {
            let name: String = row.try_get("name")?;
            if crate::wildmat::wildmat(pattern, &name) {
                matching_groups.push(name);
            }
        }

        // Remove each matching group
        for group in matching_groups {
            self.remove_group(&group).await?;
        }

        Ok(())
    }

    #[tracing::instrument(skip_all)]
    async fn is_group_moderated(&self, group: &str) -> Result<bool> {
        let row = sqlx::query("SELECT moderated FROM groups WHERE name = ?")
            .bind(group)
            .fetch_optional(&self.pool)
            .await?;
        if let Some(r) = row {
            let m: i64 = r.try_get("moderated")?;
            Ok(m != 0)
        } else {
            Ok(false)
        }
    }

    #[tracing::instrument(skip_all)]
    async fn group_exists(&self, group: &str) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM groups WHERE name = ? LIMIT 1")
            .bind(group)
            .fetch_optional(&self.pool)
            .await?;
        Ok(row.is_some())
    }

    #[tracing::instrument(skip_all)]
    async fn add_group_with_description(
        &self,
        group: &str,
        moderated: bool,
        description: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        // Use INSERT OR REPLACE to upsert: if group exists, update moderated and description
        sqlx::query(
            "INSERT INTO groups (name, created_at, moderated, description) VALUES (?, ?, ?, ?)
             ON CONFLICT(name) DO UPDATE SET moderated = excluded.moderated, description = excluded.description",
        )
        .bind(group)
        .bind(now)
        .bind(i32::from(moderated))
        .bind(description)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    #[tracing::instrument(skip_all)]
    fn list_groups_with_descriptions(&self) -> GroupDescriptionStream<'_> {
        let pool = self.pool.clone();
        Box::pin(stream! {
            let mut rows = sqlx::query("SELECT name, description FROM groups ORDER BY name")
                .fetch(&pool);

            while let Some(row) = rows.next().await {
                match row {
                    Ok(r) => {
                        match (r.try_get::<String, _>("name"), r.try_get::<String, _>("description")) {
                            (Ok(name), Ok(desc)) => yield Ok((name, desc)),
                            (Err(e), _) => yield Err(anyhow::Error::from(e)),
                            (_, Err(e)) => yield Err(anyhow::Error::from(e)),
                        }
                    },
                    Err(e) => yield Err(anyhow::Error::from(e)),
                }
            }
        })
    }

    #[tracing::instrument(skip_all)]
    fn list_groups(&self) -> StringStream<'_> {
        let pool = self.pool.clone();
        Box::pin(stream! {
            let mut rows = sqlx::query("SELECT name FROM groups ORDER BY name")
                .fetch(&pool);

            while let Some(row) = rows.next().await {
                match row {
                    Ok(r) => match r.try_get::<String, _>("name") {
                        Ok(name) => yield Ok(name),
                        Err(e) => yield Err(anyhow::Error::from(e)),
                    },
                    Err(e) => yield Err(anyhow::Error::from(e)),
                }
            }
        })
    }

    #[tracing::instrument(skip_all)]
    fn list_groups_since(&self, since: chrono::DateTime<chrono::Utc>) -> StringStream<'_> {
        let pool = self.pool.clone();
        let timestamp = since.timestamp();
        Box::pin(stream! {
            let mut rows = sqlx::query("SELECT name FROM groups WHERE created_at > ? ORDER BY name")
                .bind(timestamp)
                .fetch(&pool);

            while let Some(row) = rows.next().await {
                match row {
                    Ok(r) => match r.try_get::<String, _>("name") {
                        Ok(name) => yield Ok(name),
                        Err(e) => yield Err(anyhow::Error::from(e)),
                    },
                    Err(e) => yield Err(anyhow::Error::from(e)),
                }
            }
        })
    }

    #[tracing::instrument(skip_all)]
    fn list_groups_with_times(&self) -> StringTimestampStream<'_> {
        let pool = self.pool.clone();
        Box::pin(stream! {
            let mut rows = sqlx::query("SELECT name, created_at FROM groups ORDER BY name")
                .fetch(&pool);

            while let Some(row) = rows.next().await {
                match row {
                    Ok(r) => {
                        match (r.try_get::<String, _>("name"), r.try_get::<i64, _>("created_at")) {
                            (Ok(name), Ok(ts)) => yield Ok((name, ts)),
                            (Err(e), _) => yield Err(anyhow::Error::from(e)),
                            (_, Err(e)) => yield Err(anyhow::Error::from(e)),
                        }
                    },
                    Err(e) => yield Err(anyhow::Error::from(e)),
                }
            }
        })
    }

    #[tracing::instrument(skip_all)]
    fn list_article_numbers(&self, group: &str) -> U64Stream<'_> {
        let pool = self.pool.clone();
        let group = group.to_string();
        Box::pin(stream! {
            let mut rows = sqlx::query("SELECT number FROM group_articles WHERE group_name = ? ORDER BY number")
                .bind(&group)
                .fetch(&pool);

            while let Some(row) = rows.next().await {
                match row {
                    Ok(r) => match r.try_get::<i64, _>("number") {
                        Ok(number) => yield Ok(u64::try_from(number).unwrap_or(0)),
                        Err(e) => yield Err(anyhow::Error::from(e)),
                    },
                    Err(e) => yield Err(anyhow::Error::from(e)),
                }
            }
        })
    }

    #[tracing::instrument(skip_all)]
    fn list_article_ids(&self, group: &str) -> StringStream<'_> {
        let pool = self.pool.clone();
        let group = group.to_string();
        Box::pin(stream! {
            let mut rows = sqlx::query("SELECT message_id FROM group_articles WHERE group_name = ? ORDER BY number")
                .bind(&group)
                .fetch(&pool);

            while let Some(row) = rows.next().await {
                match row {
                    Ok(r) => match r.try_get::<String, _>("message_id") {
                        Ok(message_id) => yield Ok(message_id),
                        Err(e) => yield Err(anyhow::Error::from(e)),
                    },
                    Err(e) => yield Err(anyhow::Error::from(e)),
                }
            }
        })
    }

    #[tracing::instrument(skip_all)]
    fn list_article_ids_since(
        &self,
        group: &str,
        since: chrono::DateTime<chrono::Utc>,
    ) -> StringStream<'_> {
        let pool = self.pool.clone();
        let group = group.to_string();
        let timestamp = since.timestamp();
        Box::pin(stream! {
            let mut rows = sqlx::query("SELECT message_id FROM group_articles WHERE group_name = ? AND inserted_at > ? ORDER BY number")
                .bind(&group)
                .bind(timestamp)
                .fetch(&pool);

            while let Some(row) = rows.next().await {
                match row {
                    Ok(r) => match r.try_get::<String, _>("message_id") {
                        Ok(message_id) => yield Ok(message_id),
                        Err(e) => yield Err(anyhow::Error::from(e)),
                    },
                    Err(e) => yield Err(anyhow::Error::from(e)),
                }
            }
        })
    }

    #[tracing::instrument(skip_all)]
    async fn purge_group_before(
        &self,
        group: &str,
        before: chrono::DateTime<chrono::Utc>,
    ) -> Result<()> {
        sqlx::query("DELETE FROM group_articles WHERE group_name = ? AND inserted_at < ?")
            .bind(group)
            .bind(before.timestamp())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    #[tracing::instrument(skip_all)]
    async fn purge_orphan_messages(&self) -> Result<()> {
        sqlx::query(
            "DELETE FROM messages WHERE message_id NOT IN (SELECT DISTINCT message_id FROM group_articles)"
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    #[tracing::instrument(skip_all)]
    async fn get_message_size(&self, message_id: &str) -> Result<Option<u64>> {
        if let Some(row) = sqlx::query("SELECT size FROM messages WHERE message_id = ?")
            .bind(message_id)
            .fetch_optional(&self.pool)
            .await?
        {
            let size: i64 = row.try_get("size")?;
            Ok(Some(u64::try_from(size).unwrap_or(0)))
        } else {
            Ok(None)
        }
    }

    async fn delete_article_by_id(&self, message_id: &str) -> Result<()> {
        sqlx::query("DELETE FROM group_articles WHERE message_id = ?")
            .bind(message_id)
            .execute(&self.pool)
            .await?;
        sqlx::query(
            "DELETE FROM messages WHERE message_id = ? AND NOT EXISTS (SELECT 1 FROM group_articles WHERE message_id = ?)",
        )
        .bind(message_id)
        .bind(message_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    #[tracing::instrument(skip_all)]
    async fn get_overview_range(&self, group: &str, start: u64, end: u64) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT overview_data FROM overview WHERE group_name = ? AND article_number >= ? AND article_number <= ? ORDER BY article_number",
        )
        .bind(group)
        .bind(i64::try_from(start).unwrap_or(0))
        .bind(i64::try_from(end).unwrap_or(i64::MAX))
        .fetch_all(&self.pool)
        .await?;

        let mut overview_lines = Vec::new();
        for row in rows {
            let overview_data: String = row.try_get("overview_data")?;
            overview_lines.push(overview_data);
        }

        Ok(overview_lines)
    }

    async fn list_group_catalog(&self) -> Result<Vec<super::GroupCatalog>> {
        let rows =
            sqlx::query("SELECT name, description, creator, created_at FROM groups ORDER BY name")
                .fetch_all(&self.pool)
                .await?;
        let mut out = Vec::with_capacity(rows.len());
        for row in rows {
            out.push(super::GroupCatalog {
                name: row.try_get("name")?,
                description: row.try_get("description")?,
                creator: row.try_get("creator")?,
                created_at: row.try_get("created_at")?,
            });
        }
        Ok(out)
    }

    async fn create_bot_group(&self, group: &str, description: &str, creator: &str) -> Result<()> {
        let now = chrono::Utc::now().timestamp();
        sqlx::query(
            "INSERT INTO groups (name, created_at, moderated, description, creator) VALUES (?, ?, 0, ?, ?)",
        )
        .bind(group)
        .bind(now)
        .bind(description)
        .bind(creator)
        .execute(&self.pool)
        .await?;
        sqlx::query(
            "INSERT INTO group_creations (username, group_name, created_at) VALUES (?, ?, ?)",
        )
        .bind(creator)
        .bind(group)
        .bind(now)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn count_groups(&self) -> Result<u64> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM groups")
            .fetch_one(&self.pool)
            .await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }

    async fn group_creates_since(&self, username: &str, since: i64) -> Result<u64> {
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM group_creations WHERE username = ? AND created_at >= ?",
        )
        .bind(username)
        .bind(since)
        .fetch_one(&self.pool)
        .await?;
        Ok(u64::try_from(n).unwrap_or(0))
    }
}
