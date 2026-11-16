#!/usr/bin/env sh
# Install a prebuilt slowcode binary from GitHub Releases.
#
# Usage:
#   curl -fsSL https://raw.githubusercontent.com/milocooper74575/slowcode/main/install.sh | sh
#   sh install.sh
#   SLOWCODE_VERSION=v0.1.0 sh install.sh
#   SLOWCODE_INSTALL_DIR="$HOME/bin" sh install.sh
#   SLOWCODE_BASE_URL=http://localhost:8000 sh install.sh
#   sh install.sh --uninstall
#   SLOWCODE_UNINSTALL=1 sh install.sh
#
# Supported targets: Linux x86_64/aarch64, macOS x86_64/arm64.
set -eu

repo="${SLOWCODE_REPO:-milocooper74575/slowcode}"

version="${SLOWCODE_VERSION:-latest}"
install_dir="${SLOWCODE_INSTALL_DIR:-$HOME/.local/bin}"

uninstall="${SLOWCODE_UNINSTALL:-}"
case "${1:-}" in
  --uninstall|-u) uninstall=1 ;;
esac

if [ -n "$uninstall" ]; then
  if [ -e "${install_dir}/slowcode" ]; then
    rm -f "${install_dir}/slowcode"
    echo "removed ${install_dir}/slowcode"
  else
    echo "nothing to remove at ${install_dir}/slowcode"
  fi
  echo "note: session data under .slowcode/ and .scratch/ is left untouched"
  exit 0
fi

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
    echo "error: unsupported OS: $os. Download a release manually." >&2
    exit 2
    ;;
esac

archive="slowcode-${target}.tar.gz"
if [ -n "${SLOWCODE_BASE_URL:-}" ]; then
  base_url="${SLOWCODE_BASE_URL%/}"
elif [ "$version" = "latest" ]; then
  base_url="https://github.com/${repo}/releases/latest/download"
else
  base_url="https://github.com/${repo}/releases/download/${version}"
fi

tmp="$(mktemp -d "${TMPDIR:-/tmp}/slowcode-install.XXXXXX")"
trap 'rm -rf "$tmp"' EXIT HUP INT TERM

echo "downloading ${base_url}/${archive}"
curl -fsSL "${base_url}/${archive}" -o "${tmp}/${archive}"
curl -fsSL "${base_url}/${archive}.sha256" -o "${tmp}/${archive}.sha256"

expected="$(tr -d '[:space:]' < "${tmp}/${archive}.sha256")"
if command -v sha256sum >/dev/null 2>&1; then
  actual="$(sha256sum "${tmp}/${archive}" | awk '{print $1}')"
else
  actual="$(shasum -a 256 "${tmp}/${archive}" | awk '{print $1}')"
fi
if [ "$expected" != "$actual" ]; then
  echo "error: sha256 mismatch for ${archive}" >&2
  exit 1
fi

tar -xzf "${tmp}/${archive}" -C "${tmp}"
mkdir -p "$install_dir"
install -m 0755 "${tmp}/slowcode/slowcode" "${install_dir}/slowcode"

echo "installed ${install_dir}/slowcode"
if ! command -v slowcode >/dev/null 2>&1; then
  echo "note: ${install_dir} is not on PATH"
  echo "      add it with: export PATH=\"${install_dir}:\$PATH\""
fi

"${install_dir}/slowcode" --version
