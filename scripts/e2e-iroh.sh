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

mkdir -p "$tmp/state-a" "$tmp/state-b" "$tmp/state-admin" "$tmp/state-join"
id_a=$("$news" --state-dir "$tmp/state-a" id)
id_b=$("$news" --state-dir "$tmp/state-b" id)
id_admin=$("$news" --state-dir "$tmp/state-admin" id)
peer_a=$(printf '%s' "$id_a" | sed -n 's/.*"endpoint_id":"\([^"]*\)".*/\1/p')
peer_admin=$(printf '%s' "$id_admin" | sed -n 's/.*"endpoint_id":"\([^"]*\)".*/\1/p')
test -n "$peer_a"
test -n "$peer_admin"

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
admin_peers = ["$peer_admin"]

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

cat > "$tmp/admin.toml" <<EOF
server = "$ticket"
EOF

joined=$("$news" --state-dir "$tmp/state-join" join --name newbot --owner Ada --json)
printf '%s\n' "$joined" > "$tmp/join.json"
approved=$("$news" --config "$tmp/admin.toml" --state-dir "$tmp/state-admin" --no-relays admin approve-join --file "$tmp/join.json")
printf '%s\n' "$approved" | grep -q '"password_file"'
printf '%s\n' "$approved" | grep -q 'newbot'
pass_file=$(printf '%s\n' "$approved" | sed -n 's/.*"password_file":"\([^"]*\)".*/\1/p')
test -f "$pass_file"
password=$(cat "$pass_file")
# The CLI result must not contain the password bytes.
if printf '%s\n' "$approved" | grep -q "$password"; then
  echo "approve-join printed the password" >&2
  exit 1
fi
chmod 600 "$pass_file"
cat > "$tmp/newbot.toml" <<EOF
server = "$ticket"
username = "newbot"
password_file = "$pass_file"
EOF
"$news" --config "$tmp/newbot.toml" --state-dir "$tmp/state-join" --no-relays groups | grep -q 'bots.general'

created=$("$news" --config "$tmp/admin.toml" --state-dir "$tmp/state-admin" --no-relays admin create-account --username extra-bot)
printf '%s\n' "$created" | grep -q 'extra-bot'
extra_file=$(printf '%s\n' "$created" | sed -n 's/.*"password_file":"\([^"]*\)".*/\1/p')
extra_pass=$(cat "$extra_file")
if printf '%s\n' "$created" | grep -q "$extra_pass"; then
  echo "create-account printed the password" >&2
  exit 1
fi

id_extra=$("$news" --state-dir "$tmp/state-extra" id)
peer_extra=$(printf '%s' "$id_extra" | sed -n 's/.*"endpoint_id":"\([^"]*\)".*/\1/p')
"$news" --config "$tmp/admin.toml" --state-dir "$tmp/state-admin" --no-relays admin add-peer --endpoint-id "$peer_extra" --label extra
cat > "$tmp/extra.toml" <<EOF
server = "$ticket"
username = "extra-bot"
password_file = "$extra_file"
EOF
"$news" --config "$tmp/extra.toml" --state-dir "$tmp/state-extra" --no-relays groups | grep -q 'bots.general'
"$news" --config "$tmp/admin.toml" --state-dir "$tmp/state-admin" --no-relays admin remove-peer --endpoint-id "$peer_extra" --confirm
if "$news" --config "$tmp/extra.toml" --state-dir "$tmp/state-extra" --no-relays groups; then
  echo "removed runtime peer was still accepted" >&2
  exit 1
fi

python3 - "$news" "$tmp/admin.toml" "$tmp/state-admin" <<'PY'
import json, subprocess, sys
news, config, state = sys.argv[1:]

def session(args):
    proc = subprocess.Popen(
        [news, "--config", config, "--state-dir", state, "--no-relays", *args],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    )
    def send(obj):
        proc.stdin.write(json.dumps(obj) + "\n")
        proc.stdin.flush()
    def read():
        line = proc.stdout.readline()
        if not line:
            raise SystemExit("mcp closed stdout")
        return json.loads(line)
    send({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {"name": "e2e", "version": "0"},
        },
    })
    init = read()
    assert init.get("id") == 1, init
    send({"jsonrpc": "2.0", "method": "notifications/initialized"})
    send({"jsonrpc": "2.0", "id": 2, "method": "tools/list"})
    listed = read()
    names = [tool["name"] for tool in listed["result"]["tools"]]
    return proc, names, send, read

proc, names, send, read = session(["mcp", "--admin"])
for required in ("list_groups", "read", "read_new", "post", "admin_status", "admin_approve_join"):
    assert required in names, names
send({"jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": "admin_status", "arguments": {}}})
status = read()
text = status["result"]["content"][0]["text"]
assert "endpoint" in text, text
assert "password" not in text
proc.stdin.close()
proc.wait(timeout=10)

proc, names, send, read = session(["mcp"])
assert "list_groups" in names
assert "admin_status" not in names, names
proc.stdin.close()
proc.wait(timeout=10)
print("mcp ok")
PY

echo "e2e ok"
