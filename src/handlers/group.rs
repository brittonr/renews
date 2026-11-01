//! Group and listing command handlers.

use super::utils::{write_lines, write_simple};
use super::{CommandHandler, HandlerContext, HandlerResult};
use crate::error::StorageError;
use crate::responses::*;
use crate::{parse_datetime, wildmat};
use futures_util::{StreamExt, TryStreamExt};
use tokio::io::AsyncWriteExt;
use tracing::Span;

/// Handler for the GROUP command.
pub struct GroupHandler;

impl CommandHandler for GroupHandler {
    async fn handle(ctx: &mut HandlerContext, args: &[String]) -> HandlerResult {
        if let Some(group_name) = args.first() {
            Span::current().record("group", group_name.as_str());

            // Check if the group exists using the storage interface
            if !ctx.storage.group_exists(group_name).await? {
                let err = StorageError::GroupNotFound(group_name.clone());
                tracing::debug!(error = %err, "Group lookup failed");
                Span::current().record("outcome", "not_found");
                write_simple(&mut ctx.writer, RESP_411_NO_SUCH_GROUP).await?;
                return Ok(());
            }

            let stream = ctx.storage.list_article_numbers(group_name);
            let nums = stream.try_collect::<Vec<u64>>().await?;
            let count = nums.len();
            let high = nums.last().copied().unwrap_or(0);
            let low = nums.first().copied().unwrap_or(0);

            ctx.session
                .select_group(group_name.clone(), nums.first().copied());

            Span::current().record("article_count", count as u64);
            Span::current().record("outcome", "success");

            write_simple(
                &mut ctx.writer,
                &format!("211 {count} {low} {high} {group_name}\r\n"),
            )
            .await?;
        } else {
            write_simple(&mut ctx.writer, RESP_501_NOT_ENOUGH).await?;
        }
        Ok(())
    }
}

/// Handler for the LIST command.
pub struct ListHandler;

impl CommandHandler for ListHandler {
    async fn handle(ctx: &mut HandlerContext, args: &[String]) -> HandlerResult {
        if let Some(keyword) = args.first() {
            match keyword.as_str() {
                "ACTIVE" => {
                    handle_list_active(ctx, args.get(1)).await?;
                }
                "NEWSGROUPS" => {
                    handle_list_newsgroups(ctx).await?;
                }
                "ACTIVE.TIMES" => {
                    handle_list_active_times(ctx).await?;
                }
                "OVERVIEW.FMT" => {
                    handle_list_overview_fmt(ctx).await?;
                }
                "HEADERS" => {
                    handle_list_headers(ctx).await?;
                }
                "BOTGROUPS" => {
                    handle_list_bot_groups(ctx).await?;
                }
                "DISTRIB.PATS" => {
                    write_simple(&mut ctx.writer, RESP_503_NOT_SUPPORTED).await?;
                }
                _ => {
                    write_simple(&mut ctx.writer, RESP_501_UNKNOWN_KEYWORD).await?;
                }
            }
        } else {
            // Default LIST without keyword behaves like LIST ACTIVE
            handle_list_active(ctx, None).await?;
        }
        Ok(())
    }
}

/// Handler for the LISTGROUP command.
pub struct ListGroupHandler;

impl CommandHandler for ListGroupHandler {
    async fn handle(ctx: &mut HandlerContext, args: &[String]) -> HandlerResult {
        let group_name = if let Some(name) = args.first() {
            name.clone()
        } else if let Some(current) = ctx.session.current_group() {
            current.to_string()
        } else {
            write_simple(&mut ctx.writer, RESP_412_NO_GROUP).await?;
            return Ok(());
        };

        write_simple(&mut ctx.writer, RESP_211_LISTGROUP).await?;
        let mut stream = ctx.storage.list_article_numbers(&group_name);
        while let Some(result) = stream.next().await {
            let num = result?;
            ctx.writer.write_all(num.to_string().as_bytes()).await?;
            ctx.writer.write_all(b"\r\n").await?;
        }
        ctx.writer.write_all(RESP_DOT_CRLF.as_bytes()).await?;
        Ok(())
    }
}

/// Handler for the NEXT command.
pub struct NextHandler;

impl CommandHandler for NextHandler {
    async fn handle(ctx: &mut HandlerContext, _args: &[String]) -> HandlerResult {
        navigate_article(ctx, NavigationDirection::Next).await
    }
}

/// Handler for the LAST command.
pub struct LastHandler;

impl CommandHandler for LastHandler {
    async fn handle(ctx: &mut HandlerContext, _args: &[String]) -> HandlerResult {
        navigate_article(ctx, NavigationDirection::Previous).await
    }
}

/// Handler for the NEWGROUPS command.
pub struct NewGroupsHandler;

impl CommandHandler for NewGroupsHandler {
    async fn handle(ctx: &mut HandlerContext, args: &[String]) -> HandlerResult {
        if args.len() < 2 {
            write_simple(&mut ctx.writer, RESP_501_NOT_ENOUGH).await?;
            return Ok(());
        }

        let date = &args[0];
        let time = &args[1];
        let gmt = match args.get(2) {
            Some(arg) => {
                if !arg.eq_ignore_ascii_case("GMT") {
                    write_simple(&mut ctx.writer, RESP_501_INVALID_ARG).await?;
                    return Ok(());
                }
                true
            }
            None => false,
        };
        let Ok(since) = parse_datetime(date, time, gmt) else {
            write_simple(&mut ctx.writer, RESP_501_INVALID_DATE).await?;
            return Ok(());
        };

        write_simple(&mut ctx.writer, RESP_231_NEWGROUPS).await?;
        let mut stream = ctx.storage.list_groups_since(since);
        while let Some(result) = stream.next().await {
            let group = result?;
            ctx.writer.write_all(group.as_bytes()).await?;
            ctx.writer.write_all(b"\r\n").await?;
        }
        ctx.writer.write_all(RESP_DOT_CRLF.as_bytes()).await?;
        Ok(())
    }
}

/// Handler for the NEWNEWS command.
pub struct NewNewsHandler;

impl CommandHandler for NewNewsHandler {
    async fn handle(ctx: &mut HandlerContext, args: &[String]) -> HandlerResult {
        if args.len() < 3 {
            write_simple(&mut ctx.writer, RESP_501_NOT_ENOUGH).await?;
            return Ok(());
        }

        let wildmat_pattern = &args[0];
        let date = &args[1];
        let time = &args[2];
        let gmt = match args.get(3) {
            Some(arg) => {
                if !arg.eq_ignore_ascii_case("GMT") {
                    write_simple(&mut ctx.writer, RESP_501_INVALID_ARG).await?;
                    return Ok(());
                }
                true
            }
            None => false,
        };
        let Ok(since) = parse_datetime(date, time, gmt) else {
            write_simple(&mut ctx.writer, RESP_501_INVALID_DATE).await?;
            return Ok(());
        };

        write_simple(&mut ctx.writer, RESP_230_NEWNEWS).await?;
        let mut groups_stream = ctx.storage.list_groups();
        while let Some(result) = groups_stream.next().await {
            let group = result?;
            if wildmat::wildmat(&group, wildmat_pattern) {
                let mut articles_stream = ctx.storage.list_article_ids_since(&group, since);
                while let Some(article_result) = articles_stream.next().await {
                    let article_id = article_result?;
                    ctx.writer.write_all(article_id.as_bytes()).await?;
                    ctx.writer.write_all(b"\r\n").await?;
                }
            }
        }

        ctx.writer.write_all(RESP_DOT_CRLF.as_bytes()).await?;
        Ok(())
    }
}

// Helper functions for LIST subcommands

async fn handle_list_active(ctx: &mut HandlerContext, pattern: Option<&String>) -> HandlerResult {
    write_simple(&mut ctx.writer, RESP_215_LIST_FOLLOWS).await?;
    let mut groups_stream = ctx.storage.list_groups();
    while let Some(result) = groups_stream.next().await {
        let group = result?;
        if let Some(pat) = pattern
            && !wildmat::wildmat(&group, pat)
        {
            continue;
        }

        let mut nums_stream = ctx.storage.list_article_numbers(&group);
        let mut low = None;
        let mut high = None;

        while let Some(result) = nums_stream.next().await {
            let num = result?;
            if low.is_none() {
                low = Some(num);
            }
            high = Some(num);
        }

        let low = low.unwrap_or(0);
        let high = high.unwrap_or(0);

        ctx.writer.write_all(group.as_bytes()).await?;
        ctx.writer.write_all(b" ").await?;
        ctx.writer.write_all(high.to_string().as_bytes()).await?;
        ctx.writer.write_all(b" ").await?;
        ctx.writer.write_all(low.to_string().as_bytes()).await?;
        ctx.writer.write_all(b" y\r\n").await?;
    }

    ctx.writer.write_all(RESP_DOT_CRLF.as_bytes()).await?;
    Ok(())
}

async fn handle_list_bot_groups(ctx: &mut HandlerContext) -> HandlerResult {
    let catalog = ctx.storage.list_group_catalog().await?;
    ctx.writer
        .write_all(RESP_215_INFO_FOLLOWS.as_bytes())
        .await?;
    for row in catalog {
        let line = format!(
            "{}\t{}\t{}\t{}\r\n",
            row.name, row.description, row.creator, row.created_at
        );
        ctx.writer.write_all(line.as_bytes()).await?;
    }
    ctx.writer.write_all(RESP_DOT_CRLF.as_bytes()).await?;
    Ok(())
}

/// `NEWGROUP name description...` creates a group for an authenticated account.
pub struct NewGroupHandler;

impl CommandHandler for NewGroupHandler {
    async fn handle(ctx: &mut HandlerContext, args: &[String]) -> HandlerResult {
        if !ctx.session.is_authenticated() {
            write_simple(&mut ctx.writer, RESP_480_AUTH_REQUIRED).await?;
            return Ok(());
        }
        let Some(name) = args.first() else {
            write_simple(&mut ctx.writer, RESP_501_NOT_ENOUGH).await?;
            return Ok(());
        };
        let description = args.iter().skip(1).cloned().collect::<Vec<_>>().join(" ");
        let (enabled, prefixes, per_day, max_groups, site) = {
            let cfg = ctx.config.read().await;
            (
                cfg.bots_can_create_groups,
                cfg.group_prefixes.clone(),
                cfg.group_creates_per_day,
                cfg.max_groups,
                cfg.site_name.clone(),
            )
        };
        let username = ctx.session.username().unwrap_or("").to_string();
        let since = chrono::Utc::now().timestamp().saturating_sub(86_400);
        let creates_today = ctx.storage.group_creates_since(&username, since).await?;
        let group_count = ctx.storage.count_groups().await?;
        if let Err(err) = crate::groups::check_new_group(
            &crate::groups::GroupCreatePolicy {
                enabled,
                prefixes: &prefixes,
                group_count,
                creates_today,
                max_groups,
                creates_per_day: per_day,
                is_admin: ctx.session.is_admin(),
            },
            name,
            &description,
        ) {
            tracing::info!(group = %name, error = %err, "NEWGROUP rejected");
            write_simple(&mut ctx.writer, &format!("441 {err}\r\n")).await?;
            return Ok(());
        }
        if ctx.storage.group_exists(name).await? {
            write_simple(&mut ctx.writer, "441 group already exists\r\n").await?;
            return Ok(());
        }
        if let Err(err) = ctx
            .storage
            .create_bot_group(name, &description, &username)
            .await
        {
            tracing::info!(group = %name, error = %err, "NEWGROUP failed");
            write_simple(&mut ctx.writer, "441 group creation failed\r\n").await?;
            return Ok(());
        }
        if ctx.storage.group_exists("bots.announce").await? {
            let mut message = crate::Message {
                headers: smallvec::smallvec![
                    ("From".into(), format!("{username}@bots.local")),
                    ("Newsgroups".into(), "bots.announce".into()),
                    ("Subject".into(), format!("new group {name}")),
                    (
                        "Message-ID".into(),
                        format!(
                            "<newgroup.{}.{}@{}>",
                            name.replace('.', "-"),
                            chrono::Utc::now().timestamp_millis(),
                            site
                        )
                    ),
                ],
                body: format!("{username} created {name}\n\n{description}\n"),
            };
            crate::parse::ensure_date(&mut message);
            match ctx.storage.store_article(&message).await {
                Ok(placed) => ctx.queue.announce(&message, &placed),
                Err(err) => tracing::warn!(error = %err, "failed to announce new group"),
            }
        }
        tracing::info!(group = %name, creator = %username, "group created");
        write_simple(&mut ctx.writer, &format!("231 {name} group created\r\n")).await?;
        Ok(())
    }
}

async fn handle_list_newsgroups(ctx: &mut HandlerContext) -> HandlerResult {
    write_simple(&mut ctx.writer, RESP_215_DESCRIPTIONS).await?;
    let mut groups_stream = ctx.storage.list_groups_with_descriptions();
    while let Some(result) = groups_stream.next().await {
        let (group, description) = result?;
        ctx.writer
            .write_all(format!("{group} {description}\r\n").as_bytes())
            .await?;
    }
    ctx.writer.write_all(RESP_DOT_CRLF.as_bytes()).await?;
    Ok(())
}

async fn handle_list_active_times(ctx: &mut HandlerContext) -> HandlerResult {
    write_simple(&mut ctx.writer, RESP_215_INFO_FOLLOWS).await?;
    let mut stream = ctx.storage.list_groups_with_times();
    while let Some(result) = stream.next().await {
        let (group, time) = result?;
        ctx.writer
            .write_all(format!("{group} {time} -\r\n").as_bytes())
            .await?;
    }

    ctx.writer.write_all(RESP_DOT_CRLF.as_bytes()).await?;
    Ok(())
}

async fn handle_list_overview_fmt(ctx: &mut HandlerContext) -> HandlerResult {
    use crate::overview::get_overview_format_lines;

    ctx.writer
        .write_all(RESP_215_OVERVIEW_FMT.as_bytes())
        .await?;

    let format_lines = get_overview_format_lines();
    for line in format_lines {
        ctx.writer.write_all(line.as_bytes()).await?;
    }

    ctx.writer.write_all(RESP_DOT_CRLF.as_bytes()).await?;
    Ok(())
}

async fn handle_list_headers(ctx: &mut HandlerContext) -> HandlerResult {
    write_lines(
        &mut ctx.writer,
        &[
            RESP_215_METADATA,
            RESP_COLON,
            RESP_LINES,
            RESP_BYTES,
            RESP_DOT_CRLF,
        ],
    )
    .await
}

/// Navigate to the next or previous article in the current group.
async fn navigate_article(
    ctx: &mut HandlerContext,
    direction: NavigationDirection,
) -> HandlerResult {
    let (group, current) = match (ctx.session.current_group(), ctx.session.current_article()) {
        (Some(group), Some(current)) => (group.to_string(), current),
        _ => {
            write_simple(&mut ctx.writer, RESP_412_NO_GROUP).await?;
            return Ok(());
        }
    };

    let stream = ctx.storage.list_article_numbers(&group);
    let nums = stream.try_collect::<Vec<u64>>().await?;
    let pos = match nums.iter().position(|&n| n == current) {
        Some(pos) => pos,
        None => {
            write_simple(&mut ctx.writer, direction.error_response()).await?;
            return Ok(());
        }
    };

    let new_pos = match direction {
        NavigationDirection::Next => pos + 1,
        NavigationDirection::Previous => pos.saturating_sub(1),
    };

    if let Some(&new_num) = nums.get(new_pos) {
        if let Some(article) = ctx.storage.get_article_by_number(&group, new_num).await? {
            ctx.session.set_current_article(new_num);
            let id = super::utils::extract_message_id(&article).unwrap_or_default();
            write_simple(
                &mut ctx.writer,
                &format!("223 {new_num} {id} article exists\r\n"),
            )
            .await?;
        } else {
            write_simple(&mut ctx.writer, direction.error_response()).await?;
        }
    } else {
        write_simple(&mut ctx.writer, direction.error_response()).await?;
    }

    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum NavigationDirection {
    Next,
    Previous,
}

impl NavigationDirection {
    fn error_response(&self) -> &'static str {
        match self {
            Self::Next => RESP_421_NO_NEXT,
            Self::Previous => RESP_422_NO_PREV,
        }
    }
}
