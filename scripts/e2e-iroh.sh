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
watch_pid=""
cleanup() {
  if [ -n "$watch_pid" ]; then
    kill "$watch_pid" 2>/dev/null || true
    wait "$watch_pid" 2>/dev/null || true
  fi
  if [ -n "$srv" ]; then
    kill "$srv" 2>/dev/null || true
    wait "$srv" 2>/dev/null || true
  fi
  rm -rf "$tmp"
}
trap cleanup EXIT

# iroh-news uses a configured SecretSpec provider by default. Keep this run
# out of the developer's provider, config, and state; the SecretSpec section
# below names its own providers.
export HOME="$tmp/home" XDG_CONFIG_HOME="$tmp/home/.config" XDG_STATE_HOME="$tmp/home/.local/state"
unset SECRETSPEC_PROVIDER SECRETSPEC_PROFILE SECRETSPEC_SCOPE
mkdir -p "$HOME"

mkdir -p "$tmp/state-a" "$tmp/state-b" "$tmp/state-admin" "$tmp/state-join" "$tmp/state-watch"
id_a=$("$news" --state-dir "$tmp/state-a" id)
id_b=$("$news" --state-dir "$tmp/state-b" id)
id_admin=$("$news" --state-dir "$tmp/state-admin" id)
id_watch=$("$news" --state-dir "$tmp/state-watch" id)
peer_a=$(printf '%s' "$id_a" | sed -n 's/.*"endpoint_id":"\([^"]*\)".*/\1/p')
peer_admin=$(printf '%s' "$id_admin" | sed -n 's/.*"endpoint_id":"\([^"]*\)".*/\1/p')
peer_watch=$(printf '%s' "$id_watch" | sed -n 's/.*"endpoint_id":"\([^"]*\)".*/\1/p')
test -n "$peer_a"
test -n "$peer_admin"
test -n "$peer_watch"

printf '%s\n' 'secret' > "$tmp/pass"
chmod 600 "$tmp/pass"

cat > "$tmp/renews.toml" <<EOF
addr = "127.0.0.1:11119"
site_name = "bots.local"
db_path = "sqlite://$tmp/news.db"
auth_db_path = "sqlite://$tmp/auth.db"
peer_db_path = "sqlite://$tmp/peers.db"
bots_can_create_groups = true
group_prefixes = ["bots."]
group_creates_per_day = 1
max_groups = 200

[iroh]
enabled = true
key_path = "$tmp/iroh.key"
localhost_tcp = true
bind_addr = "127.0.0.1:0"
wait_for_relay = false
allowed_peers = ["$peer_a", "$peer_watch"]
admin_peers = ["$peer_admin"]

[iroh.join_requests]
enable = true

[user_limits]
post_rate_per_minute = 1
post_burst = 1
EOF

"$renews" --config "$tmp/renews.toml" --init
"$renews" --config "$tmp/renews.toml" admin add-group bots.general
"$renews" --config "$tmp/renews.toml" admin add-group bots.announce
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

endpoint_id=$(sed -n 's/^iroh endpoint id: //p' "$tmp/server.log")
test -n "$endpoint_id"
record=$("$renews" --config "$tmp/renews.toml" admin discovery-txt slime.onix.computer)
printf '%s\n' "$record" | grep -q "_iroh-news.slime.onix.computer. IN TXT \"id=${endpoint_id}\""
printf '%s\n' "$record" | grep -qx "id=${endpoint_id}"
"$renews" --config "$tmp/renews.toml" admin discovery-txt slime.onix.computer --ticket \
  | grep -q "_iroh-news.slime.onix.computer. IN TXT \"ticket=${ticket}\""
"$news" --config "$tmp/bot.toml" --state-dir "$tmp/state-a" --no-relays groups
cat > "$tmp/watch.toml" <<EOF
server = "$ticket"
username = "bot"
password_file = "$tmp/pass"
EOF
"$news" --config "$tmp/watch.toml" --state-dir "$tmp/state-watch" --no-relays watch bots.general \
  > "$tmp/watch.out" 2> "$tmp/watch.err" &
watch_pid=$!
ready=""
for _ in $(seq 1 50); do
  if grep -q 'via gossip' "$tmp/watch.err"; then
    ready=1
    break
  fi
  if ! kill -0 "$watch_pid" 2>/dev/null; then
    echo "watch exited before joining" >&2
    cat "$tmp/watch.err" >&2
    exit 1
  fi
  sleep 0.1
done
if [ -z "$ready" ]; then
  echo "watch did not join gossip" >&2
  cat "$tmp/watch.err" >&2
  exit 1
fi
start_ns=$(date +%s%N)
printf '%s\n' 'hello from e2e' | "$news" --config "$tmp/bot.toml" --state-dir "$tmp/state-a" --no-relays post \
  --group bots.general --subject 'e2e'
seen=""
for _ in $(seq 1 20); do
  if grep -q 'hello from e2e' "$tmp/watch.out"; then
    seen=1
    break
  fi
  sleep 0.1
done
end_ns=$(date +%s%N)
if [ -z "$seen" ]; then
  echo "watch did not receive the article via gossip" >&2
  cat "$tmp/watch.err" "$tmp/watch.out" >&2
  exit 1
fi
elapsed_ms=$(( (end_ns - start_ns) / 1000000 ))
echo "gossip latency ms: ${elapsed_ms}"
if [ "$elapsed_ms" -ge 2000 ]; then
  echo "gossip notification took ${elapsed_ms}ms" >&2
  exit 1
fi
if grep -q 'polling every' "$tmp/watch.err"; then
  echo "watch fell back to polling" >&2
  exit 1
fi

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

"$news" --config "$tmp/bot.toml" --state-dir "$tmp/state-a" --no-relays group create bots.widgets --description 'widget notes'
"$news" --config "$tmp/bot.toml" --state-dir "$tmp/state-a" --no-relays groups | grep -q '"creator":"bot"'
if "$news" --config "$tmp/bot.toml" --state-dir "$tmp/state-a" --no-relays group create bots.other --description 'second'; then
  echo "group creation rate limit did not trip" >&2
  exit 1
fi
if "$news" --config "$tmp/bot.toml" --state-dir "$tmp/state-a" --no-relays group create general --description 'nope'; then
  echo "group outside bots. was accepted" >&2
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

joined=$("$news" --state-dir "$tmp/state-join" --no-relays --server "$ticket" join --name newbot --owner Ada --json)
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

mkdir -p "$tmp/state-fresh" "$tmp/state-spoof"
"$news" --state-dir "$tmp/state-fresh" --no-relays --server "$ticket" join --name joiner --owner Ada --json > "$tmp/fresh.json"
grep -q '"status":"pending"' "$tmp/fresh.json"
fresh_id=$(sed -n 's/.*"endpoint_id":"\([^"]*\)".*/\1/p' "$tmp/fresh.json")
cat > "$tmp/fresh.toml" <<EOF
server = "$ticket"
username = "joiner"
EOF
if "$news" --config "$tmp/fresh.toml" --state-dir "$tmp/state-fresh" --no-relays groups; then
  echo "pending join was allowed onto NNTP" >&2
  exit 1
fi
if "$news" --config "$tmp/admin.toml" --state-dir "$tmp/state-fresh" --no-relays admin status; then
  echo "pending join was allowed onto the admin channel" >&2
  exit 1
fi
if "$news" --config "$tmp/fresh.toml" --state-dir "$tmp/state-fresh" --no-relays group create bots.sneak --description 'nope'; then
  echo "pending peer created a group" >&2
  exit 1
fi
python3 - "$news" "$tmp/admin.toml" "$tmp/state-admin" "$fresh_id" <<'PY'
import json, subprocess, sys
news, config, state, endpoint_id = sys.argv[1:]
proc = subprocess.Popen(
    [news, "--config", config, "--state-dir", state, "--no-relays", "mcp", "--admin"],
    stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
)
def send(obj):
    proc.stdin.write(json.dumps(obj) + "\n")
    proc.stdin.flush()
def read():
    return json.loads(proc.stdout.readline())
send({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"e2e","version":"0"}}})
read()
send({"jsonrpc":"2.0","method":"notifications/initialized"})
send({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"admin_approve_join_request","arguments":{"endpoint_id": endpoint_id}}})
approved = read()
text = approved["result"]["content"][0]["text"]
assert "approved" in text, text
assert "password" not in text
proc.stdin.close()
proc.wait(timeout=20)
print("approve ok")
PY
"$news" --config "$tmp/fresh.toml" --state-dir "$tmp/state-fresh" --no-relays groups | grep -q 'bots.widgets'
printf '%s\n' 'saw the new group' | "$news" --config "$tmp/fresh.toml" --state-dir "$tmp/state-fresh" --no-relays post --group bots.widgets --subject 'from joiner'
if "$news" --state-dir "$tmp/state-spoof" --no-relays --server "$ticket" join --name joiner --owner Eve --json; then
  echo "second endpoint took an approved name" >&2
  exit 1
fi

# SecretSpec: a generated account password goes to the admin's provider, not
# a file, and never into the command output.
cat > "$tmp/admin-spec.toml" <<EOF
server = "$ticket"
secretspec = "dotenv://$tmp/admin-secrets.env"
secretspec_project = "e2e-admin"
EOF
created_spec=$("$news" --config "$tmp/admin-spec.toml" --state-dir "$tmp/state-admin" --no-relays admin create-account --username spec-bot)
printf '%s\n' "$created_spec" | grep -q '"password_secret":"IROH_NEWS_ACCOUNT_SPEC_BOT"'
printf '%s\n' "$created_spec" | grep -q '"secretspec_project":"e2e-admin"'
if printf '%s\n' "$created_spec" | grep -q '"password_file"'; then
  echo "create-account wrote a password file instead of SecretSpec" >&2
  exit 1
fi
spec_pass=$("$news" --config "$tmp/admin-spec.toml" --state-dir "$tmp/state-admin" secrets account-password --username spec-bot \
  | sed -n 's/.*"password":"\([^"]*\)".*/\1/p')
test -n "$spec_pass"
if printf '%s\n' "$created_spec" | grep -qF -- "$spec_pass"; then
  echo "create-account printed the password" >&2
  exit 1
fi

# No secretspec setting: SECRETSPEC_PROVIDER alone puts the bot's key and
# password in the provider, and no key file is written.
spec_bot() {
  SECRETSPEC_PROVIDER="dotenv://$tmp/bot-secrets.env" "$news" --state-dir "$tmp/state-spec" "$@"
}
mkdir -p "$tmp/state-spec"
cat > "$tmp/spec-bot.toml" <<EOF
server = "$ticket"
username = "spec-bot"
EOF
cat > "$tmp/spec-status.toml" <<EOF
server = "$ticket"
EOF
spec_id=$(spec_bot --config "$tmp/spec-bot.toml" id | sed -n 's/.*"endpoint_id":"\([^"]*\)".*/\1/p')
test -n "$spec_id"
test ! -e "$tmp/state-spec/secret_key"
grep -q '^IROH_NEWS_SECRET_KEY=' "$tmp/bot-secrets.env"
test "$(spec_bot --config "$tmp/spec-bot.toml" id | sed -n 's/.*"endpoint_id":"\([^"]*\)".*/\1/p')" = "$spec_id"
printf '%s\n' "$spec_pass" | spec_bot --config "$tmp/spec-bot.toml" secrets set-password > /dev/null
"$news" --config "$tmp/admin.toml" --state-dir "$tmp/state-admin" --no-relays admin add-peer --endpoint-id "$spec_id" --label spec-bot
printf '%s\n' 'password from secretspec' | spec_bot --config "$tmp/spec-bot.toml" --no-relays post \
  --group bots.general --subject 'from spec-bot'
spec_status=$(spec_bot --config "$tmp/spec-status.toml" status)
printf '%s\n' "$spec_status" | grep -q '"key":"secretspec"'
printf '%s\n' "$spec_status" | grep -q '"password_configured":true'

echo "e2e ok"
