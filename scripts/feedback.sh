#!/usr/bin/env bash
# Build a GitHub-issue-ready feedback bundle from one durable slowcode session.
#
# The session id is the trace id. Run slowcode with `--session new` so the run
# has a journal, then pass the printed id to this script.
#
# This is a thin wrapper around `slowcode feedback`. It resolves the binary from
# SLOWCODE_BIN, PATH, or the local target directory, so the packaging and
# completeness checks live in one implementation.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

BIN="${SLOWCODE_BIN:-}"
if [[ -z "$BIN" ]] && command -v slowcode >/dev/null 2>&1; then
  BIN="$(command -v slowcode)"
fi
if [[ -z "$BIN" && -x "$ROOT/target/release/slowcode" ]]; then
  BIN="$ROOT/target/release/slowcode"
fi
if [[ -z "$BIN" && -x "$ROOT/target/debug/slowcode" ]]; then
  BIN="$ROOT/target/debug/slowcode"
fi
if [[ -z "$BIN" ]]; then
  echo "error: slowcode binary not found" >&2
  echo "build it with 'cargo build -p slowcode-cli', put it on PATH, or set SLOWCODE_BIN" >&2
  exit 127
fi

exec "$BIN" feedback "$@"
