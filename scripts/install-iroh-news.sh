#!/bin/sh
# Install the latest iroh-news release binary into ~/.local/bin.
# Verifies the sha256 published next to the binary. No Nix required.
#
#   curl -fsSL https://raw.githubusercontent.com/brittonr/renews/main/scripts/install-iroh-news.sh | sh -s -- --join <name>
#
# --join (or IROH_NEWS_JOIN_NAME) runs `iroh-news join` after install and
# prints the block to paste to Britton. Nothing is sent over the network
# except the binary download.
set -eu

repo="${IROH_NEWS_REPO:-brittonr/renews}"
dest_dir="${HOME}/.local/bin"
dest="${dest_dir}/iroh-news"
join_name="${IROH_NEWS_JOIN_NAME:-}"

while [ $# -gt 0 ]; do
  case "$1" in
    --join)
      if [ $# -lt 2 ] || [ -z "$2" ]; then
        echo "install-iroh-news: --join needs a username" >&2
        exit 1
      fi
      join_name=$2
      shift 2
      ;;
    --join=*)
      join_name=${1#--join=}
      if [ -z "$join_name" ]; then
        echo "install-iroh-news: --join needs a username" >&2
        exit 1
      fi
      shift
      ;;
    *)
      echo "install-iroh-news: unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

say() {
  if [ -n "$join_name" ]; then
    echo "$@" >&2
  else
    echo "$@"
  fi
}

os=$(uname -s)
arch=$(uname -m)
case "${os}-${arch}" in
  Linux-x86_64) target="x86_64-unknown-linux-musl" ;;
  Darwin-x86_64) target="x86_64-apple-darwin" ;;
  *)
    echo "install-iroh-news: no release binary for ${os}-${arch}." >&2
    echo "Use Nix: nix run github:brittonr/renews#iroh-news -- id" >&2
    echo "Or: cargo install --path crates/iroh-news   # from a git checkout" >&2
    exit 1
    ;;
esac

asset="iroh-news-${target}"
base="https://github.com/${repo}/releases/latest/download"
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

say "downloading ${asset} from ${repo}"
curl -fsSL "${base}/${asset}" -o "${tmpdir}/iroh-news"
curl -fsSL "${base}/${asset}.sha256" -o "${tmpdir}/iroh-news.sha256"

expected=$(awk '{print $1}' "${tmpdir}/iroh-news.sha256")
if command -v sha256sum >/dev/null 2>&1; then
  actual=$(sha256sum "${tmpdir}/iroh-news" | awk '{print $1}')
elif command -v shasum >/dev/null 2>&1; then
  actual=$(shasum -a 256 "${tmpdir}/iroh-news" | awk '{print $1}')
else
  echo "install-iroh-news: need sha256sum or shasum" >&2
  exit 1
fi

if [ "$expected" != "$actual" ]; then
  echo "install-iroh-news: checksum mismatch" >&2
  echo "  expected ${expected}" >&2
  echo "  actual   ${actual}" >&2
  exit 1
fi

mkdir -p "$dest_dir"
install -m 0755 "${tmpdir}/iroh-news" "$dest"
say "installed ${dest}"
if [ -n "$join_name" ]; then
  "$dest" join --name "$join_name"
else
  echo "endpoint id:"
  "$dest" id
fi
