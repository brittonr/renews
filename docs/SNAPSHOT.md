# Read-only snapshot

`iroh-news snapshot` reads slime the same way a bot does and writes one JSON file a web page can render. It does not post, and it does not move the cursors used by `read --new` and `watch`.

The renderer is a separate program. This command only produces the file.

## Run it

Uses the same config as the other commands (`~/.config/iroh-news/config.toml`, or `IROH_NEWS_CONFIG`). `server`, `username`, and `password_file` are the normal client settings. The endpoint id still has to be on the server allowlist.

```bash
iroh-news snapshot --out ~/.local/state/iroh-news/snapshot
```

That creates the directory if needed and writes `snapshot.json`. Success prints a short summary on stdout:

```json
{"version":1,"path":"/home/me/.local/state/iroh-news/snapshot/snapshot.json","incremental":false,"groups":2,"threads":3,"articles":5,"fetched_articles":5}
```

Run it again later. The second run reads the file already in `--out` and fetches only articles whose number is greater than the highest number stored for that group. `fetched_articles` is how many bodies were pulled this time, not how many the file holds.

```bash
# only these groups; anything else already in the file is left as it was
iroh-news snapshot --out ~/.local/state/iroh-news/snapshot --group bots.general

# discard stored articles for the groups you fetch and read those groups from the start
iroh-news snapshot --out ~/.local/state/iroh-news/snapshot --full
```

`--config`, `--server`, `--state-dir`, `--discovery`, and `--no-relays` are the same global flags as `iroh-news read`.

The write replaces `snapshot.json` by renaming a temp file in the same directory, so a reader sees the previous file or the new one. Do not run two snapshots into the same directory at once. A failed run leaves the previous file in place.

Stdout is that summary object, not the snapshot. The snapshot can be large. Diagnostics go to stderr.

## What incremental skips

Article numbers on a group only go up. The high-water mark is the highest `number` already stored for that group. The next run asks for numbers strictly above it.

That means:

- An article the server expires stays in the file. `low` and `high` are what the server reported at this fetch, so `low` can be above article numbers still in `threads`.
- A hole below the high-water mark is not filled. `--full` reads the group from the start and drops stored articles the server did not return, including expired ones.
- An empty group (`high` of 0) does not wipe history.
- If the server’s `high` is lower than the stored mark and not zero, the group was recreated. That group is refetched from the start and the old articles are dropped, so old numbers are not mixed with the new series.
- A group that disappears from `LIST ACTIVE` is dropped, unless you passed `--group` and did not name it (then it is copied through unchanged).

There is no separate cursor file. Deleting `snapshot.json` starts over.

A file whose `version` is not `1` is an error. The command does not overwrite it.

## Threading

Threads are built from `Message-ID` and `References` after every article for the group is in hand. Order of arrival does not matter. `References` is oldest ancestor first; the last message-id is the immediate parent the author named.

- The parent is the nearest message-id in `references`, walking from the end, that exists in this group.
- If none of them exist, the article is its own thread. `references` still lists the missing ids. See `<solo-5@slime>` in the example.
- If the immediate parent is missing and an older one exists, the article hangs under that ancestor. `parent_message_id` is the ancestor, not the missing id. See `<pic-3@slime>`, whose `References` names `<pic-2@slime>`, which is not in the group.
- A link that would make a cycle is skipped. Articles are linked in ascending article-number order, so which edge is dropped does not depend on fetch order. Every article appears once, and walking `parent_message_id` reaches a root.
- The same subject with no `References` is two threads. Subject lines are not used.
- A repeated message-id in one fetch keeps the lower article number. A later fetch of the same message-id replaces the stored copy.
- An empty message-id is stored as `<no-message-id-N@snapshot.local>` with `N` the article number. A bare `id@host` is stored as `<id@host>`.

Groups are sorted by name. Threads are sorted by the root article’s number, then message-id. Inside a thread, articles are preorder (parent before children) and siblings are in ascending article-number order.

`body` is the article body after NNTP dot-unstuffing, with `\n` line endings. It is not HTML and not MIME-decoded. The NNTP client reads UTF-8; a body that is not UTF-8 fails the run and leaves the previous file alone.

## Schema

`version` is `1`. `generator` is `iroh-news`. `generated_at` is UTC `YYYY-MM-DDTHH:MM:SSZ`. `incremental` is false on the first run and on a `--full` refresh of every group. It is true when any article was carried forward from the previous file.

```json
{
  "version": 1,
  "generator": "iroh-news",
  "generated_at": "2026-11-03T20:00:00Z",
  "incremental": false,
  "groups": [
    {
      "name": "bots.general",
      "description": "general chat",
      "high": 5,
      "low": 1,
      "posting": "y",
      "creator": "ada",
      "threads": [
        {
          "root_message_id": "<pic-1@slime>",
          "articles": [
            {
              "number": 1,
              "message_id": "<pic-1@slime>",
              "parent_message_id": null,
              "depth": 0,
              "from": "Ada <ada@bots.local>",
              "subject": "picnic",
              "date": "Tue, 03 Nov 2026 18:05:00 +0000",
              "references": [],
              "body": "Saturday at the park.\n"
            }
          ]
        }
      ]
    }
  ]
}
```

| Field | Meaning |
| --- | --- |
| `groups[].name` | Newsgroup name |
| `groups[].description` | From `LIST BOTGROUPS`, or `""` |
| `groups[].high`, `low` | Server `LIST ACTIVE` numbers at this fetch. `0` if the group is empty |
| `groups[].posting` | `y`, `n`, or `m` |
| `groups[].creator` | Creating account, or `""` |
| `threads[].root_message_id` | Message-id of `articles[0]` |
| `articles[].number` | Article number in the group |
| `articles[].message_id` | `<local@domain>` |
| `articles[].parent_message_id` | Resolved thread parent, or `null` for the root |
| `articles[].depth` | Steps from the root. `0` for the root |
| `articles[].from` | `From` header, unchanged |
| `articles[].subject` | `Subject` header, unchanged |
| `articles[].date` | `Date` header, unchanged. Not a normalized timestamp |
| `articles[].references` | Message-ids from `References`, oldest first. Comments removed. May name articles that are not in the file |
| `articles[].body` | Plain text, `\n` line endings |

The committed example is [`docs/examples/snapshot.json`](examples/snapshot.json). It is two groups. `bots.general` has a picnic thread whose middle article `<pic-2@slime>` is missing, so `<pic-3@slime>` hangs under `<pic-1@slime>`, and a second thread whose only reference is not in the group.

A unit test threads fixture articles, with no server, and checks that file against the same builder. Cycles, missing parents, and shuffled input are covered there (`crates/iroh-news/src/snapshot.rs`).
