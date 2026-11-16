#!/usr/bin/env bash
# MCP install shorthand acceptance.
#
# Drives the real TUI in a macOS PTY through:
#   /mcp add ChromeDevTools/chrome-devtools-mcp
#   y (run connection test) -> y (save)
# then asserts the written config and the refreshed tool snapshot.
#
# Requires DEEPSEEK_API_KEY because the TUI builds its provider at startup.
# The MCP handshake itself does not call the model.

set -euo pipefail

cd "$(dirname "$0")/.."

if [ "$(uname -s)" != "Darwin" ]; then
  echo "e2e-mcp.sh currently targets macOS script(1)" >&2
  exit 2
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "error: python3 is required" >&2
  exit 2
fi

if [ -z "${DEEPSEEK_API_KEY:-}" ] && [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi
if [ -z "${DEEPSEEK_API_KEY:-}" ]; then
  echo "error: DEEPSEEK_API_KEY is required for TUI startup" >&2
  exit 2
fi

if [ "${SLOWCODE_E2E_BUILD:-0}" = "1" ]; then
  cargo build -p slowcode-cli
fi
BIN="${SLOWCODE_E2E_BIN:-target/debug/slowcode}"
if [ ! -x "$BIN" ]; then
  echo "error: $BIN is missing; run cargo build -p slowcode-cli" >&2
  exit 2
fi

TMP="${TMPDIR:-/tmp}/slowcode-e2e-mcp.$$"
WORKSPACE="$TMP/workspace"
SESSIONS="$TMP/sessions"
HOME_DIR="$TMP/home"
TRANSCRIPT="$TMP/transcript.txt"
SCREEN="$TMP/screen.txt"
INPUT="$TMP/input.fifo"
mkdir -p "$WORKSPACE" "$SESSIONS" "$HOME_DIR"
chmod 700 "$TMP" "$WORKSPACE" "$SESSIONS" "$HOME_DIR"
mkfifo "$INPUT"

cleanup() {
  exec 3>&- 2>/dev/null || true
  if [ -n "${RUNNER:-}" ]; then
    kill "$RUNNER" 2>/dev/null || true
  fi
  if [ "${SLOWCODE_E2E_KEEP:-0}" = "1" ]; then
    echo "kept: $TMP" >&2
  else
    rm -rf "$TMP"
  fi
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  if [ -f "$TRANSCRIPT" ]; then
    python3 scripts/ansi-screen.py "$TRANSCRIPT" 60 120 >"$SCREEN" 2>/dev/null || true
    cat "$SCREEN" >&2 || true
    echo "--- transcript matches ---" >&2
    grep -a -E 'MCP|mcp|error|testing|resolving' "$TRANSCRIPT" | tail -20 >&2 || true
  fi
  exit 1
}

wait_for() {
  local needle="$1"
  local seconds="$2"
  local index=0
  while [ "$index" -lt "$seconds" ]; do
    if grep -a -qF "$needle" "$TRANSCRIPT"; then
      return 0
    fi
    sleep 1
    index=$((index + 1))
  done
  fail "timed out waiting for: $needle"
}

env \
  HOME="$HOME_DIR" \
  DSH_SESSION_ROOT="$SESSIONS" \
  DEEPSEEK_API_KEY="$DEEPSEEK_API_KEY" \
  script -q /dev/null sh -c "stty rows 60 cols 120; exec '$BIN' --workspace '$WORKSPACE'" \
  < "$INPUT" > "$TRANSCRIPT" 2>&1 &
RUNNER=$!
exec 3>"$INPUT"

wait_for 'slowcode TUI' 30
sleep 2
printf '/mcp add ChromeDevTools/chrome-devtools-mcp\r' >&3
sleep 1
# A second Enter is harmless after submit and commits the line if the first
# keystroke arrived before raw mode was ready.
printf '\r' >&3
wait_for 'Run connection test' 60
printf 'y\r' >&3
sleep 1
printf '\r' >&3
wait_for 'Save? [y/n]' 120
printf 'y\r' >&3
sleep 1
printf '\r' >&3
wait_for 'tool(s) from 1 server(s)' 120

CONFIG="$WORKSPACE/.slowcode/mcp.json"
if [ ! -f "$CONFIG" ]; then
  fail "missing $CONFIG"
fi
for needle in '"chrome-devtools"' '"repo": "ChromeDevTools/chrome-devtools-mcp"' '"package": "chrome-devtools-mcp"' '"version":' '"integrity":'; do
  if ! grep -qF "$needle" "$CONFIG"; then
    fail "config is missing $needle"
  fi
done

printf '/quit\r' >&3
wait "$RUNNER" 2>/dev/null || true
exec 3>&-

TRUST="$HOME_DIR/.slowcode/mcp-trust.json"
if [ ! -f "$TRUST" ]; then
  fail "activation trust was not remembered in $TRUST"
fi

# Restart: the project server must be dormant, then activate from cached trust.
INPUT2="$TMP/input2.fifo"
TRANSCRIPT2="$TMP/transcript2.txt"
mkfifo "$INPUT2"
env \
  HOME="$HOME_DIR" \
  DSH_SESSION_ROOT="$SESSIONS" \
  DEEPSEEK_API_KEY="$DEEPSEEK_API_KEY" \
  script -q /dev/null sh -c "stty rows 60 cols 120; exec '$BIN' --workspace '$WORKSPACE'" \
  < "$INPUT2" > "$TRANSCRIPT2" 2>&1 &
RUNNER=$!
exec 3>"$INPUT2"
TRANSCRIPT="$TRANSCRIPT2"

wait_for 'slowcode TUI' 30
sleep 2
printf '/mcp\r' >&3
wait_for '[project] dormant' 30
printf '/mcp enable chrome-devtools\r' >&3
sleep 1
printf '\r' >&3
wait_for 'MCP server enabled: chrome-devtools' 30
wait_for 'tool(s) from 1 server(s)' 120
printf '/quit\r' >&3
sleep 2

echo "PASS: /mcp add ChromeDevTools/chrome-devtools-mcp"
