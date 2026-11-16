#!/usr/bin/env sh
# Build slowcode for this host and package it into a local release directory
# that install.sh can consume without GitHub.
#
# Usage:
#   sh release.sh
#   SLOWCODE_DIST_DIR=/tmp/slowcode-dist sh release.sh
#
# Then install from the local bundle:
#   SLOWCODE_BASE_URL="file://$PWD/dist" \
#   SLOWCODE_INSTALL_DIR="$HOME/.local/bin" \
#   sh install.sh
set -eu

repo_root="$(cd "$(dirname "$0")" && pwd)"
cd "$repo_root"

dist="${SLOWCODE_DIST_DIR:-$repo_root/dist}"
case "$dist" in
  /*) ;;
  *) dist="$repo_root/$dist" ;;
esac

os="$(uname -s)"
arch="$(uname -m)"

case "$os" in
  Darwin)
    case "$arch" in
      arm64) target="aarch64-apple-darwin" ;;
      x86_64) target="x86_64-apple-darwin" ;;
      *) echo "error: unsupported macOS arch: $arch" >&2; exit 2 ;;
    esac
    ;;
  Linux)
    case "$arch" in
      aarch64|arm64) target="aarch64-unknown-linux-musl" ;;
      x86_64|amd64) target="x86_64-unknown-linux-musl" ;;
      *) echo "error: unsupported Linux arch: $arch" >&2; exit 2 ;;
    esac
    ;;
  *)
    echo "error: unsupported OS: $os. Build a release manually." >&2
    exit 2
    ;;
esac

echo "building slowcode (release)"
cargo build --locked --release -p slowcode-cli

binary="target/release/slowcode"
if [ ! -f "$binary" ]; then
  echo "error: $binary was not produced" >&2
  exit 1
fi

stage="$dist/slowcode"
archive="slowcode-${target}.tar.gz"
rm -rf "$stage"
mkdir -p "$stage"
cp "$binary" "$stage/slowcode"
for extra in LICENSE README.md SECURITY.md; do
  if [ -f "$extra" ]; then
    cp "$extra" "$stage/"
  fi
done

tar -C "$dist" -czf "$dist/$archive" slowcode
if command -v sha256sum >/dev/null 2>&1; then
  sha256sum "$dist/$archive" | awk '{print $1}' > "$dist/$archive.sha256"
else
  shasum -a 256 "$dist/$archive" | awk '{print $1}' > "$dist/$archive.sha256"
fi

echo "packaged $dist/$archive"
echo "packaged $dist/$archive.sha256"
echo
echo "install locally with:"
echo "  SLOWCODE_BASE_URL=\"file://$dist\" \\"
echo "  SLOWCODE_INSTALL_DIR=\"\$HOME/.local/bin\" \\"
echo "  sh install.sh"
