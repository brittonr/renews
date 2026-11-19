//! Article retrieval command handlers.

use super::utils::{
    ArticleOperation, BandwidthContext, get_header_value, handle_article_operation, metadata_value,
    resolve_articles, write_response_with_values, write_simple,
};
use super::{CommandHandler, HandlerContext, HandlerResult};
use crate::responses::*;
use tokio::io::AsyncWriteExt;

/// Macro to create simple article command handlers.
macro_rules! article_handler {
    ($name:ident, $operation:expr) => {
        pub struct $name;

        impl CommandHandler for $name {
            async fn handle(ctx: &mut HandlerContext, args: &[String]) -> HandlerResult {
                // Create bandwidth context for authenticated non-admin users
                let bandwidth_ctx = if ctx.session.is_authenticated() && !ctx.session.is_admin() {
                    ctx.session.username().map(|username| BandwidthContext {
                        tracker: ctx.usage_tracker.clone(),
                        username: username.to_string(),
                    })
                } else {
                    None
                };

                handle_article_operation(
                    &mut ctx.writer,
                    &ctx.storage,
                    &mut ctx.session,
                    args,
                    $operation,
                    bandwidth_ctx,
                )
                .await
            }
        }
    };
}

// Generate handlers for basic article operations
article_handler!(ArticleHandler, ArticleOperation::Full);
article_handler!(HeadHandler, ArticleOperation::Headers);
article_handler!(BodyHandler, ArticleOperation::Body);
article_handler!(StatHandler, ArticleOperation::Stat);

/// Handler for the HDR command.
pub struct HdrHandler;

impl CommandHandler for HdrHandler {
    async fn handle(ctx: &mut HandlerContext, args: &[String]) -> HandlerResult {
        if args.is_empty() {
            return write_simple(&mut ctx.writer, RESP_501_NOT_ENOUGH).await;
        }

        let field = &args[0];

        // Handle special case for all headers
        if field == ":" {
            return handle_all_headers(ctx, args).await;
        }

        // Collect header values for the specified field
        match collect_header_values(
            &ctx.storage,
            &ctx.session,
            field,
            args.get(1).map(|s| s.as_str()),
        )
        .await
        {
            Ok(values) => {
                // Send response
                write_response_with_values(&mut ctx.writer, RESP_225_HEADERS, &values).await
            }
            Err(error) => {
                use super::utils::handle_article_error;
                handle_article_error(&mut ctx.writer, error).await
            }
        }
    }
}

/// Handler for the XPAT command.
pub struct XPatHandler;

impl CommandHandler for XPatHandler {
    async fn handle(ctx: &mut HandlerContext, args: &[String]) -> HandlerResult {
        if args.len() < 3 {
            return write_simple(&mut ctx.writer, RESP_501_NOT_ENOUGH).await;
        }

        let field = &args[0];
        let range_or_msgid = &args[1];
        let patterns: Vec<&str> = args[2..].iter().map(String::as_str).collect();

        let values =
            match collect_header_values(&ctx.storage, &ctx.session, field, Some(range_or_msgid))
                .await
            {
                Ok(values) => values,
                Err(error) => {
                    use super::utils::handle_article_error;
                    handle_article_error(&mut ctx.writer, error).await?;
                    return Ok(());
                }
            };

        write_simple(&mut ctx.writer, RESP_221_HEADER_FOLLOWS).await?;

        for (n, val) in values {
            if let Some(v) = val
                && patterns.iter().any(|pat| crate::wildmat::wildmat(pat, &v))
            {
                ctx.writer
                    .write_all(format!("{n} {v}\r\n").as_bytes())
                    .await?;
            }
        }

        ctx.writer.write_all(RESP_DOT_CRLF.as_bytes()).await?;
        Ok(())
    }
}

/// Handler for the OVER command.
pub struct OverHandler;

impl CommandHandler for OverHandler {
    async fn handle(ctx: &mut HandlerContext, args: &[String]) -> HandlerResult {
        match resolve_articles(
            &ctx.storage,
            &mut ctx.session,
            args.first().map(String::as_str),
        )
        .await
        {
            Ok(articles) => {
                ctx.writer.write_all(RESP_224_OVERVIEW.as_bytes()).await?;
                for (num, article) in articles {
                    let overview_line = crate::overview::generate_overview_line(
                        ctx.storage.as_ref(),
                        num,
                        &article,
                    )
                    .await?;
                    ctx.writer
                        .write_all(format!("{overview_line}\r\n").as_bytes())
                        .await?;
                }
                ctx.writer.write_all(RESP_DOT_CRLF.as_bytes()).await?;
            }
            Err(error) => {
                use super::utils::handle_article_error;
                handle_article_error(&mut ctx.writer, error).await?;
            }
        }
        Ok(())
    }
}

/// Handle the special case of HDR with ":" for all headers.
async fn handle_all_headers(ctx: &mut HandlerContext, args: &[String]) -> HandlerResult {
    // Use the existing resolve_articles function to handle the complex logic
    let articles = match resolve_articles(
        &ctx.storage,
        &mut ctx.session,
        args.get(1).map(String::as_str),
    )
    .await
    {
        Ok(articles) => articles,
        Err(error) => {
            use super::utils::handle_article_error;
            handle_article_error(&mut ctx.writer, error).await?;
            return Ok(());
        }
    };

    ctx.writer.write_all(RESP_225_HEADERS.as_bytes()).await?;
    for (n, article) in articles {
        for (name, val) in &article.headers {
            let sanitized_val = sanitize_header_value(val);
            ctx.writer
                .write_all(format!("{n} {name}: {sanitized_val}\r\n").as_bytes())
                .await?;
        }
    }
    ctx.writer.write_all(RESP_DOT_CRLF.as_bytes()).await?;
    Ok(())
}

/// Sanitize header values by removing tabs and line breaks.
fn sanitize_header_value(val: &str) -> String {
    let mut v = val.replace('\t', " ");
    v.retain(|c| c != '\r' && c != '\n');
    v
}

/// Extract header value for a field (handles both standard headers and metadata).
async fn get_field_value(
    storage: &crate::storage::DynStorage,
    article: &crate::Message,
    field: &str,
) -> Option<String> {
    if field.starts_with(':') {
        metadata_value(storage, article, field).await
    } else {
        get_header_value(article, field)
    }
}

/// Collect header values for HDR/XPAT commands.
async fn collect_header_values(
    storage: &crate::storage::DynStorage,
    session: &crate::session::Session,
    field: &str,
    range_or_msgid: Option<&str>,
) -> std::result::Result<Vec<(u64, Option<String>)>, super::utils::ArticleQueryError> {
    use super::utils::ArticleQueryError;

    let mut values = Vec::new();

    if let Some(arg) = range_or_msgid {
        if arg.starts_with('<') && arg.ends_with('>') {
            // Message-ID lookup
            if let Some(article) = storage
                .get_article_by_id(arg)
                .await
                .map_err(|_| ArticleQueryError::MessageIdNotFound)?
            {
                if !super::utils::may_read_article(session, &article) {
                    return Err(ArticleQueryError::MessageIdNotFound);
                }
                let val = get_field_value(storage, &article, field).await;
                values.push((0, val));
            } else {
                return Err(ArticleQueryError::MessageIdNotFound);
            }
        } else if let Some(group) = session.current_group() {
            // Range lookup - check if it's an article number first
            let nums = crate::parse_range(storage, group, arg)
                .await
                .map_err(|_| ArticleQueryError::RangeEmpty)?;

            if nums.is_empty() {
                return Err(ArticleQueryError::RangeEmpty);
            }

            for n in nums {
                if let Some(article) = storage
                    .get_article_by_number(group, n)
                    .await
                    .map_err(|_| ArticleQueryError::NotFoundByNumber)?
                {
                    let val = get_field_value(storage, &article, field).await;
                    values.push((n, val));
                }
            }

            if values.is_empty() {
                return Err(ArticleQueryError::NotFoundByNumber);
            }
        } else {
            // No group selected - check if this looks like an article number or range
            // Article numbers and ranges are numeric (possibly with '-' for ranges)
            let looks_like_number_or_range = arg.chars().all(|c| c.is_ascii_digit() || c == '-');

            if looks_like_number_or_range {
                // Article number/range provided but no group selected
                return Err(ArticleQueryError::NoGroup);
            } else {
                // Invalid argument format
                return Err(ArticleQueryError::InvalidId);
            }
        }
    } else if let (Some(group), Some(num)) = (session.current_group(), session.current_article()) {
        // Current article lookup
        if let Some(article) = storage
            .get_article_by_number(group, num)
            .await
            .map_err(|_| ArticleQueryError::NoCurrentArticle)?
        {
            let val = get_field_value(storage, &article, field).await;
            values.push((num, val));
        } else {
            return Err(ArticleQueryError::NoCurrentArticle);
        }
    } else if session.current_group().is_none() {
        return Err(ArticleQueryError::NoGroup);
    } else {
        return Err(ArticleQueryError::NoCurrentArticle);
    }

    Ok(values)
}
