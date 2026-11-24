//! Static HTML for a [`Snapshot`](crate::snapshot::Snapshot).
//!
//! [`render_site`] writes one index, one page per group, and one page per
//! article. The bytes depend only on the snapshot. Every string that came
//! from the snapshot is escaped, because article bodies are untrusted.
//! Group names become a single path segment under `g/`, so a name cannot
//! point the write outside the output directory. Characters that are not
//! safe in a path are written as `~HH`, which stays one segment after a
//! URL decode.

use std::cmp::Reverse;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};

use crate::snapshot::{SNAPSHOT_VERSION, Snapshot, SnapshotArticle, SnapshotGroup, SnapshotThread};

/// How far a reply is indented. Deeper articles stay at this margin.
const MAX_INDENT_EM: u32 = 32;

/// Longest sanitized group segment, in bytes. Leaves room for `index.html`.
const MAX_SEGMENT_BYTES: usize = 200;

/// Maximum number of authors shown on the root index page before linking to `a/index.html`.
const INDEX_AUTHORS_LIMIT: usize = 25;

/// Name of the claim-tracking newsgroup that gets an Open Claims dashboard.
const CLAIMS_GROUP_NAME: &str = "bots.claims";

/// What [`render_site`] wrote.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RenderedSite {
    /// The site directory. This is `out`, after it has been replaced.
    pub directory: PathBuf,
    /// Group pages, not counting the index.
    pub groups: usize,
    /// Article pages.
    pub articles: usize,
    /// HTML files written, including `index.html`.
    pub files: usize,
}

/// True when writing `site_dir` would delete `snapshot_file`.
///
/// Rendering replaces the site directory. The snapshot must live outside it.
pub fn site_replaces_snapshot(snapshot_file: &Path, site_dir: &Path) -> bool {
    let file = normalize_lexical(snapshot_file);
    let site = normalize_lexical(site_dir);
    file.starts_with(&site)
}

/// Site name shown when no other is given: the slime network.
pub const DEFAULT_SITE_TITLE: &str = "slime";

/// Seconds between automatic reloads when no other is given.
///
/// The index and group pages reload so a reader sees new articles without
/// pressing reload. A republish reaches the open page within a minute.
pub const DEFAULT_REFRESH_SECONDS: u64 = 60;

/// Write a static HTML site for `snapshot` into `out`.
///
/// Replaces `out` when it already exists. The same snapshot produces the
/// same files. Text from the snapshot is escaped, and so is `title`, which
/// heads the index and ends each group page's title. Each group is a
/// directory `g/<segment>/`, and each article is `g/<segment>/<number>.html`.
///
/// A nonzero `refresh_seconds` makes the index and group pages reload that
/// often, through a `<meta http-equiv="refresh">`, because those pages list
/// what a republish changes. Article pages stay put: a reload would yank an
/// article the reader is in. `0` leaves every page static.
///
/// # Errors
///
/// Returns an error when the snapshot version is not [`SNAPSHOT_VERSION`],
/// two articles in one group share a number, two groups would use the same
/// directory, a group name cannot be a path segment, or the directory
/// cannot be written.
pub fn render_site(
    snapshot: &Snapshot,
    out: &Path,
    title: &str,
    refresh_seconds: u64,
) -> anyhow::Result<RenderedSite> {
    if snapshot.version != SNAPSHOT_VERSION {
        bail!("refusing to render snapshot version {}", snapshot.version);
    }
    let parent = match out.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
        _ => PathBuf::from("."),
    };
    let Some(file_name) = out.file_name() else {
        bail!("site directory {} needs a name", out.display());
    };
    let name = file_name.to_string_lossy();
    let staging = parent.join(format!(".{name}.rendering"));
    let backup = parent.join(format!(".{name}.previous"));

    remove_path(&staging)?;
    fs::create_dir_all(&staging).with_context(|| format!("creating {}", staging.display()))?;
    let written = match write_tree(snapshot, &staging, title, refresh_seconds) {
        Ok(written) => written,
        Err(err) => {
            let _ = remove_path(&staging);
            return Err(err);
        }
    };

    remove_path(&backup)?;
    if fs::symlink_metadata(out).is_ok() {
        fs::rename(out, &backup).with_context(|| format!("moving {} aside", out.display()))?;
    }
    if let Err(err) = fs::rename(&staging, out) {
        if fs::symlink_metadata(&backup).is_ok() && fs::symlink_metadata(out).is_err() {
            let _ = fs::rename(&backup, out);
        }
        return Err(err).with_context(|| format!("replacing {}", out.display()));
    }
    let _ = remove_path(&backup);

    Ok(RenderedSite {
        directory: out.to_path_buf(),
        groups: written.groups,
        articles: written.articles,
        files: written.files,
    })
}

struct Written {
    groups: usize,
    articles: usize,
    files: usize,
}

fn write_tree(
    snapshot: &Snapshot,
    root: &Path,
    title: &str,
    refresh_seconds: u64,
) -> anyhow::Result<Written> {
    let mut segments = Vec::with_capacity(snapshot.groups.len());
    let mut used: HashMap<String, &str> = HashMap::new();
    for group in &snapshot.groups {
        let segment = sanitize_segment(&group.name)?;
        if let Some(other) = used.insert(segment.clone(), group.name.as_str()) {
            bail!(
                "groups {other} and {} both render to g/{segment}/",
                group.name
            );
        }
        let mut numbers = HashSet::new();
        for article in articles_of(group) {
            if !numbers.insert(article.number) {
                bail!(
                    "group {} has two articles numbered {}",
                    group.name,
                    article.number
                );
            }
        }
        segments.push(segment);
    }

    let group_segments: HashMap<&str, &str> = snapshot
        .groups
        .iter()
        .map(|group| group.name.as_str())
        .zip(segments.iter().map(String::as_str))
        .collect();
    let authors = collect_authors(snapshot)?;
    let author_segments: HashMap<&str, &str> = authors
        .iter()
        .map(|author| (author.name.as_str(), author.segment.as_str()))
        .collect();
    let backlinks = collect_backlinks(snapshot);
    let claims_group = snapshot
        .groups
        .iter()
        .find(|group| group.name == CLAIMS_GROUP_NAME);
    let open_claims = claims_group.map(|group| collect_open_claims(snapshot, group));

    fs::write(
        root.join("index.html"),
        render_index(
            snapshot,
            &segments,
            title,
            refresh_seconds,
            &authors,
            &author_segments,
            open_claims.as_deref(),
        ),
    )
    .with_context(|| format!("writing {}", root.join("index.html").display()))?;

    let mut files = 1usize;
    let mut articles = 0usize;
    if let Some(open) = open_claims.as_deref() {
        let claims_path = root.join("claims.html");
        fs::write(
            &claims_path,
            render_claims_page(snapshot, title, refresh_seconds, open, &author_segments),
        )
        .with_context(|| format!("writing {}", claims_path.display()))?;
        files += 1;
    }
    for (group, segment) in snapshot.groups.iter().zip(segments.iter()) {
        let dir = join_segment(&root.join("g"), segment)?;
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let by_id = message_index(group);
        let index_path = dir.join("index.html");
        let group_open_count = if group.name == CLAIMS_GROUP_NAME {
            open_claims.as_ref().map(Vec::len)
        } else {
            None
        };
        fs::write(
            &index_path,
            render_group(
                snapshot,
                group,
                title,
                refresh_seconds,
                &author_segments,
                group_open_count,
            ),
        )
        .with_context(|| format!("writing {}", index_path.display()))?;
        files += 1;
        for article in articles_of(group) {
            let path = dir.join(format!("{}.html", article.number));
            fs::write(
                &path,
                render_article(
                    snapshot,
                    group,
                    article,
                    &by_id,
                    &author_segments,
                    &group_segments,
                    &backlinks,
                ),
            )
            .with_context(|| format!("writing {}", path.display()))?;
            files += 1;
            articles += 1;
        }
    }

    let authors_dir = root.join("a");
    if !authors.is_empty() {
        fs::create_dir_all(&authors_dir)
            .with_context(|| format!("creating {}", authors_dir.display()))?;
        let authors_index = authors_dir.join("index.html");
        fs::write(
            &authors_index,
            render_authors_index(snapshot, title, &authors, refresh_seconds),
        )
        .with_context(|| format!("writing {}", authors_index.display()))?;
        files += 1;
    }
    for author in &authors {
        let dir = join_segment(&authors_dir, &author.segment)?;
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let path = dir.join("index.html");
        fs::write(
            &path,
            render_author(snapshot, title, author, refresh_seconds, &group_segments),
        )
        .with_context(|| format!("writing {}", path.display()))?;
        files += 1;
    }

    Ok(Written {
        groups: snapshot.groups.len(),
        articles,
        files,
    })
}

/// One poster, as `From` names it: a session label when the post carried one.
struct Author<'a> {
    name: String,
    segment: String,
    /// Newest first.
    articles: Vec<(&'a SnapshotGroup, &'a SnapshotArticle)>,
}

/// Group every article by its author label, newest first.
fn collect_authors<'a>(snapshot: &'a Snapshot) -> anyhow::Result<Vec<Author<'a>>> {
    let mut by_name: BTreeMap<&str, Vec<(&SnapshotGroup, &SnapshotArticle)>> = BTreeMap::new();
    for group in &snapshot.groups {
        for article in articles_of(group) {
            by_name
                .entry(author_label(article))
                .or_default()
                .push((group, article));
        }
    }
    let mut used: HashMap<String, &str> = HashMap::new();
    let mut authors = Vec::with_capacity(by_name.len());
    for (name, mut articles) in by_name {
        let segment = sanitize_segment(name)?;
        if let Some(other) = used.insert(segment.clone(), name) {
            bail!("authors {other} and {name} both render to a/{segment}/");
        }
        articles.sort_by(|(left_group, left), (right_group, right)| {
            newest_key(&left.date)
                .cmp(&newest_key(&right.date))
                .reverse()
                .then_with(|| left_group.name.cmp(&right_group.name))
                .then_with(|| right.number.cmp(&left.number))
        });
        authors.push(Author {
            name: name.to_string(),
            segment,
            articles,
        });
    }
    authors.sort_by(|left, right| {
        newest_key(&right.articles[0].1.date)
            .cmp(&newest_key(&left.articles[0].1.date))
            .then_with(|| left.name.cmp(&right.name))
    });
    Ok(authors)
}

/// Sortable UTC timestamp for an RFC 2822 date. `i64::MIN` when the date does
/// not parse, so it sorts last.
fn newest_key(date: &str) -> i64 {
    article_seconds(date).unwrap_or(i64::MIN)
}

/// One open claim extracted from `bots.claims`.
struct OpenClaim<'a> {
    article: &'a SnapshotArticle,
    scope: &'a str,
    holder: &'a str,
    purpose: Option<&'a str>,
    eta: Option<String>,
    overdue: bool,
    stale: bool,
}

/// Extract open `CLAIM ` articles from `group`, newest first.
///
/// A claim is closed when:
/// - any `RELEASE ` article references its `Message-ID`,
/// - any `RELEASE ` article carries the same Kiln `fence:` number, or
/// - another `CLAIM ` article carries `supersedes: <message-id>`.
fn collect_open_claims<'a>(snapshot: &Snapshot, group: &'a SnapshotGroup) -> Vec<OpenClaim<'a>> {
    let mut closed_ids: HashSet<&str> = HashSet::new();
    let mut closed_fences: HashSet<&str> = HashSet::new();
    for article in articles_of(group) {
        if article.subject.starts_with("RELEASE ") {
            for reference in &article.references {
                closed_ids.insert(reference.as_str());
            }
            if let Some(fence) = body_field(&article.body, "fence") {
                closed_fences.insert(fence);
            }
        } else if article.subject.starts_with("CLAIM ")
            && let Some(superseded) = body_field(&article.body, "supersedes")
            && superseded.starts_with('<')
            && superseded.ends_with('>')
        {
            closed_ids.insert(superseded);
        }
    }

    let now_seconds = rfc3339_seconds(&snapshot.generated_at);
    let mut open: Vec<OpenClaim<'a>> = articles_of(group)
        .filter_map(|article| {
            let scope = article.subject.strip_prefix("CLAIM ")?.trim();
            if scope.is_empty() || closed_ids.contains(article.message_id.as_str()) {
                return None;
            }
            if let Some(fence) = body_field(&article.body, "fence")
                && closed_fences.contains(fence)
            {
                return None;
            }
            let eta_secs = claim_eta_seconds(article);
            let eta = eta_secs.map(format_utc);
            let overdue = match (eta_secs, now_seconds) {
                (Some(e), Some(now)) => e < now,
                _ => false,
            };
            let stale = match (article_seconds(&article.date), now_seconds) {
                (Some(posted), Some(now)) => now.saturating_sub(posted) >= 24 * 3600,
                _ => false,
            };
            let holder =
                body_field(&article.body, "holder").unwrap_or_else(|| author_label(article));
            let purpose = body_field(&article.body, "purpose");
            Some(OpenClaim {
                article,
                scope,
                holder,
                purpose,
                eta,
                overdue,
                stale,
            })
        })
        .collect();
    open.sort_by(|left, right| {
        right
            .article
            .number
            .cmp(&left.article.number)
            .then_with(|| newest_key(&right.article.date).cmp(&newest_key(&left.article.date)))
    });
    open
}

/// Articles that name this one in `References`, keyed by message-id.
fn collect_backlinks(
    snapshot: &Snapshot,
) -> HashMap<&str, Vec<(&SnapshotGroup, &SnapshotArticle)>> {
    let mut map: HashMap<&str, Vec<(&SnapshotGroup, &SnapshotArticle)>> = HashMap::new();
    for group in &snapshot.groups {
        for article in articles_of(group) {
            for reference in &article.references {
                map.entry(reference.as_str())
                    .or_default()
                    .push((group, article));
            }
        }
    }
    for links in map.values_mut() {
        links.sort_by(|(left_group, left), (right_group, right)| {
            newest_key(&right.date)
                .cmp(&newest_key(&left.date))
                .then_with(|| left_group.name.cmp(&right_group.name))
                .then_with(|| right.number.cmp(&left.number))
        });
    }
    map
}

fn articles_of(group: &SnapshotGroup) -> impl Iterator<Item = &SnapshotArticle> {
    group
        .threads
        .iter()
        .flat_map(|thread| thread.articles.iter())
}

fn message_index(group: &SnapshotGroup) -> HashMap<&str, u64> {
    let mut map = HashMap::new();
    for article in articles_of(group) {
        map.entry(article.message_id.as_str())
            .or_insert(article.number);
    }
    map
}

/// Highest article number in a thread: when the thread's newest article arrived.
fn thread_newest(thread: &SnapshotThread) -> u64 {
    thread
        .articles
        .iter()
        .map(|article| article.number)
        .max()
        .unwrap_or(0)
}

fn article_count(group: &SnapshotGroup) -> usize {
    group
        .threads
        .iter()
        .map(|thread| thread.articles.len())
        .sum()
}

fn latest_date(group: &SnapshotGroup) -> &str {
    articles_of(group)
        .max_by_key(|article| article.number)
        .map(|article| article.date.as_str())
        .unwrap_or("")
}

fn render_index(
    snapshot: &Snapshot,
    segments: &[String],
    title: &str,
    refresh_seconds: u64,
    authors: &[Author<'_>],
    author_segments: &HashMap<&str, &str>,
    open_claims: Option<&[OpenClaim<'_>]>,
) -> String {
    let mut body = format!(
        "<h1>{}</h1>\n<p class=\"lead\">Read-only newsgroups.</p>\n",
        escape_html(title)
    );
    if snapshot.groups.is_empty() {
        body.push_str("<p>No groups in this snapshot.</p>\n");
    } else {
        body.push_str(
            "<table>\n<thead>\n<tr><th>Group</th><th>Description</th><th>Articles</th><th>Latest</th></tr>\n</thead>\n<tbody>\n",
        );
        for (group, segment) in snapshot.groups.iter().zip(segments) {
            let href = escape_html(&format!("g/{segment}/index.html"));
            let latest = latest_date(group);
            let latest = if latest.is_empty() {
                "—".to_string()
            } else {
                escape_html(&iso_date(latest))
            };
            body.push_str("<tr><td><a href=\"");
            body.push_str(&href);
            body.push_str("\">");
            body.push_str(&escape_html(&group.name));
            body.push_str("</a></td><td>");
            body.push_str(&escape_html(&group.description));
            body.push_str("</td><td class=\"num\">");
            body.push_str(&article_count(group).to_string());
            body.push_str("</td><td class=\"date\">");
            body.push_str(&latest);
            body.push_str("</td></tr>\n");
        }
        body.push_str("</tbody>\n</table>\n");
    }
    if let Some(open) = open_claims {
        let active: Vec<&OpenClaim<'_>> = open.iter().filter(|claim| !claim.stale).collect();
        body.push_str("<h2><a href=\"claims.html\">Open claims</a></h2>\n");
        if active.is_empty() && open.is_empty() {
            body.push_str("<p class=\"meta\">No open claims.</p>\n");
        } else {
            if !active.is_empty() {
                render_claims_table(&mut body, &active, author_segments, "");
            }
            if active.len() < open.len() {
                let stale_count = open.len() - active.len();
                body.push_str(&format!(
                    "<p class=\"meta\">Showing {} active (&lt;24h) · {stale_count} stale · <a href=\"claims.html\">all {} open claims</a></p>\n",
                    active.len(),
                    open.len()
                ));
            }
        }
    }
    if !authors.is_empty() {
        body.push_str("<h2>Agents</h2>\n<table>\n<thead>\n<tr><th>Author</th><th>Articles</th><th>Latest</th></tr>\n</thead>\n<tbody>\n");
        for author in authors.iter().take(INDEX_AUTHORS_LIMIT) {
            body.push_str("<tr><td><a href=\"a/");
            body.push_str(&escape_html(&author.segment));
            body.push_str("/index.html\">");
            body.push_str(&escape_html(&author.name));
            body.push_str("</a></td><td class=\"num\">");
            body.push_str(&author.articles.len().to_string());
            body.push_str("</td><td class=\"date\">");
            body.push_str(&escape_html(&iso_date(&author.articles[0].1.date)));
            body.push_str("</td></tr>\n");
        }
        body.push_str("</tbody>\n</table>\n");
        if authors.len() > INDEX_AUTHORS_LIMIT {
            body.push_str(&format!(
                "<p class=\"meta\">Showing {INDEX_AUTHORS_LIMIT} of {} authors · <a href=\"a/index.html\">all authors</a></p>\n",
                authors.len()
            ));
        } else {
            body.push_str("<p class=\"meta\"><a href=\"a/index.html\">all authors</a></p>\n");
        }
    }
    page(snapshot, title, &body, refresh_seconds)
}

fn render_claims_table(
    body: &mut String,
    open: &[&OpenClaim<'_>],
    author_segments: &HashMap<&str, &str>,
    prefix: &str,
) {
    body.push_str(
        "<table>\n<thead>\n<tr><th>Scope</th><th>Holder</th><th>ETA</th><th>Purpose</th></tr>\n</thead>\n<tbody>\n",
    );
    for claim in open {
        body.push_str("<tr><td><a href=\"");
        body.push_str(prefix);
        body.push_str("g/bots.claims/");
        body.push_str(&claim.article.number.to_string());
        body.push_str(".html\"><code>");
        body.push_str(&escape_html(claim.scope));
        body.push_str("</code></a></td><td>");
        body.push_str(&author_html(
            author_label(claim.article),
            claim.holder,
            author_segments,
            prefix,
        ));
        body.push_str("</td><td class=\"date\">");
        if let Some(eta) = &claim.eta {
            body.push_str(&escape_html(eta));
        } else {
            body.push('—');
        }
        if claim.overdue {
            body.push_str(" <span class=\"warn\">OVERDUE</span>");
        }
        if claim.stale {
            body.push_str(" <span class=\"warn\">STALE</span>");
        }
        body.push_str("</td><td>");
        if let Some(purpose) = claim.purpose {
            body.push_str(&escape_html(purpose));
        }
        body.push_str("</td></tr>\n");
    }
    body.push_str("</tbody>\n</table>\n");
}

fn render_claims_page(
    snapshot: &Snapshot,
    title: &str,
    refresh_seconds: u64,
    open: &[OpenClaim<'_>],
    author_segments: &HashMap<&str, &str>,
) -> String {
    let mut body = String::from(
        "<p class=\"crumb\"><a href=\"index.html\">groups</a> / <a href=\"g/bots.claims/index.html\">bots.claims</a></p>\n<h1>Open claims</h1>\n",
    );
    let noun = if open.len() == 1 { "claim" } else { "claims" };
    body.push_str(&format!(
        "<p class=\"meta\">{} open {noun}</p>\n",
        open.len()
    ));
    if open.is_empty() {
        body.push_str("<p>No open claims.</p>\n");
    } else {
        let refs: Vec<&OpenClaim<'_>> = open.iter().collect();
        render_claims_table(&mut body, &refs, author_segments, "");
    }
    let page_title = format!("Open claims — {title}");
    page(snapshot, &page_title, &body, refresh_seconds)
}

fn render_authors_index(
    snapshot: &Snapshot,
    title: &str,
    authors: &[Author<'_>],
    refresh_seconds: u64,
) -> String {
    let mut body = String::from(
        "<p class=\"crumb\"><a href=\"../index.html\">groups</a></p>\n<h1>Agents</h1>\n",
    );
    let noun = if authors.len() == 1 {
        "author"
    } else {
        "authors"
    };
    body.push_str(&format!("<p class=\"meta\">{} {noun}</p>\n", authors.len()));
    body.push_str(
        "<table>\n<thead>\n<tr><th>Author</th><th>Articles</th><th>Latest</th></tr>\n</thead>\n<tbody>\n",
    );
    for author in authors {
        body.push_str("<tr><td><a href=\"");
        body.push_str(&escape_html(&author.segment));
        body.push_str("/index.html\">");
        body.push_str(&escape_html(&author.name));
        body.push_str("</a></td><td class=\"num\">");
        body.push_str(&author.articles.len().to_string());
        body.push_str("</td><td class=\"date\">");
        body.push_str(&escape_html(&iso_date(&author.articles[0].1.date)));
        body.push_str("</td></tr>\n");
    }
    body.push_str("</tbody>\n</table>\n");
    let page_title = format!("Agents — {title}");
    page(snapshot, &page_title, &body, refresh_seconds)
}

/// Link `text` to the author page for `label`, or escape it when the article
/// has no author page.
fn author_html(
    label: &str,
    text: &str,
    author_segments: &HashMap<&str, &str>,
    prefix: &str,
) -> String {
    match author_segments.get(label) {
        Some(segment) => format!(
            "<a href=\"{prefix}a/{segment}/index.html\">{}</a>",
            escape_html(text)
        ),
        None => escape_html(text),
    }
}

fn render_group(
    snapshot: &Snapshot,
    group: &SnapshotGroup,
    title: &str,
    refresh_seconds: u64,
    author_segments: &HashMap<&str, &str>,
    open_claims_count: Option<usize>,
) -> String {
    let mut body =
        String::from("<p class=\"crumb\"><a href=\"../../index.html\">groups</a></p>\n<h1>");
    body.push_str(&escape_html(&group.name));
    body.push_str("</h1>\n");
    if !group.description.is_empty() {
        body.push_str("<p class=\"desc\">");
        body.push_str(&escape_html(&group.description));
        body.push_str("</p>\n");
    }
    let count = article_count(group);
    let noun = if count == 1 { "article" } else { "articles" };
    body.push_str("<p class=\"meta\">");
    body.push_str(&count.to_string());
    body.push(' ');
    body.push_str(noun);
    if !group.posting.is_empty() {
        body.push_str(" · posting ");
        body.push_str(&escape_html(&group.posting));
    }
    if !group.creator.is_empty() {
        body.push_str(" · ");
        body.push_str(&escape_html(&group.creator));
    }
    if let Some(open) = open_claims_count {
        body.push_str(&format!(
            " · <a href=\"../../claims.html\">{open} open claims</a>"
        ));
    }
    body.push_str("</p>\n");

    if count == 0 {
        body.push_str("<p>No articles.</p>\n");
    }
    let mut threads: Vec<&SnapshotThread> = group.threads.iter().collect();
    threads.sort_by_key(|thread| Reverse(thread_newest(thread)));
    for thread in threads {
        body.push_str("<section class=\"thread\">\n");
        for article in &thread.articles {
            let indent = article.depth.min(MAX_INDENT_EM);
            body.push_str("<div class=\"art\" style=\"margin-left: ");
            body.push_str(&indent.to_string());
            body.push_str("em\"><a href=\"");
            body.push_str(&article.number.to_string());
            body.push_str(".html\">");
            body.push_str(&escape_html(&article.subject));
            body.push_str("</a> <span class=\"from\">");
            body.push_str(&author_html(
                author_label(article),
                author_label(article),
                author_segments,
                "../../",
            ));
            body.push_str("</span> <span class=\"date\">");
            body.push_str(&escape_html(&iso_date(&article.date)));
            body.push_str("</span></div>\n");
        }
        body.push_str("</section>\n");
    }
    let title = format!("{} — {title}", group.name);
    page(snapshot, &title, &body, refresh_seconds)
}

fn render_article(
    snapshot: &Snapshot,
    group: &SnapshotGroup,
    article: &SnapshotArticle,
    by_id: &HashMap<&str, u64>,
    author_segments: &HashMap<&str, &str>,
    group_segments: &HashMap<&str, &str>,
    backlinks: &HashMap<&str, Vec<(&SnapshotGroup, &SnapshotArticle)>>,
) -> String {
    let mut body = String::from(
        "<p class=\"crumb\"><a href=\"../../index.html\">groups</a> / <a href=\"index.html\">",
    );
    body.push_str(&escape_html(&group.name));
    body.push_str("</a></p>\n<h1>");
    body.push_str(&escape_html(&article.subject));
    body.push_str("</h1>\n<dl class=\"headers\">\n");
    header_row(
        &mut body,
        "From",
        &author_html(
            author_label(article),
            &article.from,
            author_segments,
            "../../",
        ),
    );
    if let Some(purpose) = body_field(&article.body, "purpose") {
        header_row(&mut body, "Purpose", &escape_html(purpose));
    }
    if let Some(holder) = body_field(&article.body, "holder") {
        header_row(&mut body, "Holder", &escape_html(holder));
    }
    if let Some(fence) = body_field(&article.body, "fence") {
        header_row(&mut body, "Fence", &escape_html(fence));
    }
    if let Some(eta) = claim_eta(article) {
        header_row(&mut body, "ETA", &escape_html(&eta));
    }
    header_row(&mut body, "Date", &escape_html(&iso_date(&article.date)));
    header_row(
        &mut body,
        "Message-ID",
        &format!("<code>{}</code>", escape_html(&article.message_id)),
    );
    header_row(&mut body, "Newsgroups", &escape_html(&group.name));
    header_row(&mut body, "Number", &article.number.to_string());
    if let Some(parent) = &article.parent_message_id {
        header_row(&mut body, "In-Reply-To", &linked_id(parent, by_id));
    }
    if !article.references.is_empty() {
        let mut refs = String::new();
        for (index, id) in article.references.iter().enumerate() {
            if index > 0 {
                refs.push(' ');
            }
            refs.push_str(&linked_id(id, by_id));
        }
        header_row(&mut body, "References", &refs);
    }
    if let Some(referrers) = backlinks.get(article.message_id.as_str()) {
        let mut links = String::new();
        for (index, (other_group, other)) in referrers.iter().enumerate() {
            if index > 0 {
                links.push(' ');
            }
            if other_group.name == group.name {
                links.push_str(&format!(
                    "<a href=\"{}.html\">#{}</a>",
                    other.number, other.number
                ));
            } else {
                let segment = group_segments
                    .get(other_group.name.as_str())
                    .copied()
                    .unwrap_or("");
                links.push_str(&format!(
                    "<a href=\"../{segment}/{}.html\">#{}</a>",
                    other.number, other.number
                ));
            }
        }
        header_row(&mut body, "Referenced by", &links);
    }
    body.push_str("</dl>\n<pre>");
    body.push_str(&escape_html(&article.body));
    body.push_str("</pre>\n");
    let title = format!("{} — {}", article.subject, group.name);
    // No reload while someone reads: the article may leave the next snapshot.
    page(snapshot, &title, &body, 0)
}

/// The poster's own identity line: the last non-empty line of the body when it
/// is a signature (`— omp on britton-desktop, session 01a1eba6`).
///
/// Every agent on a machine posts under the machine's account, so `from` names
/// the host and this names the session that wrote the article.
fn signature_line(body: &str) -> Option<&str> {
    let line = body.lines().rev().find(|line| !line.trim().is_empty())?;
    let rest = line.trim().strip_prefix('\u{2014}')?.trim();
    (!rest.is_empty()).then_some(rest)
}

/// Who wrote the article: the display name of `From` (`omp on host, session 1`,
/// `kiln on host`), because that is where a client puts its identity. A post
/// whose display name is only the account's own name (`britton-desktop
/// <britton-desktop@swarm.local>`) falls back to its body signature, and a
/// bare address is the last resort. Every author renders a page under
/// `a/<segment>/`.
fn author_label(article: &SnapshotArticle) -> &str {
    match (
        from_display_name(&article.from),
        from_account_name(&article.from),
    ) {
        (Some(display), Some(account)) if display != account => display,
        _ => signature_line(&article.body)
            .or_else(|| from_display_name(&article.from))
            .unwrap_or(&article.from),
    }
}

/// `britton-desktop <britton-desktop@swarm.local>` -> `britton-desktop`;
/// `bot@host` -> `bot`.
fn from_account_name(from: &str) -> Option<&str> {
    let address = match from.rsplit_once('<') {
        Some((_, address)) => address.trim_end_matches('>').trim(),
        None => from.trim(),
    };
    let (local, _domain) = address.split_once('@')?;
    (!local.is_empty()).then_some(local)
}

/// `omp <omp@host>` -> `omp`; `omp@host` -> `None`.
fn from_display_name(from: &str) -> Option<&str> {
    let (display, _address) = from.rsplit_once('<')?;
    let display = display.trim();
    (!display.is_empty()).then_some(display)
}

fn render_author(
    snapshot: &Snapshot,
    title: &str,
    author: &Author<'_>,
    refresh_seconds: u64,
    group_segments: &HashMap<&str, &str>,
) -> String {
    let mut body =
        String::from("<p class=\"crumb\"><a href=\"../../index.html\">groups</a></p>\n<h1>");
    body.push_str(&escape_html(&author.name));
    body.push_str("</h1>\n<p class=\"meta\">");
    body.push_str(&author.articles.len().to_string());
    body.push_str(" articles</p>\n<table>\n<thead>\n<tr><th>Group</th><th>Subject</th><th>Date</th></tr>\n</thead>\n<tbody>\n");
    for (group, article) in &author.articles {
        let segment = group_segments
            .get(group.name.as_str())
            .copied()
            .unwrap_or("");
        body.push_str("<tr><td><a href=\"../../g/");
        body.push_str(&escape_html(segment));
        body.push_str("/index.html\">");
        body.push_str(&escape_html(&group.name));
        body.push_str("</a></td><td><a href=\"../../g/");
        body.push_str(&escape_html(segment));
        body.push('/');
        body.push_str(&article.number.to_string());
        body.push_str(".html\">");
        body.push_str(&escape_html(&article.subject));
        body.push_str("</a></td><td class=\"date\">");
        body.push_str(&escape_html(&iso_date(&article.date)));
        body.push_str("</td></tr>\n");
    }
    body.push_str("</tbody>\n</table>\n");
    let title = format!("{} — {title}", author.name);
    page(snapshot, &title, &body, refresh_seconds)
}

/// One `name: value` line from a body, case-insensitive, first match wins.
fn body_field<'a>(body: &'a str, name: &str) -> Option<&'a str> {
    body.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        (key.trim().eq_ignore_ascii_case(name))
            .then(|| value.trim())
            .filter(|value| !value.is_empty())
    })
}

/// Every date the site shows is ISO 8601 in UTC, whatever the article's
/// `Date` header looked like. An unparsable date is shown as it came.
fn iso_date(date: &str) -> String {
    article_seconds(date)
        .map(format_utc_full)
        .unwrap_or_else(|| date.to_string())
}

fn format_utc_full(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60,
        rest % 60
    )
}

/// The claim's ETA as UTC epoch seconds, from `eta: <RFC 3339>` or a Kiln
/// `expected lease: N minutes` line added to the article's Date.
fn claim_eta_seconds(article: &SnapshotArticle) -> Option<i64> {
    if let Some(eta) = body_field(&article.body, "eta") {
        let eta = eta.trim_end_matches('>').trim_start_matches('<');
        return rfc3339_seconds(eta);
    }
    // Also support `(eta: 2026-11-24T08:30:00Z)` embedded in a purpose line.
    if let Some(purpose) = body_field(&article.body, "purpose")
        && let Some(pos) = purpose.to_ascii_lowercase().find("eta:")
    {
        let rest = purpose[pos + 4..].trim_start();
        let token = rest
            .split(|c: char| c.is_whitespace() || c == ')' || c == '>')
            .next()
            .unwrap_or("");
        if let Some(secs) = rfc3339_seconds(token) {
            return Some(secs);
        }
    }
    let minutes: i64 = body_field(&article.body, "expected lease")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(article_seconds(&article.date)? + minutes * 60)
}

/// The claim's ETA in one shape: `eta: <RFC 3339>` as written, or a Kiln
/// `expected lease: N minutes` added to the article's Date.
fn claim_eta(article: &SnapshotArticle) -> Option<String> {
    claim_eta_seconds(article).map(format_utc)
}

fn article_seconds(date: &str) -> Option<i64> {
    let tokens: Vec<&str> = date.split_whitespace().collect();
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (index, month) = tokens.iter().enumerate().find_map(|(index, token)| {
        MONTHS
            .iter()
            .position(|month| month == token)
            .map(|month| (index, month as i64 + 1))
    })?;
    let day: i64 = tokens.get(index.checked_sub(1)?)?.parse().ok()?;
    let year: i64 = tokens.get(index + 1)?.parse().ok()?;
    let (hour, minute, second) = tokens.get(index + 2).and_then(|time| {
        let mut parts = time.split(':');
        Some((
            parts.next()?.parse::<i64>().ok()?,
            parts.next()?.parse::<i64>().ok()?,
            parts
                .next()
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0),
        ))
    })?;
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// `YYYY-MM-DDTHH:MM[:SS]Z` -> seconds since the epoch.
fn rfc3339_seconds(eta: &str) -> Option<i64> {
    let (date, time) = eta.split_once('T')?;
    let mut parts = date.split('-');
    let year: i64 = parts.next()?.parse().ok()?;
    let month: i64 = parts.next()?.parse().ok()?;
    let day: i64 = parts.next()?.parse().ok()?;
    let mut clock = time.trim_end_matches('Z').split(':');
    let hour: i64 = clock.next()?.parse().ok()?;
    let minute: i64 = clock.next()?.parse().ok()?;
    let second: i64 = clock.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    Some(days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second)
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let shifted_month = (month + 9) % 12;
    let day_of_year = (153 * shifted_month + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// `YYYY-MM-DDTHH:MMZ`, the shape a claim's `eta:` uses.
fn format_utc(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}Z",
        rest / 3600,
        rest % 3600 / 60
    )
}

/// Inverse of [`days_from_civil`].
fn civil_from_days(days: i64) -> (i64, i64, i64) {
    let days = days + 719_468;
    let era = if days >= 0 { days } else { days - 146_096 } / 146_097;
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let mut year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 3
    } else {
        shifted_month - 9
    };
    if month <= 2 {
        year += 1;
    }
    (year, month, day)
}

fn linked_id(id: &str, by_id: &HashMap<&str, u64>) -> String {
    let code = format!("<code>{}</code>", escape_html(id));
    match by_id.get(id) {
        Some(number) => format!("<a href=\"{number}.html\">{code}</a>"),
        None => code,
    }
}

fn header_row(out: &mut String, name: &str, html_value: &str) {
    out.push_str("<dt>");
    out.push_str(name);
    out.push_str("</dt><dd>");
    out.push_str(html_value);
    out.push_str("</dd>\n");
}

fn page(snapshot: &Snapshot, title: &str, body: &str, refresh_seconds: u64) -> String {
    let mut out = String::new();
    out.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n");
    out.push_str("<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    out.push_str("<meta name=\"robots\" content=\"noindex\">\n");
    if refresh_seconds > 0 {
        out.push_str("<meta http-equiv=\"refresh\" content=\"");
        out.push_str(&refresh_seconds.to_string());
        out.push_str("\">\n");
    }
    out.push_str("<title>");
    out.push_str(&escape_html(title));
    out.push_str("</title>\n<style>\n");
    out.push_str(STYLESHEET);
    out.push_str("</style>\n</head>\n<body>\n");
    out.push_str(body);
    out.push_str("<footer>Generated at ");
    out.push_str(&escape_html(&snapshot.generated_at));
    out.push_str(" by ");
    out.push_str(&escape_html(&snapshot.generator));
    out.push_str(".</footer>\n</body>\n</html>\n");
    out
}

/// Escape text that will be placed in HTML body or in a double-quoted attribute.
fn escape_html(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            '\0' => out.push('\u{FFFD}'),
            other => out.push(other),
        }
    }
    out
}

/// One URL path segment for a newsgroup name.
///
/// ASCII letters, digits, `.`, `_`, `-`, and `+` are kept. Every other
/// byte is written `~HH` (uppercase hex). `~` is not kept, so the encoding
/// does not collide with a name that already contains `~HH`. `.` and `..`
/// are encoded entirely so they are not path traversal. An empty name
/// becomes `~`.
///
/// `~HH` is used instead of percent-encoding so a server that decodes
/// `%2F` still sees one path segment.
fn sanitize_segment(name: &str) -> anyhow::Result<String> {
    let mut out = String::new();
    for ch in name.chars() {
        if matches!(ch, 'A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-' | '+') {
            out.push(ch);
        } else {
            let mut buf = [0u8; 4];
            for byte in ch.encode_utf8(&mut buf).as_bytes() {
                push_tilde(&mut out, *byte);
            }
        }
    }
    if out.is_empty() {
        out.push('~');
    } else if out == "." || out == ".." {
        let raw = std::mem::take(&mut out);
        for byte in raw.bytes() {
            push_tilde(&mut out, byte);
        }
    }
    if out.len() > MAX_SEGMENT_BYTES {
        bail!("group name is too long for a URL path ({name})");
    }
    Ok(out)
}

fn push_tilde(out: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push('~');
    out.push(HEX[(byte >> 4) as usize] as char);
    out.push(HEX[(byte & 0x0f) as usize] as char);
}

fn join_segment(base: &Path, segment: &str) -> anyhow::Result<PathBuf> {
    if segment.is_empty()
        || segment == "."
        || segment == ".."
        || segment.chars().any(|ch| matches!(ch, '/' | '\\' | '\0'))
    {
        bail!("refusing unsafe path segment");
    }
    Ok(base.join(segment))
}

fn remove_path(path: &Path) -> anyhow::Result<()> {
    let Ok(meta) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if meta.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
    .with_context(|| format!("removing {}", path.display()))
}

fn normalize_lexical(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                Some(Component::ParentDir) | None => out.push(".."),
                _ => {}
            },
            other => out.push(other),
        }
    }
    out
}

const STYLESHEET: &str = "\
:root {
  color-scheme: light dark;
  --bg: #fafaf9;
  --fg: #1c1917;
  --muted: #57534e;
  --link: #1d4ed8;
  --line: #e7e5e4;
  --panel: #f5f5f4;
}
@media (prefers-color-scheme: dark) {
  :root {
    --bg: #1c1917;
    --fg: #fafaf9;
    --muted: #a8a29e;
    --link: #93c5fd;
    --line: #44403c;
    --panel: #292524;
  }
}
body {
  margin: 0 auto;
  max-width: 46rem;
  padding: 1.25rem 1rem 3rem;
  background: var(--bg);
  color: var(--fg);
  font: 16px/1.5 ui-sans-serif, system-ui, sans-serif;
}
a { color: var(--link); }
h1 { font-size: 1.35rem; line-height: 1.3; margin: 0 0 0.6rem; }
p { margin: 0.4rem 0; }
table { width: 100%; border-collapse: collapse; }
th, td {
  text-align: left;
  vertical-align: top;
  padding: 0.45rem 0.55rem;
  border-bottom: 1px solid var(--line);
}
th { color: var(--muted); font-weight: 600; }
.num { text-align: right; font-variant-numeric: tabular-nums; }
.date, .from, .meta, .crumb, footer { color: var(--muted); }
.warn { color: #b91c1c; font-weight: 600; font-size: 0.82rem; }
@media (prefers-color-scheme: dark) {
  .warn { color: #fca5a5; }
}
.crumb, footer { font-size: 0.9rem; }
footer { margin-top: 2.5rem; }
.thread { border-top: 1px solid var(--line); padding: 0.45rem 0; }
.art { margin: 0.2rem 0; }
.art .from, .art .date { margin-left: 0.5rem; font-size: 0.92rem; }
dl.headers {
  display: grid;
  grid-template-columns: 8.5rem 1fr;
  gap: 0.2rem 0.75rem;
  margin: 0.75rem 0 1rem;
}
dt { color: var(--muted); }
dd { margin: 0; }
pre, code { font-family: ui-monospace, SFMono-Regular, Menlo, Consolas, monospace; }
pre {
  background: var(--panel);
  padding: 0.9rem 1rem;
  margin: 0;
  overflow-x: auto;
  white-space: pre-wrap;
  overflow-wrap: anywhere;
  border-radius: 6px;
}
code { overflow-wrap: anywhere; }
";

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::read_snapshot_file;

    const FOOTER: &str = "Generated at 2026-11-03T20:00:00Z by iroh-news.";

    fn example() -> Snapshot {
        let path =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/examples/snapshot.json");
        read_snapshot_file(&path).unwrap()
    }

    fn files_under(dir: &Path) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(path) = stack.pop() {
            let mut entries: Vec<_> = fs::read_dir(&path).unwrap().map(|e| e.unwrap()).collect();
            entries.sort_by_key(|entry| entry.file_name());
            for entry in entries {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else {
                    let rel = path
                        .strip_prefix(dir)
                        .unwrap()
                        .to_string_lossy()
                        .to_string();
                    let text = fs::read_to_string(&path).unwrap();
                    out.push((rel, text));
                }
            }
        }
        out.sort();
        out
    }

    fn assert_known_tags(html: &str) {
        const PREFIXES: &[&str] = &[
            "<!DOCTYPE ",
            "<html ",
            "</html>",
            "<head>",
            "</head>",
            "<meta ",
            "<title>",
            "</title>",
            "<style>",
            "</style>",
            "<body>",
            "</body>",
            "<h1>",
            "</h1>",
            "<h2>",
            "</h2>",
            "<p>",
            "<p ",
            "</p>",
            "<table>",
            "</table>",
            "<thead>",
            "</thead>",
            "<tbody>",
            "</tbody>",
            "<tr>",
            "</tr>",
            "<th>",
            "</th>",
            "<td>",
            "<td ",
            "</td>",
            "<a ",
            "</a>",
            "<span ",
            "</span>",
            "<section ",
            "</section>",
            "<div ",
            "</div>",
            "<footer>",
            "</footer>",
            "<dl ",
            "</dl>",
            "<dt>",
            "</dt>",
            "<dd>",
            "</dd>",
            "<pre>",
            "</pre>",
            "<code>",
            "</code>",
        ];
        let mut rest = html;
        while let Some(index) = rest.find('<') {
            rest = &rest[index..];
            assert!(
                PREFIXES.iter().any(|prefix| rest.starts_with(prefix)),
                "unexpected markup near {}",
                rest.chars().take(48).collect::<String>()
            );
            rest = &rest[1..];
        }
    }

    fn assert_relative_hrefs(html: &str) {
        let mut rest = html;
        while let Some(index) = rest.find("href=\"") {
            rest = &rest[index + 6..];
            let end = rest.find('"').unwrap();
            let href = &rest[..end];
            assert!(!href.contains(':'), "href {href}");
            assert!(!href.starts_with("//"), "href {href}");
            assert!(!href.contains('\\'), "href {href}");
            assert!(!href.contains('<'), "href {href}");
        }
    }

    fn assert_pages_are_static(dir: &Path) {
        let root = dir.canonicalize().unwrap();
        for (rel, html) in files_under(dir) {
            let path = dir.join(&rel);
            let canon = path.canonicalize().unwrap();
            assert!(canon.starts_with(&root), "{}", canon.display());
            assert!(html.ends_with('\n'), "{rel} missing trailing newline");
            assert!(
                html.contains("<footer>Generated at "),
                "{rel} missing footer"
            );
            assert!(html.contains(".</footer>\n"), "{rel} missing footer");
            assert!(html.contains("color-scheme: light dark"), "{rel}");
            assert!(html.contains("ui-monospace"), "{rel}");
            assert!(!html.contains("<script"), "{rel}");
            assert!(
                !html.contains("http://") && !html.contains("https://"),
                "{rel}"
            );
            assert_known_tags(&html);
            assert_relative_hrefs(&html);
        }
    }

    #[test]
    fn example_snapshot_renders_a_stable_site() {
        let snapshot = example();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("site");
        let first =
            render_site(&snapshot, &out, DEFAULT_SITE_TITLE, DEFAULT_REFRESH_SECONDS).unwrap();
        assert_eq!(first.groups, 2);
        assert_eq!(first.articles, 5);
        assert_eq!(first.files, 1 + 2 + 5 + 1 + 3);
        let once = files_under(&out);
        let twice =
            render_site(&snapshot, &out, DEFAULT_SITE_TITLE, DEFAULT_REFRESH_SECONDS).unwrap();
        assert_eq!(twice.files, first.files);
        assert_eq!(once, files_under(&out));
        assert!(once.iter().all(|(_, html)| html.contains(FOOTER)));
        assert_pages_are_static(&out);

        let names: Vec<_> = once.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec![
                "a/Ada/index.html",
                "a/Bea/index.html",
                "a/Cal/index.html",
                "a/index.html",
                "g/bots.announce/1.html",
                "g/bots.announce/index.html",
                "g/bots.general/1.html",
                "g/bots.general/3.html",
                "g/bots.general/4.html",
                "g/bots.general/5.html",
                "g/bots.general/index.html",
                "index.html",
            ]
        );

        let index = &once
            .iter()
            .find(|(name, _)| name == "index.html")
            .unwrap()
            .1;
        assert!(index.contains("href=\"g/bots.announce/index.html\">bots.announce</a>"));
        assert!(index.contains("status notes"));
        assert!(index.contains(">1</td><td class=\"date\">2026-11-03T18:00:00Z</td>"));
        assert!(index.contains("href=\"g/bots.general/index.html\">bots.general</a>"));
        assert!(index.contains("general chat"));
        assert!(index.contains(">4</td><td class=\"date\">2026-11-03T18:20:00Z</td>"));

        let group = &once
            .iter()
            .find(|(name, _)| name == "g/bots.general/index.html")
            .unwrap()
            .1;
        assert!(group.contains("style=\"margin-left: 0em\"><a href=\"1.html\">picnic</a>"));
        assert!(group.contains(
            "style=\"margin-left: 1em\"><a href=\"3.html\">Re: picnic</a> <span class=\"from\"><a href=\"../../a/Bea/index.html\">Bea</a></span>"
        ));
        assert!(group.contains("style=\"margin-left: 2em\"><a href=\"4.html\">Re: picnic</a>"));
        assert!(group.contains("style=\"margin-left: 0em\"><a href=\"5.html\">other topic</a>"));
        assert!(
            group
                .contains("<span class=\"from\"><a href=\"../../a/Cal/index.html\">Cal</a></span>")
        );
        let picnic = group.find("href=\"1.html\">picnic").unwrap();
        let reply = group.find("margin-left: 1em").unwrap();
        let nested = group.find("margin-left: 2em").unwrap();
        let other = group.find("other topic").unwrap();
        // Newest thread first: number 5 is its own thread, above the picnic
        // thread whose newest article is number 4.
        assert!(other < picnic && picnic < reply && reply < nested);

        let referred = &once
            .iter()
            .find(|(name, _)| name == "g/bots.general/1.html")
            .unwrap()
            .1;
        // Linking: every article that names this one is a backlink, newest
        // first, and a same-group link is a bare file name.
        assert!(
            referred.contains(
                "<dt>Referenced by</dt><dd><a href=\"4.html\">#4</a> <a href=\"3.html\">#3</a></dd>"
            ),
            "{referred}"
        );

        let author_page = &once
            .iter()
            .find(|(name, _)| name == "a/Bea/index.html")
            .unwrap()
            .1;
        // Tagging: one page per author, newest first, linking the article.
        assert!(author_page.contains("<h1>Bea</h1>"), "{author_page}");
        assert!(author_page.contains("1 articles"), "{author_page}");
        assert!(
            author_page.contains("<a href=\"../../g/bots.general/3.html\">Re: picnic</a>"),
            "{author_page}"
        );

        let article = &once
            .iter()
            .find(|(name, _)| name == "g/bots.general/3.html")
            .unwrap()
            .1;
        assert!(article.contains("<pre>I&#39;ll bring bread.\n</pre>"));
        assert!(article.contains("<dt>From</dt><dd><a href=\"../../a/Bea/index.html\">Bea &lt;bea@bots.local&gt;</a></dd>"));
        assert!(article.contains("<dt>Message-ID</dt><dd><code>&lt;pic-3@slime&gt;</code></dd>"));
        assert!(article.contains("<a href=\"1.html\"><code>&lt;pic-1@slime&gt;</code></a>"));
        assert!(article.contains("<code>&lt;pic-2@slime&gt;</code>"));
        assert!(!article.contains("pic-2@slime</code></a>"));
        assert!(!article.contains("<bea@bots.local>"));
        assert!(!article.contains("<ada@bots.local>"));
    }

    #[test]
    fn example_snapshot_text_is_html_escaped() {
        let mut snapshot = example();
        let payload = "<script>alert(1)</script>\"><img src=x onerror=alert(1)> & 'x'";
        snapshot.generated_at = payload.to_string();
        snapshot.generator = payload.to_string();
        snapshot.groups[0].name = "../<script>alert(1)</script>".to_string();
        snapshot.groups[0].description = payload.to_string();
        snapshot.groups[0].posting = payload.to_string();
        snapshot.groups[0].creator = "<svg/onload=alert(1)>".to_string();
        let article = &mut snapshot.groups[1].threads[0].articles[0];
        article.subject = payload.to_string();
        article.from = payload.to_string();
        article.date = payload.to_string();
        article.message_id = payload.to_string();
        article.body = format!("{payload}\n");
        article.references = vec![payload.to_string()];
        snapshot.groups[1].threads[0].articles[1].parent_message_id = Some(payload.to_string());
        snapshot.groups[1].description = "javascript:alert(1)".to_string();

        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("site");
        render_site(&snapshot, &out, DEFAULT_SITE_TITLE, DEFAULT_REFRESH_SECONDS).unwrap();
        assert_pages_are_static(&out);

        let pages = files_under(&out);
        let joined = pages
            .iter()
            .map(|(_, html)| html.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        assert!(!joined.to_ascii_lowercase().contains("<script"));
        assert!(!joined.contains("<img"));
        assert!(!joined.contains("<svg"));
        assert!(joined.contains("&lt;script&gt;alert(1)&lt;/script&gt;"));
        assert!(joined.contains("&quot;&gt;&lt;img src=x onerror=alert(1)&gt;"));
        assert!(joined.contains("&amp;"));
        assert!(joined.contains("&#39;x&#39;"));
        assert!(joined.contains("&lt;svg/onload=alert(1)&gt;"));
        let hostile = sanitize_segment("../<script>alert(1)</script>").unwrap();
        assert!(
            pages
                .iter()
                .any(|(name, _)| name.starts_with(&format!("g/{hostile}/")))
        );
        assert_ne!(hostile, "..");
        assert!(!hostile.contains('/'));
        assert!(!hostile.contains('<'));
        assert!(!hostile.contains('\\'));
        let outside = dir.path().join("etc");
        assert!(!outside.exists());
    }

    #[test]
    fn from_display_name_is_the_part_before_the_address() {
        assert_eq!(
            from_display_name("omp on host, session 1 <a+h@x>"),
            Some("omp on host, session 1")
        );
        assert_eq!(from_display_name("omp <omp@x>"), Some("omp"));
        assert_eq!(from_display_name("omp@x"), None);
        assert_eq!(from_display_name(" <a@x>"), None);
        assert_eq!(from_display_name(""), None);
    }

    #[test]
    fn sanitize_segment_is_one_safe_component() {
        assert_eq!(sanitize_segment("bots.general").unwrap(), "bots.general");
        assert_eq!(sanitize_segment(".").unwrap(), "~2E");
        assert_eq!(sanitize_segment("..").unwrap(), "~2E~2E");
        assert_eq!(sanitize_segment("").unwrap(), "~");
        assert_eq!(
            sanitize_segment("javascript:alert(1)").unwrap(),
            "javascript~3Aalert~281~29"
        );
        assert_eq!(
            sanitize_segment("foo/../../etc/passwd").unwrap(),
            "foo~2F..~2F..~2Fetc~2Fpasswd"
        );
        assert_eq!(sanitize_segment("a b").unwrap(), "a~20b");
        assert_eq!(sanitize_segment("%2E").unwrap(), "~252E");
        assert_eq!(sanitize_segment("~2E").unwrap(), "~7E2E");
        let long = "a".repeat(MAX_SEGMENT_BYTES + 1);
        assert!(sanitize_segment(&long).is_err());
    }

    #[test]
    fn deep_replies_are_clamped_and_duplicate_numbers_fail() {
        let mut snapshot = example();
        snapshot.groups[1].threads[0].articles[2].depth = 100;
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("site");
        render_site(&snapshot, &out, DEFAULT_SITE_TITLE, DEFAULT_REFRESH_SECONDS).unwrap();
        let group = fs::read_to_string(out.join("g/bots.general/index.html")).unwrap();
        assert!(group.contains("style=\"margin-left: 32em\""));
        assert!(!group.contains("margin-left: 100em"));

        snapshot.groups[1].threads[1].articles[0].number = 1;
        let err =
            render_site(&snapshot, &out, DEFAULT_SITE_TITLE, DEFAULT_REFRESH_SECONDS).unwrap_err();
        assert!(err.to_string().contains("two articles numbered 1"));
    }

    #[test]
    fn a_later_render_drops_pages_for_articles_that_are_gone() {
        let snapshot = example();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("site");
        render_site(&snapshot, &out, DEFAULT_SITE_TITLE, DEFAULT_REFRESH_SECONDS).unwrap();
        assert!(out.join("g/bots.general/5.html").is_file());

        let mut smaller = snapshot.clone();
        smaller.groups.pop();
        render_site(&smaller, &out, DEFAULT_SITE_TITLE, DEFAULT_REFRESH_SECONDS).unwrap();
        assert!(out.join("g/bots.announce/1.html").is_file());
        assert!(!out.join("g/bots.general").exists());
        assert!(!dir.path().join(".site.rendering").exists());
        assert!(!dir.path().join(".site.previous").exists());
    }

    #[test]
    fn wrong_version_and_overlapping_site_are_rejected() {
        let mut snapshot = example();
        snapshot.version = 2;
        let dir = tempfile::tempdir().unwrap();
        let err = render_site(
            &snapshot,
            &dir.path().join("site"),
            DEFAULT_SITE_TITLE,
            DEFAULT_REFRESH_SECONDS,
        )
        .unwrap_err();
        assert!(err.to_string().contains("version 2"));

        let snap = dir.path().join("snapshot.json");
        fs::write(&snap, "{}\n").unwrap();
        assert!(site_replaces_snapshot(&snap, dir.path()));
        assert!(!site_replaces_snapshot(&snap, &dir.path().join("site")));
        assert!(site_replaces_snapshot(
            &dir.path().join("site").join("snapshot.json"),
            &dir.path().join("site")
        ));
    }

    #[test]
    fn empty_snapshot_is_an_index_only() {
        let mut snapshot = example();
        snapshot.groups.clear();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("site");
        let rendered =
            render_site(&snapshot, &out, DEFAULT_SITE_TITLE, DEFAULT_REFRESH_SECONDS).unwrap();
        assert_eq!(rendered.files, 1);
        assert_eq!(rendered.articles, 0);
        let index = fs::read_to_string(out.join("index.html")).unwrap();
        assert!(index.contains("No groups in this snapshot."));
        assert!(index.contains(FOOTER));
        assert_pages_are_static(&out);
    }

    #[test]
    fn listing_pages_reload_and_article_pages_do_not() {
        let snapshot = example();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("site");
        render_site(&snapshot, &out, DEFAULT_SITE_TITLE, 30).unwrap();
        let meta = "<meta http-equiv=\"refresh\" content=\"30\">\n";
        let index = fs::read_to_string(out.join("index.html")).unwrap();
        assert!(index.contains(meta), "{index}");
        let group = fs::read_to_string(out.join("g/bots.general/index.html")).unwrap();
        assert!(group.contains(meta), "{group}");
        let article = fs::read_to_string(out.join("g/bots.general/3.html")).unwrap();
        assert!(!article.contains("http-equiv=\"refresh\""), "{article}");
        assert_pages_are_static(&out);
    }

    #[test]
    fn zero_refresh_leaves_every_page_static() {
        let snapshot = example();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("site");
        render_site(&snapshot, &out, DEFAULT_SITE_TITLE, 0).unwrap();
        for (rel, html) in files_under(&out) {
            assert!(!html.contains("http-equiv=\"refresh\""), "{rel}");
        }
        assert_pages_are_static(&out);
    }

    #[test]
    fn from_names_the_author_and_a_bare_account_falls_back_to_the_signature() {
        // A From with its own display name is the author; a legacy body
        // signature adds no row and does not rename it.
        let mut snapshot = example();
        let article = &mut snapshot.groups[1].threads[0].articles[1];
        article.body = format!(
            "{}\n\n\u{2014} omp on britton-desktop, session <abc&>\n",
            article.body.trim_end()
        );
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("site");
        render_site(&snapshot, &out, DEFAULT_SITE_TITLE, DEFAULT_REFRESH_SECONDS).unwrap();

        let group = fs::read_to_string(out.join("g/bots.general/index.html")).unwrap();
        assert!(
            group.contains(
                "Re: picnic</a> <span class=\"from\"><a href=\"../../a/Bea/index.html\">Bea</a></span>"
            ),
            "{group}"
        );
        let page = fs::read_to_string(out.join("g/bots.general/3.html")).unwrap();
        assert!(!page.contains("<dt>Session</dt>"), "{page}");
        assert!(
            page.contains("<dt>From</dt><dd><a href=\"../../a/Bea/index.html\">Bea &lt;bea@bots.local&gt;</a></dd>"),
            "{page}"
        );

        // A bare machine account has no identity of its own, so the body
        // signature labels the author instead.
        let mut bare = example();
        let article = &mut bare.groups[1].threads[0].articles[1];
        article.from = "britton-desktop <britton-desktop@swarm.local>".to_string();
        article.body = format!(
            "{}\n\n\u{2014} omp on britton-desktop, session 01a1eba6\n",
            article.body.trim_end()
        );
        render_site(&bare, &out, DEFAULT_SITE_TITLE, DEFAULT_REFRESH_SECONDS).unwrap();
        let group = fs::read_to_string(out.join("g/bots.general/index.html")).unwrap();
        assert!(
            group.contains(">omp on britton-desktop, session 01a1eba6</a></span>"),
            "{group}"
        );
        assert_pages_are_static(&out);
    }

    #[test]
    fn claim_metadata_rows_are_parsed_and_eta_normalized() {
        let mut snapshot = example();
        let article = &mut snapshot.groups[1].threads[0].articles[0];
        article.from = "kiln on britton-desktop <kiln@swarm.local>".to_string();
        article.date = "Thu, 19 Nov 2026 14:51:36 +0000".to_string();
        article.subject = "CLAIM brittonr/0x1de docs".to_string();
        article.body = "Claiming through the Kiln fleet.\npurpose: Write the doc\nholder: omp on britton-desktop\nfence: 23\nexpected lease: 120 minutes\n".to_string();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("site");
        render_site(&snapshot, &out, DEFAULT_SITE_TITLE, DEFAULT_REFRESH_SECONDS).unwrap();
        let page = fs::read_to_string(out.join("g/bots.general/1.html")).unwrap();
        assert!(
            page.contains("<dt>Purpose</dt><dd>Write the doc</dd>"),
            "{page}"
        );
        assert!(
            page.contains("<dt>Holder</dt><dd>omp on britton-desktop</dd>"),
            "{page}"
        );
        assert!(page.contains("<dt>Fence</dt><dd>23</dd>"), "{page}");
        assert!(
            page.contains("<dt>ETA</dt><dd>2026-11-19T16:51Z</dd>"),
            "{page}"
        );
        assert!(!page.contains("<dt>Session</dt>"), "{page}");
        assert!(
            page.contains(
                "<dt>From</dt><dd><a href=\"../../a/kiln~20on~20britton-desktop/index.html\">"
            ),
            "{page}"
        );

        let mut explicit = example();
        explicit.groups[1].threads[0].articles[0].body =
            "work\neta: 2026-11-19T07:00:00Z\n".to_string();
        render_site(&explicit, &out, DEFAULT_SITE_TITLE, DEFAULT_REFRESH_SECONDS).unwrap();
        let page = fs::read_to_string(out.join("g/bots.general/1.html")).unwrap();
        assert!(
            page.contains("<dt>ETA</dt><dd>2026-11-19T07:00Z</dd>"),
            "{page}"
        );
    }

    #[test]
    fn the_site_title_heads_the_index_and_group_pages_escaped() {
        let snapshot = example();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("site");
        render_site(&snapshot, &out, "swarm <&>", DEFAULT_REFRESH_SECONDS).unwrap();
        let index = fs::read_to_string(out.join("index.html")).unwrap();
        assert!(index.contains("<title>swarm &lt;&amp;&gt;</title>"));
        assert!(index.contains("<h1>swarm &lt;&amp;&gt;</h1>"));
        let group = fs::read_to_string(out.join("g/bots.general/index.html")).unwrap();
        assert!(group.contains("<title>bots.general — swarm &lt;&amp;&gt;</title>"));
        assert!(!index.contains("slime") && !group.contains("slime"));
        assert_pages_are_static(&out);
    }

    #[test]
    fn newest_key_orders_across_months_and_seconds() {
        let nov = newest_key("Mon, 30 Nov 2026 23:59:59 +0000");
        let dec = newest_key("Tue, 01 Dec 2026 00:00:01 +0000");
        let dec_later = newest_key("Tue, 01 Dec 2026 00:00:30 +0000");
        assert!(
            dec > nov,
            "Dec 1 ({dec}) must sort newer than Nov 30 ({nov})"
        );
        assert!(dec_later > dec, "seconds must break ties within a minute");
        assert!(newest_key("not a date") < nov);
    }

    #[test]
    fn index_caps_authors_and_writes_all_authors_page() {
        let mut snapshot = example();
        let group = &mut snapshot.groups[0];
        group.threads.clear();
        for i in 1..=30u64 {
            group.threads.push(SnapshotThread {
                root_message_id: format!("<a{i}@swarm.local>"),
                articles: vec![SnapshotArticle {
                    number: i,
                    message_id: format!("<a{i}@swarm.local>"),
                    parent_message_id: None,
                    depth: 0,
                    from: format!("agent-{i:02} <host+s{i}@swarm.local>"),
                    subject: format!("post {i}"),
                    date: format!("Mon, 23 Nov 2026 12:{:02}:00 +0000", i),
                    references: Vec::new(),
                    body: "hello\n".to_string(),
                }],
            });
        }
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("site");
        render_site(&snapshot, &out, "swarm", DEFAULT_REFRESH_SECONDS).unwrap();
        let index = fs::read_to_string(out.join("index.html")).unwrap();
        assert!(index.contains("Showing 25 of 33 authors"), "{index}");
        assert!(
            index.contains("agent-30"),
            "newest author should be on index"
        );
        assert!(
            !index.contains("agent-01"),
            "oldest author should be capped off index"
        );
        let all_authors = fs::read_to_string(out.join("a/index.html")).unwrap();
        assert!(all_authors.contains("33 authors"), "{all_authors}");
        assert!(
            all_authors.contains("agent-01"),
            "all authors page must list everyone"
        );
        assert!(all_authors.contains("agent-30"));
        assert_pages_are_static(&out);
    }

    #[test]
    fn bots_claims_renders_open_claims_dashboard_and_excludes_closed() {
        let mut snapshot = example();
        snapshot.generated_at = "2026-11-24T06:00:00Z".to_string();
        snapshot.groups.push(SnapshotGroup {
            name: "bots.claims".to_string(),
            description: "Active scope claims".to_string(),
            high: 5,
            low: 1,
            posting: "y".to_string(),
            creator: String::new(),
            threads: vec![
                SnapshotThread {
                    root_message_id: "<c1@swarm.local>".to_string(),
                    articles: vec![
                        SnapshotArticle {
                            number: 1,
                            message_id: "<c1@swarm.local>".to_string(),
                            parent_message_id: None,
                            depth: 0,
                            from: "kiln on host <kiln@swarm.local>".to_string(),
                            subject: "CLAIM org/closed .".to_string(),
                            date: "Tue, 24 Nov 2026 04:00:00 +0000".to_string(),
                            references: vec![],
                            body: "purpose: Done work\nholder: omp on host\nexpected lease: 60 minutes\n".to_string(),
                        },
                        SnapshotArticle {
                            number: 2,
                            message_id: "<r1@swarm.local>".to_string(),
                            parent_message_id: Some("<c1@swarm.local>".to_string()),
                            depth: 1,
                            from: "kiln on host <kiln@swarm.local>".to_string(),
                            subject: "RELEASE org/closed .".to_string(),
                            date: "Tue, 24 Nov 2026 04:30:00 +0000".to_string(),
                            references: vec!["<c1@swarm.local>".to_string()],
                            body: "Released.\n".to_string(),
                        },
                    ],
                },
                SnapshotThread {
                    root_message_id: "<c2@swarm.local>".to_string(),
                    articles: vec![SnapshotArticle {
                        number: 3,
                        message_id: "<c2@swarm.local>".to_string(),
                        parent_message_id: None,
                        depth: 0,
                        from: "agent1 <host+1@swarm.local>".to_string(),
                        subject: "CLAIM org/superseded .".to_string(),
                        date: "Tue, 24 Nov 2026 03:00:00 +0000".to_string(),
                        references: vec![],
                        body: "purpose: Old lease\neta: 2026-11-24T05:00:00Z\n".to_string(),
                    }],
                },
                SnapshotThread {
                    root_message_id: "<c3@swarm.local>".to_string(),
                    articles: vec![SnapshotArticle {
                        number: 4,
                        message_id: "<c3@swarm.local>".to_string(),
                        parent_message_id: None,
                        depth: 0,
                        from: "agent1 <host+1@swarm.local>".to_string(),
                        subject: "CLAIM org/superseded .".to_string(),
                        date: "Tue, 24 Nov 2026 05:00:00 +0000".to_string(),
                        references: vec![],
                        body: "purpose: Renewed lease\neta: 2026-11-24T08:00:00Z\nsupersedes: <c2@swarm.local>\n".to_string(),
                    }],
                },
                SnapshotThread {
                    root_message_id: "<c4@swarm.local>".to_string(),
                    articles: vec![SnapshotArticle {
                        number: 5,
                        message_id: "<c4@swarm.local>".to_string(),
                        parent_message_id: None,
                        depth: 0,
                        from: "kiln on host <kiln@swarm.local>".to_string(),
                        subject: "CLAIM host system".to_string(),
                        date: "Tue, 24 Nov 2026 02:00:00 +0000".to_string(),
                        references: vec![],
                        body: "purpose: Deploy <script>\nholder: antigravity on host\nexpected lease: 60 minutes\n".to_string(),
                    }],
                },
            ],
        });
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("site");
        render_site(&snapshot, &out, "swarm", DEFAULT_REFRESH_SECONDS).unwrap();

        let claims = fs::read_to_string(out.join("claims.html")).unwrap();
        assert!(claims.contains("2 open claims"), "{claims}");
        assert!(
            !claims.contains("org/closed"),
            "released claim must be excluded"
        );
        assert!(
            !claims.contains("Old lease"),
            "superseded claim must be excluded"
        );
        assert!(claims.contains("org/superseded ."), "{claims}");
        assert!(claims.contains("Renewed lease"), "{claims}");
        assert!(claims.contains("host system"), "{claims}");
        assert!(
            claims.contains("OVERDUE"),
            "expired claim must be marked OVERDUE"
        );
        assert!(
            claims.contains("Deploy &lt;script&gt;"),
            "purpose must be escaped"
        );
        let index = fs::read_to_string(out.join("index.html")).unwrap();
        assert!(
            index.contains("href=\"claims.html\">Open claims</a>"),
            "{index}"
        );
        let group = fs::read_to_string(out.join("g/bots.claims/index.html")).unwrap();
        assert!(group.contains("2 open claims"), "{group}");
        assert_pages_are_static(&out);
    }
}
