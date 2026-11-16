#!/usr/bin/env bash
# Real-model acceptance for `/quit` running the handoff skill.
#
# This spends provider tokens. It creates a throwaway workspace, runs one short
# turn, then sends `/quit` and asserts HANDOFF.md was replaced.

set -euo pipefail
trap '' PIPE

cd "$(dirname "$0")/.."

if [ "$(uname -s)" != "Darwin" ]; then
  echo "e2e-tui-handoff.sh currently targets macOS script(1)" >&2
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

TMP="${TMPDIR:-/tmp}/slowcode-e2e-handoff.$$"
WORKSPACE="$TMP/ws"
SESSIONS="$TMP/sessions"
TRANSCRIPT="$TMP/transcript.txt"
INPUT="$TMP/input.fifo"
SCREEN="$TMP/screen.txt"
mkdir -p "$WORKSPACE/.jcode/skills" "$SESSIONS"
chmod 700 "$TMP" "$WORKSPACE" "$SESSIONS"
cp -R .jcode/skills/handoff "$WORKSPACE/.jcode/skills/handoff"
printf '# HANDOFF\n\nold stub\n' >"$WORKSPACE/HANDOFF.md"
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
  script -q /dev/null sh -c "stty rows 40 cols 120; exec '$BIN' --workspace '$WORKSPACE'" \
  <"$INPUT" >"$TRANSCRIPT" 2>&1 &
RUNNER=$!
exec 3>"$INPUT"

sleep 3
printf 'Use the write tool to create note.txt with content hello, then reply DONE.\r' >&3 || true
NOTE_READY=0
for _ in $(seq 1 180); do
  if [ -f "$WORKSPACE/note.txt" ]; then
    NOTE_READY=1
    break
  fi
  sleep 0.5
done
if [ "$NOTE_READY" -ne 1 ]; then
  echo "FAIL the first turn did not create note.txt" >&2
  exit 1
fi
for _ in $(seq 1 60); do
  python3 scripts/ansi-screen.py "$TRANSCRIPT" 40 120 >"$SCREEN"
  if grep -aq "idle" "$SCREEN"; then
    break
  fi
  sleep 0.2
done
sleep 1
printf '/quit\r' >&3 || true

CHANGED=0
for _ in $(seq 1 240); do
  if ! grep -aq "old stub" "$WORKSPACE/HANDOFF.md"; then
    CHANGED=1
    break
  fi
  sleep 1
done
if [ "$CHANGED" -eq 1 ]; then
  for _ in $(seq 1 60); do
    if ! kill -0 "$RUNNER" 2>/dev/null; then
      break
    fi
    sleep 0.5
  done
fi
if kill -0 "$RUNNER" 2>/dev/null; then
  printf '\003' >&3 || true
fi
exec 3>&-

STATUS=0
wait "$RUNNER" 2>/dev/null || STATUS=$?

if [ "$CHANGED" -ne 1 ]; then
  echo "FAIL HANDOFF.md still contains the stub" >&2
  exit 1
fi
if [ "$STATUS" -ne 0 ]; then
  echo "FAIL TUI exited with $STATUS" >&2
  exit 1
fi
HANDOFF_BYTES="$(wc -c <"$WORKSPACE/HANDOFF.md" | tr -d '[:space:]')"
if [ "$HANDOFF_BYTES" -lt 20 ]; then
  echo "FAIL HANDOFF.md was not written" >&2
  exit 1
fi
echo "PASS /quit wrote HANDOFF.md"
echo "e2e-tui-handoff PASS"
