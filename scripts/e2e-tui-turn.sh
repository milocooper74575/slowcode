#!/usr/bin/env bash
# Real-model TUI acceptance: streaming turn, inline approval and clean exit.
#
# This is slower and consumes provider tokens. It complements scripts/e2e-tui.sh,
# which only checks startup and inline commands with a dummy key.

set -euo pipefail

cd "$(dirname "$0")/.."

if [ "$(uname -s)" != "Darwin" ]; then
  echo "e2e-tui-turn.sh currently targets macOS script(1)" >&2
  exit 2
fi
if ! command -v python3 >/dev/null 2>&1; then
  echo "error: python3 is required to render the PTY screen" >&2
  exit 2
fi

if [ -z "${DEEPSEEK_API_KEY:-}" ] && [ -f .env ]; then
  set -a
  # shellcheck disable=SC1091
  . ./.env
  set +a
fi
if [ -z "${DEEPSEEK_API_KEY:-}" ]; then
  echo "error: DEEPSEEK_API_KEY is required" >&2
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

TMP="${TMPDIR:-/tmp}/slowcode-e2e-tui-turn.$$"
WORKSPACE="$TMP/workspace"
SESSIONS="$TMP/sessions"
TRANSCRIPT="$TMP/transcript.txt"
SCREEN="$TMP/screen.txt"
INPUT="$TMP/input.fifo"
ROWS=40
COLS=120
mkdir -p "$WORKSPACE" "$SESSIONS"
chmod 700 "$TMP" "$WORKSPACE" "$SESSIONS"
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

env \
  DSH_SESSION_ROOT="$SESSIONS" \
  DEEPSEEK_API_KEY="$DEEPSEEK_API_KEY" \
  script -q /dev/null sh -c "stty rows $ROWS cols $COLS; exec '$BIN' --ask --workspace '$WORKSPACE'" \
  < "$INPUT" > "$TRANSCRIPT" 2>&1 &
RUNNER=$!
exec 3>"$INPUT"

refresh_screen() {
  python3 scripts/ansi-screen.py "$TRANSCRIPT" "$ROWS" "$COLS" > "$SCREEN"
}

wait_for() {
  local needle="$1"
  local attempts="${2:-100}"
  local index=0
  while [ "$index" -lt "$attempts" ]; do
    refresh_screen
    if grep -aq "$needle" "$SCREEN"; then
      return 0
    fi
    sleep 0.1
    index=$((index + 1))
  done
  echo "timeout waiting for: $needle" >&2
  return 1
}

send() {
  printf '%s\r' "$1" >&3
}

wait_for "Type a task" 100
send "Use the write tool to create tui.txt containing exactly hi. Do not do anything else."
wait_for "approval: write" 900
echo "PASS approval prompt"
send "d"
sleep 1
send "y"

FILE_READY=0
for _ in $(seq 1 300); do
  if [ -f "$WORKSPACE/tui.txt" ]; then
    FILE_READY=1
    break
  fi
  sleep 0.1
done
if [ "$FILE_READY" -ne 1 ]; then
  echo "FAIL tui.txt was not created" >&2
  exit 1
fi

content="$(cat "$WORKSPACE/tui.txt")"
if [ "$content" != "hi" ]; then
  echo "FAIL tui.txt content: $content" >&2
  exit 1
fi
echo "PASS tui.txt content"

refresh_screen
if grep -aq "approval" "$SCREEN"; then
  echo "PASS approval inline text"
else
  echo "FAIL approval inline text" >&2
  exit 1
fi

# Ctrl-C exits immediately; /quit would run the handoff model turn first.
printf '\003' >&3
exec 3>&-

STATUS=0
for _ in $(seq 1 100); do
  if ! kill -0 "$RUNNER" 2>/dev/null; then
    break
  fi
  sleep 0.1
done
kill "$RUNNER" 2>/dev/null || true
wait "$RUNNER" 2>/dev/null || STATUS=$?

if [ "$STATUS" -ne 0 ]; then
  echo "FAIL TUI did not exit cleanly (status $STATUS)" >&2
  exit 1
fi

echo "e2e-tui-turn PASS"
