//! Posting command handlers.

use super::utils::{
    check_bandwidth_rejected, comprehensive_validate_article, read_message, record_bandwidth_usage,
    write_simple,
};
use super::{CommandHandler, HandlerContext, HandlerResult};
use crate::error::{AuthError, NntpError};
use crate::limits::LimitCheckResult;
use crate::prelude::*;
use crate::queue::QueuedArticle;
use crate::responses::*;
use crate::{control, ensure_message_id, parse, parse_message};
use tracing::Span;

/// Handler for the POST command.
pub struct PostHandler;

impl CommandHandler for PostHandler {
    async fn handle(ctx: &mut HandlerContext, _args: &[String]) -> HandlerResult {
        // Check if posting is allowed on this connection
        if !ctx.session.can_post() {
            // Determine appropriate error: connection security vs authentication
            if !ctx.session.is_tls() {
                // Non-TLS connection - check if posting would be allowed with TLS
                // If anonymous posting is enabled, the issue is connection security
                // Otherwise, the issue could be either, but we prioritize the security message
                Span::current().record("outcome", "rejected_insecure");
                write_simple(&mut ctx.writer, RESP_483_SECURE_REQ).await?;
            } else {
                // TLS connection but not authenticated (and anonymous posting disabled)
                let err = NntpError::Auth(AuthError::Required);
                tracing::debug!(error = %err, "Post rejected: authentication required");
                Span::current().record("outcome", "rejected_auth");
                write_simple(&mut ctx.writer, &err.to_response()).await?;
            }
            return Ok(());
        }

        // Check per-user posting permission (only for authenticated non-admin users)
        if ctx.session.is_authenticated() && !ctx.session.is_admin() {
            if let Some(username) = ctx.session.username() {
                let username = username.to_string();
                if ctx.usage_tracker.can_post(&username).await == LimitCheckResult::PostingDisabled
                {
                    Span::current().record("outcome", "rejected_posting_disabled");
                    write_simple(&mut ctx.writer, RESP_440_POST_PROHIBITED).await?;
                    return Ok(());
                }
                if ctx.usage_tracker.try_consume_post(&username).await
                    == LimitCheckResult::PostRateExceeded
                {
                    Span::current().record("outcome", "rejected_rate_limit");
                    write_simple(&mut ctx.writer, RESP_441_RATE_LIMIT).await?;
                    return Ok(());
                }
            }
        }

        write_simple(&mut ctx.writer, RESP_340_SEND_ARTICLE).await?;

        let msg = read_message(&mut ctx.reader).await?;
        let Ok((_, mut message)) = parse_message(&msg) else {
            Span::current().record("outcome", "rejected_parse");
            write_simple(&mut ctx.writer, RESP_441_POSTING_FAILED).await?;
            return Ok(());
        };

        // Check if this is a control message first
        let is_control = control::is_control_message(&message);

        // Ensure required headers
        let cfg_guard = ctx.config.read().await;
        ensure_message_id(&mut message, &cfg_guard.site_name);
        parse::ensure_date(&mut message);
        parse::escape_message_id_header(&mut message);

        // Record article metadata in current span
        if let Some(msg_id) = message
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("Message-ID"))
            .map(|(_, v)| v.as_str())
        {
            Span::current().record("message_id", msg_id);
        }
        let size = msg.len() as u64;
        Span::current().record("size_bytes", size);
        Span::current().record("is_control", is_control);

        // Check per-user bandwidth limit (only for authenticated non-admin users)
        if check_bandwidth_rejected(&mut ctx.writer, &ctx.session, &ctx.usage_tracker, size).await?
        {
            return Ok(());
        }

        // Comprehensive validation before queuing for POST (to maintain expected behavior)
        match comprehensive_validate_article(&ctx.storage, &ctx.auth, &cfg_guard, &message, size)
            .await
        {
            Ok(()) => { /* validation passed, continue */ }
            Err(e) => {
                tracing::info!(error = %e, "Article validation failed");
                Span::current().record("outcome", "rejected_validation");
                write_simple(&mut ctx.writer, RESP_441_POSTING_FAILED).await?;
                return Ok(());
            }
        }
        drop(cfg_guard);

        // Submit to queue for background processing
        let queued_article = QueuedArticle {
            message,
            size,
            is_control,
            already_validated: true, // POST uses comprehensive validation and queues for storage only
        };

        if ctx.queue.submit(queued_article).await.is_err() {
            Span::current().record("outcome", "rejected_queue_full");
            write_simple(&mut ctx.writer, RESP_441_POSTING_FAILED).await?;
            return Ok(());
        }

        // Record bandwidth usage for authenticated non-admin users
        record_bandwidth_usage(&ctx.session, &ctx.usage_tracker, size, true).await;

        Span::current().record("outcome", "accepted");
        write_simple(&mut ctx.writer, RESP_240_ARTICLE_RECEIVED).await?;
        Ok(())
    }
}

/// Validate an article for posting (legacy function, now uses comprehensive validation).
pub async fn validate_article(
    storage: &crate::storage::DynStorage,
    auth: &crate::auth::DynAuth,
    cfg: &crate::config::Config,
    article: &crate::Message,
    size: u64,
) -> Result<()> {
    comprehensive_validate_article(storage, auth, cfg, article, size).await
}
