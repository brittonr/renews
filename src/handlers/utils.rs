//! Utility functions for command handlers.

use crate::Message;
use crate::limits::{LimitCheckResult, UsageTracker};
use crate::session::Session;
use crate::storage::DynStorage;
use anyhow::Result;
use smallvec::SmallVec;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tracing::Span;

/// Extract newsgroups from message headers.
/// Returns a collection of newsgroup names parsed from the Newsgroups header.
pub fn extract_newsgroups(article: &Message) -> SmallVec<[String; 4]> {
    crate::storage::common::parse_newsgroups_from_message(article)
}

/// Check if message has required header (case-insensitive).
pub fn has_header(article: &Message, header_name: &str) -> bool {
    article
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case(header_name))
}

/// Get header value from an article (case-insensitive).
/// Returns the first matching header value.
pub fn get_header_value(msg: &Message, name: &str) -> Option<String> {
    msg.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.clone())
}

/// Get all header values for a given header name (case-insensitive).
pub fn get_header_values(article: &Message, header_name: &str) -> SmallVec<[String; 2]> {
    article
        .headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case(header_name))
        .map(|(_, v)| v.clone())
        .collect()
}

/// Extract Message-ID from an article.
///
/// Re-exports [`crate::storage::common::extract_message_id`] for convenience.
pub use crate::storage::common::extract_message_id;

/// Errors that can occur when querying for articles.
#[derive(Debug, Clone)]
pub enum ArticleQueryError {
    /// No group is currently selected.
    NoGroup,
    /// Invalid message-id format.
    InvalidId,
    /// The specified range is empty.
    RangeEmpty,
    /// Article not found by number.
    NotFoundByNumber,
    /// Article not found by message-id.
    MessageIdNotFound,
    /// No current article is selected.
    NoCurrentArticle,
}

impl fmt::Display for ArticleQueryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ArticleQueryError::NoGroup => write!(f, "No group selected"),
            ArticleQueryError::InvalidId => write!(f, "Invalid message-id format"),
            ArticleQueryError::RangeEmpty => write!(f, "Range is empty"),
            ArticleQueryError::NotFoundByNumber => write!(f, "Article not found by number"),
            ArticleQueryError::MessageIdNotFound => write!(f, "Article not found by message-id"),
            ArticleQueryError::NoCurrentArticle => write!(f, "No current article selected"),
        }
    }
}

impl Error for ArticleQueryError {}

/// Write a simple response line to the writer.
pub async fn write_simple<W: AsyncWrite + Unpin>(writer: &mut W, response: &str) -> Result<()> {
    use tokio::io::AsyncWriteExt;
    writer.write_all(response.as_bytes()).await?;
    writer.flush().await?;
    Ok(())
}

/// Send article headers to the writer.
pub async fn send_headers<W: AsyncWrite + Unpin>(writer: &mut W, article: &Message) -> Result<()> {
    for (name, val) in &article.headers {
        writer.write_all(name.as_bytes()).await?;
        writer.write_all(b": ").await?;
        writer.write_all(val.as_bytes()).await?;
        writer.write_all(b"\r\n").await?;
    }
    Ok(())
}

/// Send article body to the writer with proper dot-stuffing.
pub async fn send_body<W: AsyncWrite + Unpin>(writer: &mut W, body: &str) -> Result<()> {
    for line in body.lines() {
        if line.starts_with('.') {
            writer.write_all(b".").await?;
        }
        writer.write_all(line.as_bytes()).await?;
        writer.write_all(b"\r\n").await?;
    }
    Ok(())
}

/// Get metadata value for an article.
pub async fn metadata_value(storage: &DynStorage, msg: &Message, name: &str) -> Option<String> {
    match name {
        ":bytes" => {
            if let Some(id) = extract_message_id(msg) {
                storage
                    .get_message_size(&id)
                    .await
                    .ok()
                    .flatten()
                    .map(|s| s.to_string())
                    .or_else(|| Some((msg.body.len() as u64).to_string()))
            } else {
                Some((msg.body.len() as u64).to_string())
            }
        }
        ":lines" => Some(msg.body.lines().count().to_string()),
        _ => None,
    }
}

/// Resolve articles based on argument (number, range, or message-id).
pub async fn resolve_articles(
    storage: &DynStorage,
    session: &mut Session,
    arg: Option<&str>,
) -> Result<Vec<(u64, Message)>, ArticleQueryError> {
    let mut articles = Vec::new();

    if let Some(arg) = arg {
        if arg.starts_with('<') && arg.ends_with('>') {
            // Message-ID
            if let Some(article) = storage
                .get_article_by_id(arg)
                .await
                .map_err(|_| ArticleQueryError::MessageIdNotFound)?
            {
                articles.push((0, article));
            } else {
                return Err(ArticleQueryError::MessageIdNotFound);
            }
        } else if let Some(group) = session.current_group().map(|s| s.to_string()) {
            // Article number or range
            let nums = crate::parse_range(storage, &group, arg)
                .await
                .map_err(|_| ArticleQueryError::RangeEmpty)?;

            if nums.is_empty() {
                return Err(ArticleQueryError::RangeEmpty);
            }

            for n in nums {
                if let Some(article) = storage
                    .get_article_by_number(&group, n)
                    .await
                    .map_err(|_| ArticleQueryError::NotFoundByNumber)?
                {
                    articles.push((n, article));
                    session.set_current_article(n);
                }
            }

            if articles.is_empty() {
                return Err(ArticleQueryError::NotFoundByNumber);
            }
        } else if arg.parse::<u64>().is_ok() {
            return Err(ArticleQueryError::NoGroup);
        } else {
            return Err(ArticleQueryError::InvalidId);
        }
    } else if let (Some(group), Some(num)) = (session.current_group(), session.current_article()) {
        // Use current article
        if let Some(article) = storage
            .get_article_by_number(group, num)
            .await
            .map_err(|_| ArticleQueryError::NoCurrentArticle)?
        {
            articles.push((num, article));
        } else {
            return Err(ArticleQueryError::NoCurrentArticle);
        }
    } else if session.current_group().is_none() {
        return Err(ArticleQueryError::NoGroup);
    } else {
        return Err(ArticleQueryError::NoCurrentArticle);
    }

    Ok(articles)
}

/// Article operation types.
#[derive(Debug, Clone, Copy)]
pub enum ArticleOperation {
    Full,    // ARTICLE command - send headers + body
    Headers, // HEAD command - send headers only
    Body,    // BODY command - send body only
    Stat,    // STAT command - send status only
}

impl ArticleOperation {
    pub fn response_code(&self) -> u16 {
        match self {
            ArticleOperation::Full => 220,
            ArticleOperation::Headers => 221,
            ArticleOperation::Body => 222,
            ArticleOperation::Stat => 223,
        }
    }

    pub fn response_suffix(&self) -> &'static str {
        match self {
            ArticleOperation::Full => "article follows",
            ArticleOperation::Headers => "article headers follow",
            ArticleOperation::Body => "article body follows",
            ArticleOperation::Stat => "article exists",
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            ArticleOperation::Full => "full",
            ArticleOperation::Headers => "headers",
            ArticleOperation::Body => "body",
            ArticleOperation::Stat => "stat",
        }
    }
}

/// Bandwidth tracking context for article operations.
#[derive(Clone)]
pub struct BandwidthContext {
    pub tracker: Arc<UsageTracker>,
    pub username: String,
}

/// Generic handler for article operations (ARTICLE, HEAD, BODY, STAT).
pub async fn handle_article_operation<W: AsyncWrite + Unpin>(
    writer: &mut W,
    storage: &DynStorage,
    session: &mut Session,
    args: &[String],
    operation: ArticleOperation,
    bandwidth_ctx: Option<BandwidthContext>,
) -> Result<()> {
    use crate::responses::*;

    // Record operation type in current span
    Span::current().record("operation", operation.as_str());

    // Record the argument (message-id or article number) if provided
    if let Some(arg) = args.first() {
        if arg.starts_with('<') && arg.ends_with('>') {
            Span::current().record("message_id", arg.as_str());
        } else {
            Span::current().record("article_number", arg.as_str());
        }
    }

    match resolve_articles(storage, session, args.first().map(String::as_str)).await {
        Ok(articles) => {
            for (num, article) in articles {
                crate::metrics::METRICS.article_fetched(session.current_group().unwrap_or(""));
                let id = extract_message_id(&article).unwrap_or_default();

                // Record resolved message_id if we didn't have it from args
                if args.first().is_none_or(|a| !a.starts_with('<')) {
                    Span::current().record("message_id", id.as_str());
                }

                // Calculate article size for bandwidth tracking
                // Only track bandwidth for Full, Headers, and Body operations (not Stat)
                let article_size = match operation {
                    ArticleOperation::Full => {
                        let header_size: usize = article
                            .headers
                            .iter()
                            .map(|(k, v)| k.len() + 2 + v.len() + 2)
                            .sum();
                        (header_size + 2 + article.body.len() + 5) as u64 // +2 for CRLF, +5 for .\r\n
                    }
                    ArticleOperation::Headers => {
                        let header_size: usize = article
                            .headers
                            .iter()
                            .map(|(k, v)| k.len() + 2 + v.len() + 2)
                            .sum();
                        (header_size + 3) as u64 // +3 for .\r\n
                    }
                    ArticleOperation::Body => (article.body.len() + 5) as u64, // +5 for .\r\n
                    ArticleOperation::Stat => 0,
                };

                // Check bandwidth limit before sending (if applicable)
                if article_size > 0
                    && let Some(ref bw_ctx) = bandwidth_ctx
                    && bw_ctx
                        .tracker
                        .check_bandwidth(&bw_ctx.username, article_size)
                        .await
                        == LimitCheckResult::BandwidthExceeded
                {
                    Span::current().record("outcome", "rejected_bandwidth");
                    write_simple(writer, RESP_403_BANDWIDTH_EXCEEDED).await?;
                    return Ok(());
                }

                // Use format! to handle arbitrarily long message-IDs
                let response_line = format!(
                    "{} {} {} {}\r\n",
                    operation.response_code(),
                    num,
                    id,
                    operation.response_suffix()
                );
                writer.write_all(response_line.as_bytes()).await?;

                match operation {
                    ArticleOperation::Full => {
                        send_headers(writer, &article).await?;
                        writer.write_all(RESP_CRLF.as_bytes()).await?;
                        send_body(writer, &article.body).await?;
                        writer.write_all(RESP_DOT_CRLF.as_bytes()).await?;
                    }
                    ArticleOperation::Headers => {
                        send_headers(writer, &article).await?;
                        writer.write_all(RESP_DOT_CRLF.as_bytes()).await?;
                    }
                    ArticleOperation::Body => {
                        send_body(writer, &article.body).await?;
                        writer.write_all(RESP_DOT_CRLF.as_bytes()).await?;
                    }
                    ArticleOperation::Stat => {
                        // STAT just sends the status line, no content
                    }
                }

                // Record bandwidth usage after successful send
                if article_size > 0
                    && let Some(ref bw_ctx) = bandwidth_ctx
                {
                    bw_ctx
                        .tracker
                        .record_bandwidth(&bw_ctx.username, article_size, false)
                        .await;
                }
            }
            Span::current().record("outcome", "success");
        }
        Err(error) => handle_article_error(writer, error).await?,
    }
    Ok(())
}

/// Check bandwidth limit for authenticated non-admin users.
///
/// Returns `true` if the request was rejected (response already sent),
/// `false` if the request can proceed.
pub async fn check_bandwidth_rejected<W: AsyncWrite + Unpin>(
    writer: &mut W,
    session: &Session,
    usage_tracker: &std::sync::Arc<UsageTracker>,
    size: u64,
) -> Result<bool> {
    use crate::responses::RESP_403_BANDWIDTH_EXCEEDED;

    if session.is_authenticated()
        && !session.is_admin()
        && let Some(username) = session.username()
        && usage_tracker.check_bandwidth(username, size).await
            == LimitCheckResult::BandwidthExceeded
    {
        Span::current().record("outcome", "rejected_bandwidth");
        write_simple(writer, RESP_403_BANDWIDTH_EXCEEDED).await?;
        return Ok(true);
    }
    Ok(false)
}

/// Record bandwidth usage for authenticated non-admin users.
pub async fn record_bandwidth_usage(
    session: &Session,
    usage_tracker: &std::sync::Arc<UsageTracker>,
    size: u64,
    is_upload: bool,
) {
    if session.is_authenticated()
        && !session.is_admin()
        && let Some(username) = session.username()
    {
        usage_tracker
            .record_bandwidth(username, size, is_upload)
            .await;
    }
}

/// Handle errors from article queries consistently.
pub async fn handle_article_error<W: AsyncWrite + Unpin>(
    writer: &mut W,
    error: ArticleQueryError,
) -> Result<()> {
    use crate::responses::*;

    let outcome = match &error {
        ArticleQueryError::NoGroup => "no_group",
        ArticleQueryError::InvalidId => "invalid_id",
        ArticleQueryError::RangeEmpty => "range_empty",
        ArticleQueryError::NotFoundByNumber => "not_found_by_number",
        ArticleQueryError::MessageIdNotFound => "not_found_by_id",
        ArticleQueryError::NoCurrentArticle => "no_current_article",
    };
    Span::current().record("outcome", outcome);

    match error {
        ArticleQueryError::NoGroup => {
            write_simple(writer, RESP_412_NO_GROUP).await?;
        }
        ArticleQueryError::InvalidId => {
            write_simple(writer, RESP_501_INVALID_ID).await?;
        }
        ArticleQueryError::RangeEmpty => {
            write_simple(writer, RESP_423_RANGE_EMPTY).await?;
        }
        ArticleQueryError::NotFoundByNumber => {
            write_simple(writer, RESP_423_NO_ARTICLE_NUM).await?;
        }
        ArticleQueryError::MessageIdNotFound => {
            write_simple(writer, RESP_430_NO_ARTICLE).await?;
        }
        ArticleQueryError::NoCurrentArticle => {
            write_simple(writer, RESP_420_NO_CURRENT).await?;
        }
    }
    Ok(())
}

/// Write a response with header values.
pub async fn write_response_with_values<W: AsyncWrite + Unpin>(
    writer: &mut W,
    response: &str,
    values: &[(u64, Option<String>)],
) -> Result<()> {
    writer.write_all(response.as_bytes()).await?;
    for (n, val) in values {
        // Use format! for variable-length values to avoid buffer overflow
        // Header values (especially References) can be arbitrarily long
        let line = if let Some(v) = val {
            format!("{n} {v}\r\n")
        } else {
            format!("{n}\r\n")
        };
        writer.write_all(line.as_bytes()).await?;
    }
    use crate::responses::RESP_DOT_CRLF;
    writer.write_all(RESP_DOT_CRLF.as_bytes()).await?;
    Ok(())
}

/// Write multiple response lines to the writer.
pub async fn write_lines<W: AsyncWrite + Unpin>(writer: &mut W, lines: &[&str]) -> Result<()> {
    for line in lines {
        writer.write_all(line.as_bytes()).await?;
    }
    Ok(())
}

/// Read a message from the reader until dot termination.
pub async fn read_message<R: AsyncBufRead + Unpin>(reader: &mut R) -> Result<String> {
    let mut msg = String::new();
    let mut line = String::new();

    loop {
        line.clear();
        reader.read_line(&mut line).await?;
        if line == ".\r\n" || line == ".\n" {
            break;
        }
        if line.starts_with("..") {
            msg.push_str(&line[1..]);
        } else {
            msg.push_str(&line);
        }
    }
    Ok(msg)
}

/// Perform basic validation on an article before queuing
///
/// This checks only what can be validated without database access:
/// - Required headers (From, Subject, Newsgroups)
/// - Size limits
///
/// NOTE: This function is maintained for backward compatibility.
/// New code should use the filter-based validation system.
pub async fn basic_validate_article(
    _cfg: &crate::config::Config,
    article: &crate::Message,
    _size: u64,
) -> Result<()> {
    // Check required headers
    let has_from = has_header(article, "From");
    let has_subject = has_header(article, "Subject");
    let newsgroups = extract_newsgroups(article);

    if !has_from || !has_subject || newsgroups.is_empty() {
        return Err(anyhow::anyhow!("missing required headers"));
    }

    Ok(())
}

/// Validate an article for posting (comprehensive validation using filter chain).
/// This performs database-dependent validation and should be used by workers.
pub async fn comprehensive_validate_article(
    storage: &crate::storage::DynStorage,
    auth: &crate::auth::DynAuth,
    cfg: &crate::config::Config,
    article: &crate::Message,
    size: u64,
) -> Result<()> {
    validate_article_with_filters(
        storage,
        auth,
        cfg,
        article,
        size,
        &crate::filters::FilterChain::default(),
    )
    .await
}

/// Validate an article using a custom filter chain.
/// This allows for customizable validation beyond the default chain.
pub async fn validate_article_with_filters(
    storage: &crate::storage::DynStorage,
    auth: &crate::auth::DynAuth,
    cfg: &crate::config::Config,
    article: &crate::Message,
    size: u64,
    filter_chain: &crate::filters::FilterChain,
) -> Result<()> {
    filter_chain
        .validate(storage, auth, cfg, article, size)
        .await
}

/// Write a formatted response line efficiently, avoiding format! allocations where possible
pub async fn write_response_with_args<W: AsyncWrite + Unpin>(
    writer: &mut W,
    prefix: &str,
    args: &[&str],
    suffix: &str,
) -> Result<()> {
    writer.write_all(prefix.as_bytes()).await?;
    for (i, arg) in args.iter().enumerate() {
        if i > 0 {
            writer.write_all(b" ").await?;
        }
        writer.write_all(arg.as_bytes()).await?;
    }
    writer.write_all(suffix.as_bytes()).await?;
    Ok(())
}
