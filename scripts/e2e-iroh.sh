#!/usr/bin/env bash
# Separate-process check: renews with the iroh listener, then iroh-news.
# Posts and reads as an allowlisted peer, rejects a second key, and checks
# the posting token bucket.
set -euo pipefail

root=$(cd "$(dirname "$0")/.." && pwd)
cd "$root"

cargo build -p renews --no-default-features --features iroh
cargo build -p iroh-news
renews="$root/target/debug/renews"
news="$root/target/debug/iroh-news"

tmp=$(mktemp -d)
srv=""
cleanup() {
  if [ -n "$srv" ]; then
    kill "$srv" 2>/dev/null || true
    wait "$srv" 2>/dev/null || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

mkdir -p "$tmp/state-a" "$tmp/state-b"
id_a=$("$news" --state-dir "$tmp/state-a" id)
id_b=$("$news" --state-dir "$tmp/state-b" id)
peer_a=$(printf '%s' "$id_a" | sed -n 's/.*"endpoint_id":"\([^"]*\)".*/\1/p')
test -n "$peer_a"

printf '%s\n' 'secret' > "$tmp/pass"
chmod 600 "$tmp/pass"

cat > "$tmp/renews.toml" <<EOF
addr = "127.0.0.1:11119"
site_name = "bots.local"
db_path = "sqlite://$tmp/news.db"
auth_db_path = "sqlite://$tmp/auth.db"
peer_db_path = "sqlite://$tmp/peers.db"

[iroh]
enabled = true
key_path = "$tmp/iroh.key"
localhost_tcp = true
bind_addr = "127.0.0.1:0"
wait_for_relay = false
allowed_peers = ["$peer_a"]

[user_limits]
post_rate_per_minute = 1
post_burst = 1
EOF

"$renews" --config "$tmp/renews.toml" --init
"$renews" --config "$tmp/renews.toml" admin add-group bots.general
"$renews" --config "$tmp/renews.toml" admin add-user bot --password-file "$tmp/pass"

"$renews" --config "$tmp/renews.toml" > "$tmp/server.log" 2>&1 &
srv=$!

ticket=""
for _ in $(seq 1 50); do
  ticket=$(sed -n 's/^iroh ticket: //p' "$tmp/server.log" || true)
  if [ -n "$ticket" ]; then
    break
  fi
  if ! kill -0 "$srv" 2>/dev/null; then
    echo "server exited" >&2
    cat "$tmp/server.log" >&2
    exit 1
  fi
  sleep 0.2
done
if [ -z "$ticket" ]; then
  echo "no ticket in server log" >&2
  cat "$tmp/server.log" >&2
  exit 1
fi

cat > "$tmp/bot.toml" <<EOF
server = "$ticket"
username = "bot"
password_file = "$tmp/pass"
EOF

"$renews" --config "$tmp/renews.toml" admin discovery-txt slime.onix.computer | grep -q '_iroh-news.slime.onix.computer. IN TXT "ticket=endpoint'
"$news" --config "$tmp/bot.toml" --state-dir "$tmp/state-a" --no-relays groups
printf '%s\n' 'hello from e2e' | "$news" --config "$tmp/bot.toml" --state-dir "$tmp/state-a" --no-relays post \
  --group bots.general --subject 'e2e'

found=""
for _ in $(seq 1 40); do
  out=$("$news" --config "$tmp/bot.toml" --state-dir "$tmp/state-a" --no-relays read bots.general --limit 5)
  if printf '%s' "$out" | grep -q 'hello from e2e'; then
    found=1
    break
  fi
  sleep 0.25
done
if [ -z "$found" ]; then
  echo "posted article was not read back" >&2
  printf '%s\n' "$out" >&2
  exit 1
fi

cat > "$tmp/stranger.toml" <<EOF
server = "$ticket"
username = "bot"
password = "secret"
EOF
if "$news" --config "$tmp/stranger.toml" --state-dir "$tmp/state-b" --no-relays groups; then
  echo "non-allowlisted key was accepted" >&2
  exit 1
fi

if printf '%s\n' 'second' | "$news" --config "$tmp/bot.toml" --state-dir "$tmp/state-a" --no-relays post \
  --group bots.general --subject 'too soon'; then
  echo "rate limit did not reject the second post" >&2
  exit 1
fi

echo "e2e ok"
