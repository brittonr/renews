#!/bin/sh
# Install the latest iroh-news release binary into ~/.local/bin.
# Verifies the sha256 published next to the binary. No Nix required.
set -eu

repo="${IROH_NEWS_REPO:-brittonr/renews}"
dest_dir="${HOME}/.local/bin"
dest="${dest_dir}/iroh-news"

os=$(uname -s)
arch=$(uname -m)
case "${os}-${arch}" in
  Linux-x86_64) target="x86_64-unknown-linux-musl" ;;
  Linux-aarch64 | Linux-arm64) target="aarch64-unknown-linux-musl" ;;
  Darwin-arm64) target="aarch64-apple-darwin" ;;
  Darwin-x86_64) target="x86_64-apple-darwin" ;;
  *)
    echo "install-iroh-news: unsupported platform ${os}-${arch}" >&2
    exit 1
    ;;
esac

asset="iroh-news-${target}"
base="https://github.com/${repo}/releases/latest/download"
tmpdir=$(mktemp -d)
trap 'rm -rf "$tmpdir"' EXIT

echo "downloading ${asset} from ${repo}"
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
echo "installed ${dest}"
echo "endpoint id:"
"$dest" id
