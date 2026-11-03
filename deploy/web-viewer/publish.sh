#!/usr/bin/env bash
# Publish the slime web viewer: snapshot, render, wrangler deploy.
#
# Required environment (never commit these):
#   CLOUDFLARE_API_TOKEN
#   CLOUDFLARE_ACCOUNT_ID
#
# Optional:
#   WORKER_NAME         Worker name. Default: slime-viewer
#   IROH_NEWS           iroh-news binary. Default: iroh-news on PATH
#   WRANGLER            wrangler binary. Default: wrangler on PATH
#   SNAPSHOT_DIR        Where snapshot.json is written
#   SITE_DIR            Rendered site. Default: ./site next to this script
#   WRANGLER_TEMPLATE   wrangler.toml to copy. Default: beside this script
#   SKIP_SNAPSHOT=1     Render and deploy the snapshot already on disk
#
# Arguments are passed to `iroh-news snapshot` (for example --config, --full,
# --group). This script adds --out itself.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
worker_name="${WORKER_NAME:-slime-viewer}"
iroh_news="${IROH_NEWS:-iroh-news}"
wrangler="${WRANGLER:-wrangler}"
site_dir="${SITE_DIR:-${here}/site}"
template="${WRANGLER_TEMPLATE:-${here}/wrangler.toml}"
compatibility_date="2026-11-03"

if [[ ! "$worker_name" =~ ^[A-Za-z0-9][A-Za-z0-9_-]{0,62}$ ]]; then
  echo "publish.sh: WORKER_NAME must match [A-Za-z0-9][A-Za-z0-9_-]{0,62}" >&2
  exit 1
fi

if [[ -z "${CLOUDFLARE_API_TOKEN:-}" ]]; then
  echo "publish.sh: CLOUDFLARE_API_TOKEN is not set" >&2
  exit 1
fi
if [[ -z "${CLOUDFLARE_ACCOUNT_ID:-}" ]]; then
  echo "publish.sh: CLOUDFLARE_ACCOUNT_ID is not set" >&2
  exit 1
fi

if [[ -z "${SNAPSHOT_DIR:-}" ]]; then
  if [[ -n "${XDG_STATE_HOME:-}" ]]; then
    snapshot_dir="${XDG_STATE_HOME}/iroh-news/snapshot"
  elif [[ -n "${HOME:-}" ]]; then
    snapshot_dir="${HOME}/.local/state/iroh-news/snapshot"
  else
    echo "publish.sh: set SNAPSHOT_DIR or HOME" >&2
    exit 1
  fi
else
  snapshot_dir="${SNAPSHOT_DIR}"
fi

need_bin() {
  local cmd="$1"
  if [[ "$cmd" != */* ]] && ! command -v "$cmd" >/dev/null 2>&1; then
    echo "publish.sh: $cmd is not on PATH" >&2
    exit 1
  fi
  if [[ "$cmd" == */* && ! -x "$cmd" ]]; then
    echo "publish.sh: $cmd is not executable" >&2
    exit 1
  fi
}

need_bin "$iroh_news"
need_bin "$wrangler"

if [[ "${SKIP_SNAPSHOT:-0}" == "1" ]]; then
  if [[ "$#" -gt 0 ]]; then
    echo "publish.sh: extra arguments are for snapshot; unset SKIP_SNAPSHOT" >&2
    exit 1
  fi
  if [[ ! -f "${snapshot_dir}/snapshot.json" ]]; then
    echo "publish.sh: ${snapshot_dir}/snapshot.json does not exist" >&2
    exit 1
  fi
else
  "$iroh_news" snapshot --out "$snapshot_dir" "$@"
fi

"$iroh_news" render --snapshot "${snapshot_dir}/snapshot.json" --out "$site_dir"
site_dir="$(cd "$site_dir" && pwd)"

case "$site_dir" in
  *\"*|*\\*|*$'\n'*)
    echo "publish.sh: SITE_DIR cannot contain quotes, backslashes, or newlines" >&2
    exit 1
    ;;
esac

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

if [[ -f "$template" ]]; then
  sed \
    -e "s/^name = \".*\"/name = \"${worker_name}\"/" \
    -e "s|^directory = \".*\"|directory = \"${site_dir}\"|" \
    "$template" >"$work/wrangler.toml"
else
  cat >"$work/wrangler.toml" <<EOF
name = "${worker_name}"
compatibility_date = "${compatibility_date}"

[assets]
directory = "${site_dir}"
EOF
fi

if ! grep -F "name = \"${worker_name}\"" "$work/wrangler.toml" >/dev/null; then
  echo "publish.sh: wrangler config is missing the worker name" >&2
  exit 1
fi
if ! grep -F "directory = \"${site_dir}\"" "$work/wrangler.toml" >/dev/null; then
  echo "publish.sh: wrangler config is missing the site directory" >&2
  exit 1
fi

export WRANGLER_SEND_METRICS="${WRANGLER_SEND_METRICS:-false}"
(
  cd "$work"
  "$wrangler" deploy --name "$worker_name"
)
