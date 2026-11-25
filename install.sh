#!/bin/sh
# Installs nodoka: downloads the release for this machine, checks its SHA-256, and sets it up.
#   curl -fsSL https://raw.githubusercontent.com/sundarshahi/nodoka/main/install.sh | sh
# Pick a version with NODOKA_VERSION=v0.1.0.
set -eu

repo="${NODOKA_REPO:-sundarshahi/nodoka}"
version="${NODOKA_VERSION:-latest}"

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-musl ;;
  MINGW* | MSYS* | CYGWIN*) echo "On Windows, run this in PowerShell instead: irm https://raw.githubusercontent.com/$repo/main/install.ps1 | iex" >&2; exit 1 ;;
  *) echo "nodoka runs on macOS, Linux (WSL 2 too) and Windows." >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64 | amd64) arch=x86_64 ;;
  arm64 | aarch64) arch=aarch64 ;;
  *) echo "nodoka has no release for this CPU ($(uname -m)). Build it with: cargo install --git https://github.com/$repo" >&2; exit 1 ;;
esac

file="nodoka-$arch-$os.tar.gz"
if [ "$version" = latest ]; then
  base="https://github.com/$repo/releases/latest/download"
else
  base="https://github.com/$repo/releases/download/$version"
fi
base="${NODOKA_BASE_URL:-$base}" # a mirror, or a local folder for testing

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT INT TERM

echo "Downloading $file…"
curl -fsSL "$base/$file" -o "$tmp/$file"
curl -fsSL "$base/$file.sha256" -o "$tmp/$file.sha256"
if command -v shasum >/dev/null 2>&1; then
  (cd "$tmp" && shasum -a 256 -c "$file.sha256" >/dev/null)
else
  (cd "$tmp" && sha256sum -c "$file.sha256" >/dev/null)
fi || { echo "The download's checksum doesn't match. Nothing was installed." >&2; exit 1; }

tar -xzf "$tmp/$file" -C "$tmp"
"$tmp/nodoka" install
