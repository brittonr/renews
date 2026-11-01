//! Header-only notifications emitted after an article is stored.
//!
//! The iroh listener installs a fanout. Other builds leave it unset, and
//! storage still records the article.

use crate::Message;
use std::sync::{Arc, RwLock};

/// One group placement of a stored article. Bodies are never included.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArticleNote {
    pub group: String,
    pub number: u64,
    pub message_id: String,
    pub subject: String,
    pub from: String,
    pub date: String,
}

/// Receives notes. Implementations must not block the storage path.
pub trait ArticleFanout: Send + Sync {
    fn publish(&self, notes: Vec<ArticleNote>);
}

/// Bytes covered by the server endpoint signature.
///
/// The same layout is implemented in `iroh-news`. A test checks they match.
#[must_use]
pub fn signed_bytes(note: &ArticleNote) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"onix-nntp-notify/1\0");
    push_field(&mut out, &note.group);
    out.extend_from_slice(&note.number.to_be_bytes());
    out.push(0);
    push_field(&mut out, &note.message_id);
    push_field(&mut out, &note.subject);
    push_field(&mut out, &note.from);
    push_field(&mut out, &note.date);
    out
}

fn push_field(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(value.as_bytes());
    out.push(0);
}

/// Build notes from the numbers `store_article` assigned.
///
/// Fields are truncated so a notification stays a small header.
#[must_use]
pub fn notes_from(article: &Message, placed: &[(String, u64)]) -> Vec<ArticleNote> {
    let message_id = truncate(header(article, "Message-ID"), 200);
    if message_id.is_empty() {
        return Vec::new();
    }
    let subject = truncate(header(article, "Subject"), 180);
    let from = truncate(header(article, "From"), 160);
    let date = truncate(header(article, "Date"), 80);
    placed
        .iter()
        .filter(|(group, _)| !group.is_empty() && group.len() <= 80)
        .map(|(group, number)| ArticleNote {
            group: group.clone(),
            number: *number,
            message_id: message_id.clone(),
            subject: subject.clone(),
            from: from.clone(),
            date: date.clone(),
        })
        .collect()
}

fn header<'a>(article: &'a Message, name: &str) -> &'a str {
    article
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
        .unwrap_or("")
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut out = String::new();
    for ch in value.chars() {
        if ch == '\r' || ch == '\n' || ch == '\0' {
            continue;
        }
        if out.chars().count() >= max_chars {
            break;
        }
        out.push(ch);
    }
    out
}

/// Shared slot so the queue and the direct-store handlers see the same fanout.
#[derive(Clone, Default)]
pub struct FanoutSlot {
    inner: Arc<RwLock<Option<Arc<dyn ArticleFanout>>>>,
}

impl FanoutSlot {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, fanout: Arc<dyn ArticleFanout>) {
        *self.inner.write().unwrap_or_else(|err| err.into_inner()) = Some(fanout);
    }

    pub fn publish(&self, article: &Message, placed: &[(String, u64)]) {
        let notes = notes_from(article, placed);
        if notes.is_empty() {
            return;
        }
        let guard = self.inner.read().unwrap_or_else(|err| err.into_inner());
        if let Some(fanout) = guard.as_ref() {
            fanout.publish(notes);
        }
    }
}
