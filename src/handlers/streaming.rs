//! Streaming command handlers (IHAVE, CHECK, TAKETHIS).

use super::utils::{
    check_bandwidth_rejected, comprehensive_validate_article, read_message, record_bandwidth_usage,
    write_simple,
};
use super::{CommandHandler, HandlerContext, HandlerResult};
use crate::responses::*;
use crate::{control, ensure_message_id, parse, parse_message};
use tracing::Span;

/// Handler for the IHAVE command.
pub struct IHaveHandler;

impl CommandHandler for IHaveHandler {
    async fn handle(ctx: &mut HandlerContext, args: &[String]) -> HandlerResult {
        if let Some(id) = args.first() {
            Span::current().record("message_id", id.as_str());

            if ctx.storage.get_article_by_id(id).await?.is_some() {
                Span::current().record("outcome", "already_have");
                write_simple(&mut ctx.writer, RESP_435_NOT_WANTED).await?;
                return Ok(());
            }

            write_simple(&mut ctx.writer, RESP_335_SEND_IT).await?;
            let msg = read_message(&mut ctx.reader).await?;
            let Ok((_, mut article)) = parse_message(&msg) else {
                Span::current().record("outcome", "rejected_parse");
                write_simple(&mut ctx.writer, RESP_437_REJECTED).await?;
                return Ok(());
            };

            // Check if this is a control message first
            let is_control = control::is_control_message(&article);
            Span::current().record("is_control", is_control);

            let cfg_guard = ctx.config.read().await;
            ensure_message_id(&mut article, &cfg_guard.site_name);
            parse::ensure_date(&mut article);
            parse::escape_message_id_header(&mut article);

            // Handle control messages immediately without comprehensive validation
            if is_control {
                if control::handle_control(&article, &ctx.storage, &ctx.auth, &cfg_guard).await? {
                    Span::current().record("outcome", "accepted_control");
                    write_simple(&mut ctx.writer, RESP_235_TRANSFER_OK).await?;
                    return Ok(());
                } else {
                    Span::current().record("outcome", "rejected_control");
                    write_simple(&mut ctx.writer, RESP_437_REJECTED).await?;
                    return Ok(());
                }
            }

            // Comprehensive validation before queuing for IHAVE (non-control messages)
            let size = msg.len() as u64;
            Span::current().record("size_bytes", size);

            // Check per-user bandwidth limit (only for authenticated non-admin users)
            if check_bandwidth_rejected(&mut ctx.writer, &ctx.session, &ctx.usage_tracker, size)
                .await?
            {
                return Ok(());
            }

            if comprehensive_validate_article(&ctx.storage, &ctx.auth, &cfg_guard, &article, size)
                .await
                .is_err()
            {
                Span::current().record("outcome", "rejected_validation");
                write_simple(&mut ctx.writer, RESP_437_REJECTED).await?;
                return Ok(());
            }
            drop(cfg_guard);

            // Submit to queue for background storage and immediate storage for protocol compliance
            let queued_article = crate::queue::QueuedArticle {
                message: article.clone(),
                size,
                is_control: false, // Control messages are handled above, so this is always false
                already_validated: true, // IHAVE does comprehensive validation before queuing
            };

            // Store immediately for protocol compliance (second IHAVE should know article exists)
            let placed = match ctx.storage.store_article(&article).await {
                Ok(placed) => placed,
                Err(_) => {
                    Span::current().record("outcome", "rejected_storage");
                    write_simple(&mut ctx.writer, RESP_437_REJECTED).await?;
                    return Ok(());
                }
            };
            ctx.queue.announce(&article, &placed);

            // Also queue for background processing consistency
            let _ = ctx.queue.submit(queued_article).await; // Don't fail if queue is full since we already stored

            // Record bandwidth usage for authenticated non-admin users
            record_bandwidth_usage(&ctx.session, &ctx.usage_tracker, size, true).await;

            Span::current().record("outcome", "accepted");
            write_simple(&mut ctx.writer, RESP_235_TRANSFER_OK).await?;
        } else {
            write_simple(&mut ctx.writer, RESP_501_MSGID_REQUIRED).await?;
        }
        Ok(())
    }
}

/// Handler for the CHECK command.
pub struct CheckHandler;

impl CommandHandler for CheckHandler {
    async fn handle(ctx: &mut HandlerContext, args: &[String]) -> HandlerResult {
        if let Some(id) = args.first() {
            Span::current().record("message_id", id.as_str());

            if ctx.storage.get_article_by_id(id).await?.is_some() {
                Span::current().record("outcome", "already_have");
                write_simple(&mut ctx.writer, &streaming_response(438, id)).await?;
            } else {
                Span::current().record("outcome", "send_it");
                write_simple(&mut ctx.writer, &streaming_response(238, id)).await?;
            }
        } else {
            write_simple(&mut ctx.writer, RESP_501_MSGID_REQUIRED).await?;
        }
        Ok(())
    }
}

/// Handler for the TAKETHIS command.
pub struct TakeThisHandler;

impl CommandHandler for TakeThisHandler {
    async fn handle(ctx: &mut HandlerContext, args: &[String]) -> HandlerResult {
        if let Some(id) = args.first() {
            Span::current().record("message_id", id.as_str());

            let msg = read_message(&mut ctx.reader).await?;
            let Ok((_, mut article)) = parse_message(&msg) else {
                Span::current().record("outcome", "rejected_parse");
                write_simple(&mut ctx.writer, &streaming_response(439, id)).await?;
                return Ok(());
            };

            if ctx.storage.get_article_by_id(id).await?.is_some() {
                Span::current().record("outcome", "already_have");
                write_simple(&mut ctx.writer, &streaming_response(439, id)).await?;
                return Ok(());
            }

            // Check if this is a control message first
            let is_control = control::is_control_message(&article);
            Span::current().record("is_control", is_control);

            let cfg_guard = ctx.config.read().await;
            ensure_message_id(&mut article, &cfg_guard.site_name);
            parse::ensure_date(&mut article);
            parse::escape_message_id_header(&mut article);

            // Handle control messages immediately without comprehensive validation
            if is_control {
                if control::handle_control(&article, &ctx.storage, &ctx.auth, &cfg_guard).await? {
                    Span::current().record("outcome", "accepted_control");
                    write_simple(&mut ctx.writer, &streaming_response(239, id)).await?;
                    return Ok(());
                } else {
                    Span::current().record("outcome", "rejected_control");
                    write_simple(&mut ctx.writer, &streaming_response(439, id)).await?;
                    return Ok(());
                }
            }

            // Comprehensive validation before queuing for TAKETHIS (non-control messages)
            let size = msg.len() as u64;
            Span::current().record("size_bytes", size);

            // Check per-user bandwidth limit (only for authenticated non-admin users)
            if check_bandwidth_rejected(&mut ctx.writer, &ctx.session, &ctx.usage_tracker, size)
                .await?
            {
                return Ok(());
            }

            if comprehensive_validate_article(&ctx.storage, &ctx.auth, &cfg_guard, &article, size)
                .await
                .is_err()
            {
                Span::current().record("outcome", "rejected_validation");
                write_simple(&mut ctx.writer, &streaming_response(439, id)).await?;
                return Ok(());
            }
            drop(cfg_guard);

            // Submit to queue for background storage and immediate storage for protocol compliance
            let queued_article = crate::queue::QueuedArticle {
                message: article.clone(),
                size,
                is_control: false, // Control messages are handled above, so this is always false
                already_validated: true, // TAKETHIS does comprehensive validation before queuing
            };

            // Store immediately for protocol compliance (duplicate TAKETHIS should be detected)
            let placed = match ctx.storage.store_article(&article).await {
                Ok(placed) => placed,
                Err(_) => {
                    Span::current().record("outcome", "rejected_storage");
                    write_simple(&mut ctx.writer, &streaming_response(439, id)).await?;
                    return Ok(());
                }
            };
            ctx.queue.announce(&article, &placed);

            // Also queue for background processing consistency
            let _ = ctx.queue.submit(queued_article).await; // Don't fail if queue is full since we already stored

            // Record bandwidth usage for authenticated non-admin users
            record_bandwidth_usage(&ctx.session, &ctx.usage_tracker, size, true).await;

            Span::current().record("outcome", "accepted");
            write_simple(&mut ctx.writer, &streaming_response(239, id)).await?;
        } else {
            write_simple(&mut ctx.writer, RESP_501_MSGID_REQUIRED).await?;
        }
        Ok(())
    }
}
