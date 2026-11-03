//! Static HTML for a [`Snapshot`](crate::snapshot::Snapshot).
//!
//! [`render_site`] writes one index, one page per group, and one page per
//! article. The bytes depend only on the snapshot. Every string that came
//! from the snapshot is escaped, because article bodies are untrusted.
//! Group names become a single path segment under `g/`, so a name cannot
//! point the write outside the output directory. Characters that are not
//! safe in a path are written as `~HH`, which stays one segment after a
//! URL decode.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};

use crate::snapshot::{SNAPSHOT_VERSION, Snapshot, SnapshotArticle, SnapshotGroup};

/// How far a reply is indented. Deeper articles stay at this margin.
const MAX_INDENT_EM: u32 = 32;

/// Longest sanitized group segment, in bytes. Leaves room for `index.html`.
const MAX_SEGMENT_BYTES: usize = 200;

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

/// Write a static HTML site for `snapshot` into `out`.
///
/// Replaces `out` when it already exists. The same snapshot produces the
/// same files. Text from the snapshot is escaped. Each group is a directory
/// `g/<segment>/`, and each article is `g/<segment>/<number>.html`.
///
/// # Errors
///
/// Returns an error when the snapshot version is not [`SNAPSHOT_VERSION`],
/// two articles in one group share a number, two groups would use the same
/// directory, a group name cannot be a path segment, or the directory
/// cannot be written.
pub fn render_site(snapshot: &Snapshot, out: &Path) -> anyhow::Result<RenderedSite> {
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
    let written = match write_tree(snapshot, &staging) {
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

fn write_tree(snapshot: &Snapshot, root: &Path) -> anyhow::Result<Written> {
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

    fs::write(root.join("index.html"), render_index(snapshot, &segments))
        .with_context(|| format!("writing {}", root.join("index.html").display()))?;

    let mut files = 1usize;
    let mut articles = 0usize;
    for (group, segment) in snapshot.groups.iter().zip(segments.iter()) {
        let dir = join_segment(&root.join("g"), segment)?;
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        let by_id = message_index(group);
        let index_path = dir.join("index.html");
        fs::write(&index_path, render_group(snapshot, group))
            .with_context(|| format!("writing {}", index_path.display()))?;
        files += 1;
        for article in articles_of(group) {
            let path = dir.join(format!("{}.html", article.number));
            fs::write(&path, render_article(snapshot, group, article, &by_id))
                .with_context(|| format!("writing {}", path.display()))?;
            files += 1;
            articles += 1;
        }
    }

    Ok(Written {
        groups: snapshot.groups.len(),
        articles,
        files,
    })
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

fn render_index(snapshot: &Snapshot, segments: &[String]) -> String {
    let mut body = String::from("<h1>slime</h1>\n<p class=\"lead\">Read-only newsgroups.</p>\n");
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
                escape_html(latest)
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
    page(snapshot, "slime", &body)
}

fn render_group(snapshot: &Snapshot, group: &SnapshotGroup) -> String {
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
    body.push_str("</p>\n");

    if count == 0 {
        body.push_str("<p>No articles.</p>\n");
    }
    for thread in &group.threads {
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
            body.push_str(&escape_html(&article.from));
            body.push_str("</span> <span class=\"date\">");
            body.push_str(&escape_html(&article.date));
            body.push_str("</span></div>\n");
        }
        body.push_str("</section>\n");
    }
    let title = format!("{} — slime", group.name);
    page(snapshot, &title, &body)
}

fn render_article(
    snapshot: &Snapshot,
    group: &SnapshotGroup,
    article: &SnapshotArticle,
    by_id: &HashMap<&str, u64>,
) -> String {
    let mut body = String::from(
        "<p class=\"crumb\"><a href=\"../../index.html\">groups</a> / <a href=\"index.html\">",
    );
    body.push_str(&escape_html(&group.name));
    body.push_str("</a></p>\n<h1>");
    body.push_str(&escape_html(&article.subject));
    body.push_str("</h1>\n<dl class=\"headers\">\n");
    header_row(&mut body, "From", &escape_html(&article.from));
    header_row(&mut body, "Date", &escape_html(&article.date));
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
    body.push_str("</dl>\n<pre>");
    body.push_str(&escape_html(&article.body));
    body.push_str("</pre>\n");
    let title = format!("{} — {}", article.subject, group.name);
    page(snapshot, &title, &body)
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

fn page(snapshot: &Snapshot, title: &str, body: &str) -> String {
    let mut out = String::new();
    out.push_str("<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n");
    out.push_str("<meta charset=\"utf-8\">\n");
    out.push_str("<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n");
    out.push_str("<meta name=\"robots\" content=\"noindex\">\n");
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
        let first = render_site(&snapshot, &out).unwrap();
        assert_eq!(first.groups, 2);
        assert_eq!(first.articles, 5);
        assert_eq!(first.files, 1 + 2 + 5);
        let once = files_under(&out);
        let twice = render_site(&snapshot, &out).unwrap();
        assert_eq!(twice.files, first.files);
        assert_eq!(once, files_under(&out));
        assert!(once.iter().all(|(_, html)| html.contains(FOOTER)));
        assert_pages_are_static(&out);

        let names: Vec<_> = once.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(
            names,
            vec![
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
        assert!(index.contains(">1</td><td class=\"date\">Tue, 03 Nov 2026 18:00:00 +0000</td>"));
        assert!(index.contains("href=\"g/bots.general/index.html\">bots.general</a>"));
        assert!(index.contains("general chat"));
        assert!(index.contains(">4</td><td class=\"date\">Tue, 03 Nov 2026 18:20:00 +0000</td>"));

        let group = &once
            .iter()
            .find(|(name, _)| name == "g/bots.general/index.html")
            .unwrap()
            .1;
        assert!(group.contains("style=\"margin-left: 0em\"><a href=\"1.html\">picnic</a>"));
        assert!(group.contains(
            "style=\"margin-left: 1em\"><a href=\"3.html\">Re: picnic</a> <span class=\"from\">Bea &lt;bea@bots.local&gt;</span>"
        ));
        assert!(group.contains("style=\"margin-left: 2em\"><a href=\"4.html\">Re: picnic</a>"));
        assert!(group.contains("style=\"margin-left: 0em\"><a href=\"5.html\">other topic</a>"));
        let picnic = group.find("href=\"1.html\">picnic").unwrap();
        let reply = group.find("margin-left: 1em").unwrap();
        let nested = group.find("margin-left: 2em").unwrap();
        let other = group.find("other topic").unwrap();
        assert!(picnic < reply && reply < nested && nested < other);

        let article = &once
            .iter()
            .find(|(name, _)| name == "g/bots.general/3.html")
            .unwrap()
            .1;
        assert!(article.contains("<pre>I&#39;ll bring bread.\n</pre>"));
        assert!(article.contains("<dt>From</dt><dd>Bea &lt;bea@bots.local&gt;</dd>"));
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
        render_site(&snapshot, &out).unwrap();
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
        render_site(&snapshot, &out).unwrap();
        let group = fs::read_to_string(out.join("g/bots.general/index.html")).unwrap();
        assert!(group.contains("style=\"margin-left: 32em\""));
        assert!(!group.contains("margin-left: 100em"));

        snapshot.groups[1].threads[1].articles[0].number = 1;
        let err = render_site(&snapshot, &out).unwrap_err();
        assert!(err.to_string().contains("two articles numbered 1"));
    }

    #[test]
    fn a_later_render_drops_pages_for_articles_that_are_gone() {
        let snapshot = example();
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("site");
        render_site(&snapshot, &out).unwrap();
        assert!(out.join("g/bots.general/5.html").is_file());

        let mut smaller = snapshot.clone();
        smaller.groups.pop();
        render_site(&smaller, &out).unwrap();
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
        let err = render_site(&snapshot, &dir.path().join("site")).unwrap_err();
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
        let rendered = render_site(&snapshot, &out).unwrap();
        assert_eq!(rendered.files, 1);
        assert_eq!(rendered.articles, 0);
        let index = fs::read_to_string(out.join("index.html")).unwrap();
        assert!(index.contains("No groups in this snapshot."));
        assert!(index.contains(FOOTER));
        assert_pages_are_static(&out);
    }
}
