# Web viewer

A read-only static site of slime. `iroh-news render` turns a [snapshot](SNAPSHOT.md) into HTML. Nothing in the pages dials the server, and the pages do not run JavaScript. Article text is untrusted, so every snapshot string is escaped.

Put the hostname behind [Cloudflare Access](https://developers.cloudflare.com/cloudflare-one/access-controls/applications/http-apps/). Access is the login. The site does not check passwords, cookies, or endpoint ids. Until an Access policy is attached, the `workers.dev` hostname is public. Each page also sends `noindex`, which only asks crawlers to skip it.

## Render

```bash
iroh-news snapshot --out ~/.local/state/iroh-news/snapshot
iroh-news render \
  --snapshot ~/.local/state/iroh-news/snapshot/snapshot.json \
  --out ~/.local/state/iroh-news/site
```

`--snapshot` may be the JSON file or the directory that contains it. The same snapshot produces the same files. The clock at render time is not written anywhere. The footer is the snapshot's `generated_at` and `generator`.

`snapshot --html` does both steps. The HTML directory must not be the snapshot directory, and it must not contain `snapshot.json`. Rendering replaces the site directory.

```bash
iroh-news snapshot \
  --out ~/.local/state/iroh-news/snapshot \
  --html ~/.local/state/iroh-news/site
```

Do not run two renders into the same directory at once.

Open `index.html` in a browser, or any static file server. No build step and no assets beyond the HTML. The stylesheet is inline. It follows `prefers-color-scheme`. Article bodies are in `<pre>` with a monospace font.

| Page | URL |
| --- | --- |
| Groups | `/` and `/index.html` |
| One group | `/g/<segment>/` and `/g/<segment>/index.html` |
| One article | `/g/<segment>/<number>.html` |

The index lists each group name, description, article count, and the `Date` header of the highest article number. A group page lists threads in the snapshot's preorder. Replies are indented by `depth` (capped at 32em). An article page shows the headers and the body.

`<segment>` is the group name with a safe subset kept as-is: ASCII letters, digits, `.`, `_`, `-`, `+`. Every other byte is written `~` plus two uppercase hex digits (`~HH`). `~` itself is encoded, so two different names cannot land on the same directory. A name that is `.` or `..` is encoded entirely. An empty name becomes `~`. `~HH` stays one path segment even if the server decodes `%2F`. Article numbers are decimal with no padding. A group with two articles of the same number is an error. A second render drops pages for articles that are no longer in the snapshot.

These characters in snapshot text are escaped in body text and in attributes: `&`, `<`, `>`, `"`, `'`. A null byte becomes U+FFFD.

`render` prints a one-line JSON summary on success. `snapshot --html` adds an `html` field to the snapshot summary.

## Publish to Cloudflare Workers

The site is a Worker with static assets and no Worker script. [`deploy/web-viewer/wrangler.toml`](../deploy/web-viewer/wrangler.toml) points `[assets].directory` at `./site`. The default Worker name is `slime-viewer`.

[`deploy/web-viewer/publish.sh`](../deploy/web-viewer/publish.sh) runs snapshot, then render, then `wrangler deploy`. Wrangler reads the credentials from the environment. They are not written into the repo or passed as arguments.

```bash
export CLOUDFLARE_API_TOKEN=...
export CLOUDFLARE_ACCOUNT_ID=...
# optional
export WORKER_NAME=slime-viewer

deploy/web-viewer/publish.sh --config ~/.config/iroh-news/config.toml
```

Extra arguments are passed to `iroh-news snapshot`. `--out` is set by the script. `SKIP_SNAPSHOT=1` renders the snapshot already on disk and deploys that.

| Variable | Meaning |
| --- | --- |
| `CLOUDFLARE_API_TOKEN` | Required. API token. |
| `CLOUDFLARE_ACCOUNT_ID` | Required. Account id from the dashboard URL. |
| `WORKER_NAME` | Worker name. Default `slime-viewer`. |
| `IROH_NEWS` | Binary. Default `iroh-news` on `PATH`. |
| `WRANGLER` | Binary. Default `wrangler` on `PATH`. |
| `SNAPSHOT_DIR` | Snapshot directory. Default `$XDG_STATE_HOME/iroh-news/snapshot` or `~/.local/state/iroh-news/snapshot`. |
| `SITE_DIR` | HTML directory. Default `deploy/web-viewer/site`. |

`WORKER_NAME` must match `[A-Za-z0-9][A-Za-z0-9_-]{0,62}`.

Create the token in the Cloudflare dashboard (My Profile → API Tokens, or an account-owned token). The first deploy creates the Worker, which needs the Workers product **Admin** role. A later deploy of that same Worker needs **Editor** on it. The older token permission for this is Account → Workers Scripts → Edit. Do not grant the token KV, R2, or zone DNS unless you add those yourself. Do not put the token or the account id in the Nix store, a unit file, or git.

The deploy publishes `https://<worker-name>.<account-subdomain>.workers.dev`. Attach a custom domain in the dashboard if you want one. This config does not declare routes, so it does not replace a domain you attached there. Create a Cloudflare Access application for whichever hostname you use, and allow the people who should read slime. A self-hosted or workers.dev application both work. Access runs at Cloudflare, in front of the assets.

`iroh-news` still needs the usual client config: `server`, `username`, and `password_file`. The endpoint id must be on the server allowlist. `snapshot` does not post and does not move the `read --new` cursors.

## NixOS timer

`nixosModules.renews-iroh` also defines `services.iroh-news-viewer`. It is off by default. When enabled, a systemd timer runs `publish.sh` every `intervalMinutes` (default 5) as the system user `iroh-news-viewer`.

```nix
services.iroh-news-viewer = {
  enable = true;
  intervalMinutes = 5;
  workerName = "slime-viewer";
  configFile = "/var/lib/iroh-news-viewer/config.toml";
  environmentFile = "/run/secrets/slime-viewer-env";
  # extraSnapshotArgs = [ "--full" ];
};
```

`configFile` and `environmentFile` are strings, not Nix paths, so neither file is copied into the Nix store.

`/var/lib/iroh-news-viewer/config.toml` is a normal iroh-news config. The password file it names must be readable by `iroh-news-viewer`. `ProtectHome` is on, so keep both files outside `/home` (`/var/lib/...` or `/run/secrets/...`).

`/run/secrets/slime-viewer-env` is a systemd EnvironmentFile:

```
CLOUDFLARE_API_TOKEN=...
CLOUDFLARE_ACCOUNT_ID=...
```

systemd reads that file as root. Mode `0600` owned by root is enough. The token is not stored in the unit.

State lives in `/var/lib/iroh-news-viewer` (`StateDirectory`, mode from `UMask=0077`): the iroh secret key, `snapshot/snapshot.json`, and `site/`. The first run creates the key. Copy the endpoint id onto the server allowlist, and make sure the NNTP account exists:

```bash
sudo -u iroh-news-viewer iroh-news --state-dir /var/lib/iroh-news-viewer id
```

`iroh-news` is on the system PATH when the service is enabled. Then:

```bash
systemctl start iroh-news-viewer.service
journalctl -u iroh-news-viewer -e
systemctl list-timers iroh-news-viewer.timer
```

The timer starts two minutes after boot and then every `intervalMinutes` after each run. `Persistent` catches a run that was missed while the machine was off. One publish can take a while on a cold snapshot. The service allows 30 minutes.

Set `services.iroh-news-viewer.package` if you are not using the flake module. The flake module sets it to `iroh-news`. Wrangler defaults to `pkgs.wrangler`.
