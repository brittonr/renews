//! Read-only snapshot of groups, threads, and article text.
//!
//! [`fetch_snapshot`] uses the same authenticated session as `iroh-news read`.
//! It lists groups and fetches article bodies. It does not post, and it does
//! not move the cursors used by `--new` and `watch`.
//!
//! Threading keys off `Message-ID` and `References` (oldest ancestor first,
//! immediate parent last). The tree is built only after the whole group is in
//! hand, so a reply that arrives before its parent still hangs under that
//! parent. A missing parent attaches to the nearest ancestor that is present.
//! An article whose whole `References` chain is absent becomes its own root.
//! A link that would close a cycle is skipped; articles are considered in
//! ascending article-number order, so the result does not depend on fetch order.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};

use crate::{Article, GroupEntry, ReadRequest, Session};

/// `version` field written into every snapshot.
pub const SNAPSHOT_VERSION: u32 = 1;

/// `generator` field written into every snapshot.
pub const SNAPSHOT_GENERATOR: &str = "iroh-news";

/// File name inside the `--out` directory.
pub const SNAPSHOT_FILE: &str = "snapshot.json";

/// One snapshot file. `groups` is sorted by name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    /// Schema version. This client writes [`SNAPSHOT_VERSION`].
    pub version: u32,
    /// Always [`SNAPSHOT_GENERATOR`].
    pub generator: String,
    /// UTC time the file was written, `YYYY-MM-DDTHH:MM:SSZ`.
    pub generated_at: String,
    /// True when articles already stored in the previous file were kept.
    pub incremental: bool,
    pub groups: Vec<SnapshotGroup>,
}

/// One newsgroup, with its threads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotGroup {
    pub name: String,
    /// Description from `LIST BOTGROUPS`. Empty when the server has none.
    #[serde(default)]
    pub description: String,
    /// Highest article number the server reported. `0` when the group is empty.
    pub high: u64,
    /// Lowest article number the server reported. `0` when the group is empty.
    pub low: u64,
    /// Posting status (`y`, `n`, or `m`).
    pub posting: String,
    /// Account that created the group. Empty when the server has none.
    #[serde(default)]
    pub creator: String,
    /// Threads sorted by the root article's number, then message-id.
    pub threads: Vec<SnapshotThread>,
}

/// One reply tree. `articles[0]` is the root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotThread {
    /// Message-id of the depth-0 article.
    pub root_message_id: String,
    /// Preorder: a parent appears before its children. Siblings are in
    /// ascending article-number order.
    pub articles: Vec<SnapshotArticle>,
}

/// One article inside a thread.
///
/// `date` is the `Date` header as the server stored it, not a normalized
/// timestamp. `body` is the article body with `\n` line endings, after NNTP
/// dot-unstuffing. It is not MIME-decoded and not HTML.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotArticle {
    pub number: u64,
    pub message_id: String,
    /// Thread parent resolved from `References`.
    ///
    /// `null` for the thread root. This may be an older entry in `references`
    /// when the immediate parent is missing, or when linking to it would
    /// cycle. It is not always the last `references` entry.
    pub parent_message_id: Option<String>,
    /// Distance from the thread root. Zero for the root.
    pub depth: u32,
    pub from: String,
    pub subject: String,
    pub date: String,
    /// Message-ids from the `References` header, oldest first, each wrapped
    /// in angle brackets. Comments are removed. Duplicates are kept.
    pub references: Vec<String>,
    pub body: String,
}

/// Article fields threading needs. `references` is the raw header value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadArticle {
    pub number: u64,
    pub message_id: String,
    pub from: String,
    pub subject: String,
    pub date: String,
    pub references: String,
    pub body: String,
}

/// Group metadata copied from `LIST ACTIVE` / `LIST BOTGROUPS`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupInfo {
    pub name: String,
    pub description: String,
    pub high: u64,
    pub low: u64,
    pub posting: String,
    pub creator: String,
}

/// One group's freshly read articles, before they are merged and threaded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GroupFetch {
    pub info: GroupInfo,
    pub articles: Vec<ThreadArticle>,
    /// Discard articles previously stored for this group.
    pub replace: bool,
}

/// What `iroh-news snapshot` asked to fetch.
#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Ignore stored articles and read each selected group from the start.
    pub full: bool,
    /// Empty means every group the server lists.
    pub groups: Vec<String>,
}

/// Result of a fetch, before it is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchResult {
    pub snapshot: Snapshot,
    /// Article bodies pulled from the server on this run.
    pub fetched_articles: usize,
}

/// Counts of what a snapshot contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotCounts {
    pub groups: usize,
    pub threads: usize,
    pub articles: usize,
}

impl Snapshot {
    /// Group, thread, and article totals.
    pub fn counts(&self) -> SnapshotCounts {
        let threads = self.groups.iter().map(|group| group.threads.len()).sum();
        let articles = self
            .groups
            .iter()
            .flat_map(|group| group.threads.iter())
            .map(|thread| thread.articles.len())
            .sum();
        SnapshotCounts {
            groups: self.groups.len(),
            threads,
            articles,
        }
    }

    /// Highest article number stored for `group`.
    ///
    /// The next incremental read asks for numbers strictly greater than this.
    /// `None` when the group is absent or has no articles.
    pub fn high_water(&self, group: &str) -> Option<u64> {
        self.groups
            .iter()
            .find(|entry| entry.name == group)
            .and_then(|entry| {
                entry
                    .threads
                    .iter()
                    .flat_map(|thread| thread.articles.iter())
                    .map(|article| article.number)
                    .max()
            })
    }
}

impl From<&Article> for ThreadArticle {
    fn from(article: &Article) -> Self {
        Self {
            number: article.number,
            message_id: article.message_id.clone(),
            from: article.from.clone(),
            subject: article.subject.clone(),
            date: article.date.clone(),
            references: article.references.clone(),
            body: article.body.clone(),
        }
    }
}

impl From<&SnapshotArticle> for ThreadArticle {
    fn from(article: &SnapshotArticle) -> Self {
        Self {
            number: article.number,
            message_id: article.message_id.clone(),
            from: article.from.clone(),
            subject: article.subject.clone(),
            date: article.date.clone(),
            references: article.references.join(" "),
            body: article.body.clone(),
        }
    }
}

impl GroupInfo {
    fn from_entry(group: &GroupEntry) -> Self {
        Self {
            name: group.name.clone(),
            description: group.description.clone(),
            high: group.high,
            low: group.low,
            posting: group.posting.clone(),
            creator: group.creator.clone(),
        }
    }
}

/// Message-ids in a `References` header, oldest first.
///
/// Parentheses are comments and are ignored, including a message-id written
/// inside a comment. Tokens that are not `<id>` are ignored.
pub fn parse_references(raw: &str) -> Vec<String> {
    let stripped = strip_comments(raw);
    let bytes = stripped.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        let Some(rel_end) = bytes[i + 1..].iter().position(|byte| *byte == b'>') else {
            break;
        };
        let start = i + 1;
        let end = start + rel_end;
        let id = &stripped[start..end];
        if !id.is_empty()
            && !id
                .chars()
                .any(|ch| ch.is_whitespace() || ch == '<' || ch == '>')
        {
            out.push(format!("<{id}>"));
        }
        i = end + 1;
    }
    out
}

fn strip_comments(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    let mut depth = 0i32;
    for ch in raw.chars() {
        match ch {
            '(' => depth += 1,
            ')' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(ch),
            _ => {}
        }
    }
    out
}

fn canonical_message_id(number: u64, message_id: &str) -> String {
    let trimmed = message_id.trim();
    let inner = if let Some(body) = trimmed
        .strip_prefix('<')
        .and_then(|value| value.strip_suffix('>'))
    {
        body.trim()
    } else {
        trimmed
    };
    if inner.is_empty() {
        format!("<no-message-id-{number}@snapshot.local>")
    } else {
        format!("<{inner}>")
    }
}

fn canonicalize(article: &ThreadArticle) -> ThreadArticle {
    let mut out = article.clone();
    out.message_id = canonical_message_id(article.number, &article.message_id);
    out
}

/// Build reply threads.
///
/// Input order does not matter. Two articles with the same message-id collapse
/// to the one with the lower article number.
pub fn thread_articles(articles: &[ThreadArticle]) -> Vec<SnapshotThread> {
    let mut items: Vec<ThreadArticle> = articles.iter().map(canonicalize).collect();
    items.sort_by(|a, b| {
        a.number
            .cmp(&b.number)
            .then_with(|| a.message_id.cmp(&b.message_id))
    });
    let mut seen = std::collections::HashSet::new();
    items.retain(|article| seen.insert(article.message_id.clone()));

    let refs: Vec<Vec<String>> = items
        .iter()
        .map(|article| parse_references(&article.references))
        .collect();
    let parent = link_parents(&items, &refs);
    let mut children = vec![Vec::<usize>::new(); items.len()];
    let mut roots = Vec::new();
    for (index, parent_index) in parent.iter().enumerate() {
        if let Some(parent_index) = parent_index {
            children[*parent_index].push(index);
        } else {
            roots.push(index);
        }
    }
    let by_number = |a: &usize, b: &usize| {
        items[*a]
            .number
            .cmp(&items[*b].number)
            .then_with(|| items[*a].message_id.cmp(&items[*b].message_id))
    };
    roots.sort_by(by_number);
    for kids in &mut children {
        kids.sort_by(by_number);
    }

    let forest = Forest {
        items: &items,
        refs: &refs,
        parent: &parent,
        children: &children,
    };
    let mut threads = Vec::new();
    let mut emitted = vec![false; items.len()];
    for root in roots {
        let mut articles = Vec::new();
        emit(
            &forest,
            root,
            0,
            &mut Vec::new(),
            &mut emitted,
            &mut articles,
        );
        if articles.is_empty() {
            continue;
        }
        threads.push(SnapshotThread {
            root_message_id: articles[0].message_id.clone(),
            articles,
        });
    }
    threads
}

struct Forest<'a> {
    items: &'a [ThreadArticle],
    refs: &'a [Vec<String>],
    parent: &'a [Option<usize>],
    children: &'a [Vec<usize>],
}

fn link_parents(items: &[ThreadArticle], refs: &[Vec<String>]) -> Vec<Option<usize>> {
    let by_id: HashMap<&str, usize> = items
        .iter()
        .enumerate()
        .map(|(index, article)| (article.message_id.as_str(), index))
        .collect();
    let mut parent = vec![None; items.len()];
    for (index, article) in items.iter().enumerate() {
        for candidate in refs[index].iter().rev() {
            if candidate == &article.message_id {
                continue;
            }
            let Some(&parent_index) = by_id.get(candidate.as_str()) else {
                continue;
            };
            if would_cycle(index, parent_index, &parent) {
                continue;
            }
            parent[index] = Some(parent_index);
            break;
        }
    }
    parent
}

fn would_cycle(child: usize, parent: usize, parents: &[Option<usize>]) -> bool {
    if parent == child {
        return true;
    }
    let mut cursor = Some(parent);
    let mut steps = 0usize;
    while let Some(index) = cursor {
        if index == child || steps > parents.len() {
            return true;
        }
        steps += 1;
        cursor = parents.get(index).copied().flatten();
    }
    false
}

fn emit(
    forest: &Forest<'_>,
    index: usize,
    depth: u32,
    stack: &mut Vec<usize>,
    emitted: &mut [bool],
    out: &mut Vec<SnapshotArticle>,
) {
    if emitted[index] || stack.contains(&index) || depth as usize > forest.items.len() {
        return;
    }
    stack.push(index);
    emitted[index] = true;
    let article = &forest.items[index];
    out.push(SnapshotArticle {
        number: article.number,
        message_id: article.message_id.clone(),
        parent_message_id: forest.parent[index]
            .map(|parent| forest.items[parent].message_id.clone()),
        depth,
        from: article.from.clone(),
        subject: article.subject.clone(),
        date: article.date.clone(),
        references: forest.refs[index].clone(),
        body: article.body.clone(),
    });
    for &child in &forest.children[index] {
        emit(forest, child, depth + 1, stack, emitted, out);
    }
    stack.pop();
}

/// Merge `fetches` into `previous` and thread each group.
///
/// `keep_unlisted` copies groups that are in `previous` but not in `fetches`.
/// Those groups are not rethreaded. Every fetched group is rethreaded.
pub fn assemble_snapshot(
    previous: Option<&Snapshot>,
    fetches: &[GroupFetch],
    keep_unlisted: bool,
    generated_at: &str,
) -> Snapshot {
    let fetched_names: std::collections::HashSet<&str> = fetches
        .iter()
        .map(|fetch| fetch.info.name.as_str())
        .collect();
    let mut groups = Vec::new();
    let mut carried = false;
    if keep_unlisted && let Some(previous) = previous {
        for group in &previous.groups {
            if !fetched_names.contains(group.name.as_str()) {
                if group
                    .threads
                    .iter()
                    .any(|thread| !thread.articles.is_empty())
                {
                    carried = true;
                }
                groups.push(group.clone());
            }
        }
    }

    for fetch in fetches {
        let old = if fetch.replace {
            Vec::new()
        } else {
            stored_articles(previous, &fetch.info.name)
        };
        if !old.is_empty() {
            carried = true;
        }
        let merged = merge_articles(old, fetch.articles.clone());
        groups.push(SnapshotGroup {
            name: fetch.info.name.clone(),
            description: fetch.info.description.clone(),
            high: fetch.info.high,
            low: fetch.info.low,
            posting: fetch.info.posting.clone(),
            creator: fetch.info.creator.clone(),
            threads: thread_articles(&merged),
        });
    }
    groups.sort_by(|a, b| a.name.cmp(&b.name));

    Snapshot {
        version: SNAPSHOT_VERSION,
        generator: SNAPSHOT_GENERATOR.to_string(),
        generated_at: generated_at.to_string(),
        incremental: carried,
        groups,
    }
}

fn stored_articles(previous: Option<&Snapshot>, name: &str) -> Vec<ThreadArticle> {
    let Some(snapshot) = previous else {
        return Vec::new();
    };
    let Some(group) = snapshot.groups.iter().find(|group| group.name == name) else {
        return Vec::new();
    };
    group
        .threads
        .iter()
        .flat_map(|thread| thread.articles.iter().map(ThreadArticle::from))
        .collect()
}

fn merge_articles(old: Vec<ThreadArticle>, new: Vec<ThreadArticle>) -> Vec<ThreadArticle> {
    let mut map = std::collections::BTreeMap::<String, ThreadArticle>::new();
    for article in old.into_iter().chain(new) {
        let article = canonicalize(&article);
        map.insert(article.message_id.clone(), article);
    }
    map.into_values().collect()
}

struct GroupPlan {
    since: Option<u64>,
    replace: bool,
}

fn plan_group(previous: Option<&Snapshot>, group: &GroupInfo, full: bool) -> GroupPlan {
    if full {
        return GroupPlan {
            since: None,
            replace: true,
        };
    }
    let Some(mark) = previous.and_then(|snapshot| snapshot.high_water(&group.name)) else {
        return GroupPlan {
            since: None,
            replace: false,
        };
    };
    // Article numbers only move forward. A lower high means the group was
    // recreated; mixing the old articles with the new numbers would collide.
    if group.high > 0 && group.high < mark {
        return GroupPlan {
            since: None,
            replace: true,
        };
    }
    GroupPlan {
        since: Some(mark),
        replace: false,
    }
}

fn select_groups(listed: &[GroupEntry], only: &[String]) -> anyhow::Result<Vec<GroupInfo>> {
    if only.is_empty() {
        return Ok(listed.iter().map(GroupInfo::from_entry).collect());
    }
    let mut out = Vec::with_capacity(only.len());
    for name in only {
        let Some(group) = listed.iter().find(|group| group.name == *name) else {
            bail!("group {name} is not on the server");
        };
        if out.iter().any(|info: &GroupInfo| info.name == group.name) {
            continue;
        }
        out.push(GroupInfo::from_entry(group));
    }
    Ok(out)
}

/// List groups and read articles, then thread them.
///
/// # Errors
///
/// Returns an error if a requested group is missing, or if a command fails.
/// On error nothing has been written. A previous snapshot passed in is not
/// modified.
pub async fn fetch_snapshot(
    session: &mut Session,
    previous: Option<&Snapshot>,
    options: &FetchOptions,
    generated_at: &str,
) -> anyhow::Result<FetchResult> {
    let listed = session.groups().await?;
    let selected = select_groups(&listed, &options.groups)?;
    let mut fetches = Vec::with_capacity(selected.len());
    let mut fetched_articles = 0usize;
    for info in selected {
        let plan = plan_group(previous, &info, options.full);
        let articles = session
            .read(ReadRequest {
                group: info.name.clone(),
                since: plan.since,
                limit: None,
                update_cursor: false,
            })
            .await?;
        fetched_articles += articles.len();
        fetches.push(GroupFetch {
            info,
            articles: articles.iter().map(ThreadArticle::from).collect(),
            replace: plan.replace,
        });
    }
    let snapshot = assemble_snapshot(previous, &fetches, !options.groups.is_empty(), generated_at);
    Ok(FetchResult {
        snapshot,
        fetched_articles,
    })
}

/// Read `snapshot.json` from `dir`.
///
/// # Errors
///
/// Returns an error if the file exists but is not a snapshot of
/// [`SNAPSHOT_VERSION`], or if the JSON cannot be parsed. A missing file is
/// `Ok(None)`, which is how the first run starts.
pub fn load_snapshot(dir: &Path) -> anyhow::Result<Option<Snapshot>> {
    let path = dir.join(SNAPSHOT_FILE);
    if !path.exists() {
        return Ok(None);
    }
    let text = fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
    let value: serde_json::Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let version = value.get("version").and_then(serde_json::Value::as_u64);
    if version != Some(u64::from(SNAPSHOT_VERSION)) {
        let found = version
            .map(|number| number.to_string())
            .unwrap_or_else(|| "missing".to_string());
        bail!(
            "{} is snapshot version {found}; this client writes version {SNAPSHOT_VERSION}",
            path.display()
        );
    }
    let snapshot =
        serde_json::from_value(value).with_context(|| format!("parsing {}", path.display()))?;
    Ok(Some(snapshot))
}

/// Write `snapshot.json` atomically (temp file, then rename).
///
/// # Errors
///
/// Returns an error if the directory cannot be created or the file cannot
/// be replaced. A reader sees either the previous file or the new one.
pub fn write_snapshot(dir: &Path, snapshot: &Snapshot) -> anyhow::Result<PathBuf> {
    if snapshot.version != SNAPSHOT_VERSION {
        bail!("refusing to write snapshot version {}", snapshot.version);
    }
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let path = dir.join(SNAPSHOT_FILE);
    let tmp = dir.join(format!("{SNAPSHOT_FILE}.tmp"));
    let mut body = serde_json::to_vec_pretty(snapshot).context("encoding snapshot")?;
    body.push(b'\n');
    fs::write(&tmp, &body).with_context(|| format!("writing {}", tmp.display()))?;
    fs::rename(&tmp, &path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(path)
}

/// Current UTC time as `YYYY-MM-DDTHH:MM:SSZ`.
pub fn utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0);
    format_rfc3339(secs)
}

/// Format a Unix timestamp as UTC `YYYY-MM-DDTHH:MM:SSZ`.
pub fn format_rfc3339(unix_secs: u64) -> String {
    let days = (unix_secs / 86_400) as i64;
    let tod = unix_secs % 86_400;
    let (year, month, day) = civil_from_days(days);
    let hour = tod / 3600;
    let minute = (tod % 3600) / 60;
    let second = tod % 60;
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Howard Hinnant's `civil_from_days`. `days` is days since 1970-01-01.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let mut year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn art(number: u64, id: &str, references: &str) -> ThreadArticle {
        ThreadArticle {
            number,
            message_id: id.to_string(),
            from: format!("User {number} <user{number}@bots.local>"),
            subject: format!("subject {number}"),
            date: "Tue, 03 Nov 2026 12:00:00 +0000".to_string(),
            references: references.to_string(),
            body: format!("body {number}\n"),
        }
    }

    fn info(name: &str, high: u64) -> GroupInfo {
        GroupInfo {
            name: name.to_string(),
            description: String::new(),
            high,
            low: u64::from(high > 0),
            posting: "y".to_string(),
            creator: String::new(),
        }
    }

    fn fetch(name: &str, high: u64, articles: Vec<ThreadArticle>, replace: bool) -> GroupFetch {
        GroupFetch {
            info: info(name, high),
            articles,
            replace,
        }
    }

    fn assert_forest(threads: &[SnapshotThread]) {
        let mut seen = std::collections::HashSet::new();
        let mut previous_root: Option<(u64, String)> = None;
        for thread in threads {
            assert!(!thread.articles.is_empty());
            let root = &thread.articles[0];
            assert_eq!(thread.root_message_id, root.message_id);
            assert!(root.parent_message_id.is_none());
            assert_eq!(root.depth, 0);
            if let Some((number, message_id)) = &previous_root {
                assert!(
                    root.number > *number
                        || (root.number == *number
                            && root.message_id.as_str() >= message_id.as_str())
                );
            }
            previous_root = Some((root.number, root.message_id.clone()));

            let mut index = HashMap::new();
            for (position, article) in thread.articles.iter().enumerate() {
                assert!(
                    seen.insert(article.message_id.clone()),
                    "duplicate {}",
                    article.message_id
                );
                index.insert(article.message_id.clone(), position);
            }
            let mut last_sibling: HashMap<Option<String>, u64> = HashMap::new();
            for (position, article) in thread.articles.iter().enumerate() {
                match &article.parent_message_id {
                    None => assert_eq!(position, 0),
                    Some(parent) => {
                        let parent_at = index[parent];
                        assert!(parent_at < position, "parent after child");
                        assert_eq!(article.depth, thread.articles[parent_at].depth + 1);
                    }
                }
                let key = article.parent_message_id.clone();
                if let Some(previous) = last_sibling.get(&key) {
                    assert!(article.number >= *previous, "siblings out of order");
                }
                last_sibling.insert(key, article.number);
            }
        }
    }

    #[test]
    fn references_extracts_ids_and_ignores_comments() {
        assert!(parse_references("").is_empty());
        assert!(parse_references("   ").is_empty());
        assert!(parse_references("no brackets").is_empty());
        assert!(parse_references("<unclosed").is_empty());
        assert!(parse_references("<has space@x>").is_empty());
        assert_eq!(parse_references("<a@b>"), vec!["<a@b>"]);
        assert_eq!(parse_references("<a@b>\t<c@d>"), vec!["<a@b>", "<c@d>"]);
        assert_eq!(
            parse_references("<a@b> (comment) <c@d>"),
            vec!["<a@b>", "<c@d>"]
        );
        assert_eq!(
            parse_references("<a@b> (see <not@id>) <c@d>"),
            vec!["<a@b>", "<c@d>"]
        );
        assert_eq!(parse_references("<a@b> <a@b>"), vec!["<a@b>", "<a@b>"]);
        assert_eq!(
            parse_references("Re: <root@x>\r\n <child@x>"),
            vec!["<root@x>", "<child@x>"]
        );
    }

    #[test]
    fn linear_thread_is_preorder_and_ignores_input_order() {
        let articles = vec![
            art(1, "<root@x>", ""),
            art(2, "<mid@x>", "<root@x>"),
            art(3, "<leaf@x>", "<root@x> <mid@x>"),
        ];
        let forward = thread_articles(&articles);
        let mut reversed = articles.clone();
        reversed.reverse();
        let backward = thread_articles(&reversed);
        assert_eq!(forward, backward);
        assert_forest(&forward);
        assert_eq!(forward.len(), 1);
        let thread = &forward[0];
        assert_eq!(
            thread
                .articles
                .iter()
                .map(|article| (
                    article.depth,
                    article.message_id.as_str(),
                    article.parent_message_id.as_deref()
                ))
                .collect::<Vec<_>>(),
            vec![
                (0, "<root@x>", None),
                (1, "<mid@x>", Some("<root@x>")),
                (2, "<leaf@x>", Some("<mid@x>")),
            ]
        );
        assert_eq!(thread.articles[2].references, vec!["<root@x>", "<mid@x>"]);
    }

    #[test]
    fn child_with_a_lower_number_still_hangs_under_its_parent() {
        let late_parent = art(5, "<parent@x>", "");
        let early_child = art(1, "<child@x>", "<parent@x>");
        let threads = thread_articles(&[early_child, late_parent]);
        assert_forest(&threads);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].articles[0].message_id, "<parent@x>");
        assert_eq!(
            threads[0].articles[1].parent_message_id.as_deref(),
            Some("<parent@x>")
        );
        assert_eq!(threads[0].articles[1].number, 1);
    }

    #[test]
    fn missing_parent_attaches_to_the_nearest_ancestor() {
        let threads = thread_articles(&[
            art(4, "<leaf@x>", "<root@x> <gone@x>"),
            art(1, "<root@x>", ""),
        ]);
        assert_forest(&threads);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].articles[1].message_id, "<leaf@x>");
        assert_eq!(threads[0].articles[1].depth, 1);
        assert_eq!(
            threads[0].articles[1].parent_message_id.as_deref(),
            Some("<root@x>")
        );
        assert_eq!(
            threads[0].articles[1].references,
            vec!["<root@x>", "<gone@x>"]
        );
    }

    #[test]
    fn missing_entire_chain_is_its_own_root() {
        let threads = thread_articles(&[art(3, "<leaf@x>", "<gone@x> <also@x>")]);
        assert_forest(&threads);
        assert_eq!(threads.len(), 1);
        let article = &threads[0].articles[0];
        assert!(article.parent_message_id.is_none());
        assert_eq!(article.depth, 0);
        assert_eq!(article.references, vec!["<gone@x>", "<also@x>"]);
        assert_eq!(article.body, "body 3\n");
    }

    #[test]
    fn mutual_references_do_not_cycle() {
        let threads = thread_articles(&[art(1, "<a@x>", "<b@x>"), art(2, "<b@x>", "<a@x>")]);
        assert_forest(&threads);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].articles.len(), 2);
        assert_eq!(threads[0].root_message_id, "<b@x>");
        assert_eq!(
            threads[0].articles[1].parent_message_id.as_deref(),
            Some("<b@x>")
        );
    }

    #[test]
    fn three_cycle_is_broken() {
        let threads = thread_articles(&[
            art(1, "<a@x>", "<b@x>"),
            art(2, "<b@x>", "<c@x>"),
            art(3, "<c@x>", "<a@x>"),
        ]);
        assert_forest(&threads);
        assert_eq!(threads.len(), 1);
        assert_eq!(
            threads[0]
                .articles
                .iter()
                .map(|article| article.message_id.as_str())
                .collect::<Vec<_>>(),
            vec!["<c@x>", "<b@x>", "<a@x>"]
        );
    }

    #[test]
    fn cycle_falls_back_to_an_older_reference() {
        let threads = thread_articles(&[
            art(1, "<r@x>", ""),
            art(2, "<a@x>", "<b@x>"),
            art(3, "<b@x>", "<r@x> <a@x>"),
        ]);
        assert_forest(&threads);
        assert_eq!(threads.len(), 1);
        assert_eq!(
            threads[0]
                .articles
                .iter()
                .map(|article| {
                    (
                        article.depth,
                        article.message_id.as_str(),
                        article.parent_message_id.as_deref(),
                    )
                })
                .collect::<Vec<_>>(),
            vec![
                (0, "<r@x>", None),
                (1, "<b@x>", Some("<r@x>")),
                (2, "<a@x>", Some("<b@x>")),
            ]
        );
    }

    #[test]
    fn self_reference_is_a_root() {
        let threads = thread_articles(&[art(1, "<a@x>", "<a@x>")]);
        assert_forest(&threads);
        assert_eq!(threads.len(), 1);
        assert!(threads[0].articles[0].parent_message_id.is_none());
        assert_eq!(threads[0].articles[0].references, vec!["<a@x>"]);
    }

    #[test]
    fn siblings_and_nested_replies_sort_by_article_number() {
        let articles = vec![
            art(4, "<d@x>", "<b@x>"),
            art(3, "<c@x>", "<root@x>"),
            art(1, "<root@x>", ""),
            art(2, "<b@x>", "<root@x>"),
        ];
        let threads = thread_articles(&articles);
        assert_forest(&threads);
        assert_eq!(
            threads[0]
                .articles
                .iter()
                .map(|article| (article.depth, article.message_id.as_str()))
                .collect::<Vec<_>>(),
            vec![(0, "<root@x>"), (1, "<b@x>"), (2, "<d@x>"), (1, "<c@x>")]
        );
    }

    #[test]
    fn same_subject_without_references_stays_two_threads() {
        let mut first = art(1, "<a@x>", "");
        let mut second = art(2, "<b@x>", "");
        first.subject = "hello".to_string();
        second.subject = "hello".to_string();
        let threads = thread_articles(&[second, first]);
        assert_forest(&threads);
        assert_eq!(threads.len(), 2);
    }

    #[test]
    fn duplicate_message_id_keeps_the_lower_number() {
        let mut low = art(1, "<a@x>", "");
        low.body = "low\n".to_string();
        let mut high = art(5, "<a@x>", "");
        high.body = "high\n".to_string();
        let threads = thread_articles(&[high, low]);
        assert_eq!(threads.len(), 1);
        assert_eq!(threads[0].articles.len(), 1);
        assert_eq!(threads[0].articles[0].number, 1);
        assert_eq!(threads[0].articles[0].body, "low\n");
    }

    #[test]
    fn bare_message_id_matches_a_bracketed_reference() {
        let threads = thread_articles(&[art(2, "<child@x>", "<root@x>"), art(1, "root@x", "")]);
        assert_forest(&threads);
        assert_eq!(threads[0].articles[0].message_id, "<root@x>");
        assert_eq!(
            threads[0].articles[1].parent_message_id.as_deref(),
            Some("<root@x>")
        );
    }

    #[test]
    fn empty_message_id_gets_a_stable_placeholder() {
        let threads = thread_articles(&[art(4, "  ", "")]);
        assert_eq!(
            threads[0].articles[0].message_id,
            "<no-message-id-4@snapshot.local>"
        );
        assert_forest(&threads);
    }

    #[test]
    fn empty_input_has_no_threads() {
        assert!(thread_articles(&[]).is_empty());
    }

    #[test]
    fn incremental_merge_rethreads_a_new_reply() {
        let first = assemble_snapshot(
            None,
            &[fetch(
                "bots.general",
                1,
                vec![art(1, "<root@x>", "")],
                false,
            )],
            false,
            "2026-11-03T18:00:00Z",
        );
        assert!(!first.incremental);
        let second = assemble_snapshot(
            Some(&first),
            &[fetch(
                "bots.general",
                2,
                vec![art(2, "<reply@x>", "<root@x>")],
                false,
            )],
            false,
            "2026-11-03T19:00:00Z",
        );
        assert!(second.incremental);
        assert_eq!(second.high_water("bots.general"), Some(2));
        assert_forest(&second.groups[0].threads);
        assert_eq!(second.groups[0].threads.len(), 1);
        assert_eq!(second.groups[0].threads[0].articles.len(), 2);
        assert_eq!(second.groups[0].high, 2);
        assert_eq!(
            second.groups[0].threads[0].articles[1]
                .parent_message_id
                .as_deref(),
            Some("<root@x>")
        );
    }

    #[test]
    fn fetched_copy_replaces_the_stored_message() {
        let first = assemble_snapshot(
            None,
            &[fetch("bots.general", 1, vec![art(1, "<a@x>", "")], false)],
            false,
            "t0",
        );
        let mut updated = art(1, "<a@x>", "");
        updated.body = "edited\n".to_string();
        let second = assemble_snapshot(
            Some(&first),
            &[fetch("bots.general", 1, vec![updated], false)],
            false,
            "t1",
        );
        assert_eq!(second.groups[0].threads[0].articles[0].body, "edited\n");
    }

    #[test]
    fn full_replace_drops_articles_the_server_did_not_return() {
        let first = assemble_snapshot(
            None,
            &[fetch(
                "bots.general",
                2,
                vec![art(1, "<a@x>", ""), art(2, "<b@x>", "<a@x>")],
                false,
            )],
            false,
            "t0",
        );
        let second = assemble_snapshot(
            Some(&first),
            &[fetch("bots.general", 1, vec![art(1, "<a@x>", "")], true)],
            false,
            "t1",
        );
        assert!(!second.incremental);
        assert_eq!(second.groups[0].threads[0].articles.len(), 1);
        assert_eq!(second.groups[0].threads[0].articles[0].message_id, "<a@x>");
    }

    #[test]
    fn naming_one_group_keeps_the_others() {
        let first = assemble_snapshot(
            None,
            &[
                fetch("bots.announce", 1, vec![art(1, "<ann@x>", "")], false),
                fetch("bots.general", 1, vec![art(1, "<gen@x>", "")], false),
            ],
            false,
            "t0",
        );
        let second = assemble_snapshot(
            Some(&first),
            &[fetch(
                "bots.general",
                2,
                vec![art(2, "<reply@x>", "<gen@x>")],
                false,
            )],
            true,
            "t1",
        );
        assert!(second.incremental);
        assert_eq!(
            second
                .groups
                .iter()
                .map(|group| group.name.as_str())
                .collect::<Vec<_>>(),
            vec!["bots.announce", "bots.general"]
        );
        assert_eq!(second.groups[0].threads[0].articles.len(), 1);
        assert_eq!(second.groups[1].threads[0].articles.len(), 2);
        assert_eq!(second.groups[0].high, 1);
    }

    #[test]
    fn a_group_missing_from_the_server_list_is_dropped() {
        let first = assemble_snapshot(
            None,
            &[
                fetch("bots.announce", 1, vec![art(1, "<ann@x>", "")], false),
                fetch("bots.general", 1, vec![art(1, "<gen@x>", "")], false),
            ],
            false,
            "t0",
        );
        let second = assemble_snapshot(
            Some(&first),
            &[fetch("bots.general", 1, vec![], false)],
            false,
            "t1",
        );
        assert_eq!(second.groups.len(), 1);
        assert_eq!(second.groups[0].name, "bots.general");
        assert_eq!(second.groups[0].threads[0].articles.len(), 1);
    }

    #[test]
    fn server_high_and_low_are_not_taken_from_stored_articles() {
        let mut group = fetch("bots.general", 99, vec![art(1, "<a@x>", "")], false);
        group.info.low = 7;
        let snapshot = assemble_snapshot(None, &[group], false, "t0");
        assert_eq!(snapshot.groups[0].high, 99);
        assert_eq!(snapshot.groups[0].low, 7);
        assert_eq!(snapshot.groups[0].threads[0].articles[0].number, 1);
    }

    #[test]
    fn groups_are_sorted_by_name() {
        let snapshot = assemble_snapshot(
            None,
            &[
                fetch("bots.general", 1, vec![art(1, "<g@x>", "")], false),
                fetch("bots.announce", 1, vec![art(1, "<a@x>", "")], false),
            ],
            false,
            "t0",
        );
        assert_eq!(snapshot.groups[0].name, "bots.announce");
        assert_eq!(snapshot.groups[1].name, "bots.general");
    }

    #[test]
    fn plan_uses_the_high_water_mark_and_notices_a_reset() {
        let snapshot = assemble_snapshot(
            None,
            &[fetch(
                "bots.general",
                4,
                vec![art(1, "<a@x>", ""), art(4, "<b@x>", "")],
                false,
            )],
            false,
            "t0",
        );
        assert_eq!(snapshot.high_water("bots.general"), Some(4));
        assert_eq!(snapshot.high_water("bots.other"), None);

        let keep = plan_group(Some(&snapshot), &info("bots.general", 10), false);
        assert_eq!(keep.since, Some(4));
        assert!(!keep.replace);

        let reset = plan_group(Some(&snapshot), &info("bots.general", 2), true);
        assert!(reset.since.is_none());
        assert!(reset.replace);
        let backwards = plan_group(Some(&snapshot), &info("bots.general", 2), false);
        assert!(backwards.since.is_none());
        assert!(backwards.replace);

        let emptied = plan_group(Some(&snapshot), &info("bots.general", 0), false);
        assert_eq!(emptied.since, Some(4));
        assert!(!emptied.replace);

        let first = plan_group(None, &info("bots.general", 10), false);
        assert!(first.since.is_none());
        assert!(!first.replace);
    }

    #[test]
    fn select_groups_filters_and_rejects_unknown_names() {
        let listed = vec![
            GroupEntry {
                name: "bots.announce".to_string(),
                high: 1,
                low: 1,
                posting: "y".to_string(),
                description: "notes".to_string(),
                creator: "ada".to_string(),
            },
            GroupEntry {
                name: "bots.general".to_string(),
                high: 3,
                low: 1,
                posting: "n".to_string(),
                description: String::new(),
                creator: String::new(),
            },
        ];
        let all = select_groups(&listed, &[]).unwrap();
        assert_eq!(all.len(), 2);
        let one = select_groups(
            &listed,
            &["bots.general".to_string(), "bots.general".to_string()],
        )
        .unwrap();
        assert_eq!(one.len(), 1);
        assert_eq!(one[0].name, "bots.general");
        assert_eq!(one[0].posting, "n");
        let err = select_groups(&listed, &["missing.group".to_string()]).unwrap_err();
        assert!(err.to_string().contains("missing.group"));
    }

    #[test]
    fn rfc3339_known_timestamps() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(format_rfc3339(86_400), "1970-01-02T00:00:00Z");
        assert_eq!(format_rfc3339(1_582_934_400), "2020-02-29T00:00:00Z");
        assert_eq!(format_rfc3339(1_700_000_000), "2023-11-14T22:13:20Z");
        assert_eq!(format_rfc3339(1_793_736_000), "2026-11-03T20:00:00Z");
    }

    #[test]
    fn snapshot_file_round_trips_and_refuses_another_version() {
        let snapshot = assemble_snapshot(
            None,
            &[fetch("bots.general", 1, vec![art(1, "<a@x>", "")], false)],
            false,
            "2026-11-03T20:00:00Z",
        );
        let dir = tempfile::tempdir().unwrap();
        assert!(load_snapshot(dir.path()).unwrap().is_none());
        let path = write_snapshot(dir.path(), &snapshot).unwrap();
        assert_eq!(path, dir.path().join("snapshot.json"));
        assert!(!dir.path().join("snapshot.json.tmp").exists());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.ends_with('\n'));
        assert!(!text.ends_with("\n\n"));
        assert_eq!(load_snapshot(dir.path()).unwrap().unwrap(), snapshot);

        fs::write(dir.path().join("snapshot.json"), "{\"version\":2}\n").unwrap();
        let err = load_snapshot(dir.path()).unwrap_err();
        assert!(err.to_string().contains("version 2"));

        fs::write(dir.path().join("snapshot.json"), "not json\n").unwrap();
        assert!(load_snapshot(dir.path()).is_err());
    }

    fn example_snapshot() -> Snapshot {
        let announce = GroupFetch {
            info: GroupInfo {
                name: "bots.announce".to_string(),
                description: "status notes".to_string(),
                high: 1,
                low: 1,
                posting: "y".to_string(),
                creator: "ada".to_string(),
            },
            articles: vec![ThreadArticle {
                number: 1,
                message_id: "<ann-1@slime>".to_string(),
                from: "Ada <ada@bots.local>".to_string(),
                subject: "bridge is up".to_string(),
                date: "Tue, 03 Nov 2026 18:00:00 +0000".to_string(),
                references: String::new(),
                body: "The read-only bridge is up.\n".to_string(),
            }],
            replace: false,
        };
        let general = GroupFetch {
            info: GroupInfo {
                name: "bots.general".to_string(),
                description: "general chat".to_string(),
                high: 5,
                low: 1,
                posting: "y".to_string(),
                creator: "ada".to_string(),
            },
            articles: vec![
                ThreadArticle {
                    number: 1,
                    message_id: "<pic-1@slime>".to_string(),
                    from: "Ada <ada@bots.local>".to_string(),
                    subject: "picnic".to_string(),
                    date: "Tue, 03 Nov 2026 18:05:00 +0000".to_string(),
                    references: String::new(),
                    body: "Saturday at the park.\n".to_string(),
                },
                ThreadArticle {
                    number: 3,
                    message_id: "<pic-3@slime>".to_string(),
                    from: "Bea <bea@bots.local>".to_string(),
                    subject: "Re: picnic".to_string(),
                    date: "Tue, 03 Nov 2026 18:10:00 +0000".to_string(),
                    references: "<pic-1@slime> <pic-2@slime>".to_string(),
                    body: "I'll bring bread.\n".to_string(),
                },
                ThreadArticle {
                    number: 4,
                    message_id: "<pic-4@slime>".to_string(),
                    from: "Ada <ada@bots.local>".to_string(),
                    subject: "Re: picnic".to_string(),
                    date: "Tue, 03 Nov 2026 18:12:00 +0000".to_string(),
                    references: "<pic-1@slime> <pic-2@slime> <pic-3@slime>".to_string(),
                    body: "See you there.\n".to_string(),
                },
                ThreadArticle {
                    number: 5,
                    message_id: "<solo-5@slime>".to_string(),
                    from: "Cal <cal@bots.local>".to_string(),
                    subject: "other topic".to_string(),
                    date: "Tue, 03 Nov 2026 18:20:00 +0000".to_string(),
                    references: "<gone@slime>".to_string(),
                    body: "Starting something new.\n".to_string(),
                },
            ],
            replace: false,
        };
        // Pass general first to prove the file is sorted by group name.
        assemble_snapshot(None, &[general, announce], false, "2026-11-03T20:00:00Z")
    }

    #[test]
    fn example_snapshot_matches_the_committed_file() {
        let snap = example_snapshot();
        assert_forest(&snap.groups[1].threads);
        let picnic = &snap.groups[1].threads[0];
        assert_eq!(picnic.articles[1].message_id, "<pic-3@slime>");
        assert_eq!(
            picnic.articles[1].parent_message_id.as_deref(),
            Some("<pic-1@slime>")
        );
        assert_eq!(picnic.articles[1].depth, 1);
        assert!(
            picnic.articles[1]
                .references
                .contains(&"<pic-2@slime>".to_string())
        );
        assert_eq!(snap.groups[1].threads[1].root_message_id, "<solo-5@slime>");
        assert!(
            snap.groups[1].threads[1].articles[0]
                .parent_message_id
                .is_none()
        );

        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/snapshot.json");
        let raw = fs::read_to_string(&path).unwrap_or_default();
        let expected = format!("{}\n", serde_json::to_string_pretty(&snap).unwrap());
        assert_eq!(raw, expected);
    }
}
