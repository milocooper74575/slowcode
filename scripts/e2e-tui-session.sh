#!/usr/bin/env bash
# Real-model acceptance for /session switch and --resume history replay.
#
# This spends provider tokens. It creates a throwaway workspace, runs one short
# turn, switches away and back, then restarts with --resume and checks the
# assistant message is visible in both cases.

set -euo pipefail
trap '' PIPE

cd "$(dirname "$0")/.."

if [ "$(uname -s)" != "Darwin" ]; then
  echo "e2e-tui-session.sh currently targets macOS script(1)" >&2
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

TMP="${TMPDIR:-/tmp}/slowcode-e2e-session.$$"
WORKSPACE="$TMP/ws"
SESSIONS="$TMP/sessions"
TRANSCRIPT="$TMP/transcript.txt"
SCREEN="$TMP/screen.txt"
INPUT="$TMP/input.fifo"
RESUME_TRANSCRIPT="$TMP/resume.txt"
RESUME_SCREEN="$TMP/resume-screen.txt"
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

refresh_screen() {
  python3 scripts/ansi-screen.py "$TRANSCRIPT" 40 120 >"$SCREEN"
}

wait_for() {
  local needle="$1"
  local attempts="${2:-150}"
  local index=0
  while [ "$index" -lt "$attempts" ]; do
    refresh_screen
    if grep -aq "$needle" "$SCREEN"; then
      return 0
    fi
    sleep 0.2
    index=$((index + 1))
  done
  echo "timeout waiting for: $needle" >&2
  return 1
}

env \
  DSH_SESSION_ROOT="$SESSIONS" \
  DEEPSEEK_API_KEY="$DEEPSEEK_API_KEY" \
  script -q /dev/null sh -c "stty rows 40 cols 120; exec '$BIN' --workspace '$WORKSPACE'" \
  <"$INPUT" >"$TRANSCRIPT" 2>&1 &
RUNNER=$!
exec 3>"$INPUT"

sleep 3
printf 'Reply with exactly HISTORY_MARK and nothing else.\r' >&3 || true
wait_for "HISTORY_MARK"
wait_for "idle"

S1="$(basename "$(ls "$SESSIONS"/*.jsonl | head -n 1)" .jsonl)"
printf '/session new\r' >&3 || true
wait_for "new session"
printf '/session switch %s\r' "$S1" >&3 || true
wait_for "session: $S1"
refresh_screen
if ! grep -aq "HISTORY_MARK" "$SCREEN"; then
  echo "FAIL switched session did not replay history" >&2
  exit 1
fi
echo "PASS /session switch replayed history"

printf '\003' >&3 || true
exec 3>&-
wait "$RUNNER" 2>/dev/null || true
RUNNER=""

(
  sleep 5
  printf '\003'
) | env \
  DSH_SESSION_ROOT="$SESSIONS" \
  DEEPSEEK_API_KEY="$DEEPSEEK_API_KEY" \
  script -q /dev/null sh -c "stty rows 40 cols 120; exec '$BIN' --session '$S1' --resume --workspace '$WORKSPACE'" \
  >"$RESUME_TRANSCRIPT" 2>&1 || true

python3 scripts/ansi-screen.py "$RESUME_TRANSCRIPT" 40 120 >"$RESUME_SCREEN"
if ! grep -aq "HISTORY_MARK" "$RESUME_SCREEN"; then
  echo "FAIL --resume did not replay history" >&2
  exit 1
fi
echo "PASS --resume replayed history"
echo "e2e-tui-session PASS"
