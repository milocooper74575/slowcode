#!/usr/bin/env bash
# TUI smoke acceptance.
#
# This is intentionally narrower than scripts/e2e.sh: it proves the TUI starts
# in a real PTY, renders the main surfaces, runs inline commands, installs a
# skill and exits on /quit. Model turns are covered by scripts/e2e-tui-turn.sh.

set -euo pipefail

cd "$(dirname "$0")/.."

if [ "$(uname -s)" != "Darwin" ]; then
  echo "e2e-tui.sh currently targets macOS script(1)" >&2
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

TMP="${TMPDIR:-/tmp}/slowcode-e2e-tui.$$"
WORKSPACE="$TMP/workspace"
SESSIONS="$TMP/sessions"
TRANSCRIPT="$TMP/transcript.txt"
SCREEN="$TMP/screen.txt"
INPUT="$TMP/input.fifo"
SKILL_SRC="$TMP/skill-src/demo"
SKILL_DRY="$TMP/skill-src/dry-only"
ROWS=50
COLS=120
mkdir -p "$WORKSPACE" "$SESSIONS" "$SKILL_SRC" "$SKILL_DRY"
chmod 700 "$TMP" "$WORKSPACE" "$SESSIONS"
mkfifo "$INPUT"
cat >"$SKILL_SRC/SKILL.md" <<'SKILL'
---
name: demo
description: demo skill for TUI acceptance
---
Demo body.
SKILL
cat >"$SKILL_DRY/SKILL.md" <<'SKILL'
---
name: dry-only
description: dry run skill for TUI acceptance
---
Dry body.
SKILL

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
  script -q /dev/null sh -c "stty rows $ROWS cols $COLS; exec '$BIN' --workspace '$WORKSPACE'" \
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

send_raw() {
  printf '%s' "$1" >&3
}

step() {
  local label="$1"
  local needle="$2"
  if wait_for "$needle"; then
    echo "PASS $label"
  else
    echo "FAIL $label" >&2
    exit 1
  fi
}

step startup "Type a task"
send "/help"
step help "keys"
send_raw "/ro"
step completion-hint "tab 补全"
send_raw $'\t'
refresh_screen
if grep -aq "> /role" "$SCREEN"; then
  echo "PASS completion-accept"
else
  echo "FAIL completion-accept" >&2
  exit 1
fi
send_raw $'\r'
step completion-command "role: driver"
send_raw "/roz"
sleep 0.3
refresh_screen
if grep -aq "tab 补全" "$SCREEN"; then
  echo "FAIL completion-cancel" >&2
  exit 1
else
  echo "PASS completion-cancel"
fi
send_raw $'\177\177\177\177'
send_raw "abc"
send_raw $'\033[D'
send_raw "X"
step cursor-left "> abXc"
send_raw $'\177'
step cursor-backspace "> abc"
send_raw $'\033[3~'
step cursor-delete "> ab"
send_raw $'\033[F'
send_raw $'\177\177'
send_raw "a中b"
send_raw $'\033[D'
send_raw $'\033[D'
send_raw "X"
step cursor-unicode "> aX中b"
send_raw $'\033[F'
send_raw $'\177\177\177\177'
send_raw $'\033[200~one\ntwo\033[201~'
sleep 0.3
send_raw $'\033[A'
send_raw "X"
step composer-multiline "> oneX"
send_raw $'\033[B'
send_raw $'\033[F'
send_raw $'\177\177\177\177\177\177\177\177\177\177\177\177\177\177\177\177\177\177\177\177'
send "/model"
step model-list "usage: /model <name>"
send "/model deepseek-v4-pro"
step model-switch "model: deepseek-v4-pro"
send "/role navigator"
step role-navigator "role: navigator"
send "/role driver"
step role-driver "role: driver"
send "/cache"
step cache "cache: 0 read"
send "/login"
step login "DEEPSEEK_API_KEY is set"
send "/context"
step context "AGENTS.md"
step context-system "System prompt"
step context-environment "Environment info"
step context-mcp "MCP tools"
step context-skills "Skill descriptions"
step context-handoff "HANDOFF.md"
step context-window "context window"
if grep -aqE '[0-9]+\.[0-9]+%' "$SCREEN"; then
  echo "PASS context-percent"
else
  echo "FAIL context-percent" >&2
  exit 1
fi
if grep -aq "ctx \[" "$SCREEN"; then
  echo "PASS context-progress"
else
  echo "FAIL context-progress" >&2
  exit 1
fi
send "/session"
step session-list "usage: /session switch"
send_raw $'\033[<0;1;2M'
send_raw $'\033[<32;60;20M'
send_raw $'\033[<0;60;20m'
step mouse-copy "已复制"
CLIP_LEN="$(pbpaste | wc -c | tr -d ' ')"
CLIP_CHARS="$(pbpaste | wc -m | tr -d ' ')"
MSG_CHARS="$(grep -aoE '已复制 [0-9]+ 个字符' "$SCREEN" | tail -n 1 | grep -oE '[0-9]+' || true)"
if [ "$CLIP_LEN" -gt 10 ] && [ -n "$MSG_CHARS" ] && [ "$CLIP_CHARS" -eq "$MSG_CHARS" ]; then
  echo "PASS clipboard-content ($CLIP_LEN bytes, $CLIP_CHARS chars)"
else
  echo "FAIL clipboard-content (bytes=$CLIP_LEN chars=$CLIP_CHARS message=$MSG_CHARS)" >&2
  exit 1
fi
send "/session rename demo-title"
step session-rename "renamed"
send "/skills install $SKILL_SRC"
step skill-preview "skill install: demo"
send "y"
step skill-install "skill installed"
send "/skills"
step skill-list "demo"
send "/skills install $SKILL_DRY --dry-run"
step skill-dry-run "dry run: nothing was written"
send "/skills install $SKILL_SRC --name renamed-demo"
step skill-rename-preview "rename: demo -> renamed-demo"
send "y"
step skill-rename-install "skill installed: renamed-demo"
send "/context reload"
step context-reload "context reloaded from disk"

for _ in $(seq 1 40); do
  send_raw $'\033[<64;10;10M'
done
sleep 0.5
refresh_screen
if grep -aq "commands" "$SCREEN"; then
  echo "PASS mouse-scroll-history"
else
  echo "FAIL mouse-scroll-history" >&2
  exit 1
fi

for _ in $(seq 1 40); do
  send_raw $'\033[<65;10;10M'
done
sleep 0.5
refresh_screen
if grep -aq "context reloaded from disk" "$SCREEN"; then
  echo "PASS mouse-scroll-bottom"
else
  echo "FAIL mouse-scroll-bottom" >&2
  exit 1
fi

for _ in $(seq 1 40); do
  send_raw $'\033[<64;10;10M'
done
sleep 0.3
send_raw $'\033[F'
sleep 0.3
refresh_screen
if grep -aq "context reloaded from disk" "$SCREEN"; then
  echo "PASS end-key-bottom"
else
  echo "FAIL end-key-bottom" >&2
  exit 1
fi

S1_PATH="$(find "$SESSIONS" -maxdepth 1 -name 'session-*.jsonl' -type f | head -n 1)"
S1_ID="$(basename "$S1_PATH" .jsonl)"
send "/session new"
step session-new "new session"
send_raw "/session switch ${S1_ID:0:12}"
step session-id-completion "tab 键补全"
send_raw $'\t'
sleep 0.3
refresh_screen
if grep -aq "> /session switch $S1_ID" "$SCREEN"; then
  echo "PASS session-id-accept"
else
  echo "FAIL session-id-accept" >&2
  exit 1
fi
send_raw $'\r'
step session-switch "session: $S1_ID"

refresh_screen
send "/quit"
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

failures=0
check() {
  local needle="$1"
  if grep -aq "$needle" "$SCREEN"; then
    echo "PASS $needle"
  else
    echo "FAIL $needle" >&2
    failures=$((failures + 1))
  fi
}

check "slowcode"
check "Shift+Enter newline"

if [ -f "$WORKSPACE/.slowcode/skills/demo/SKILL.md" ]; then
  echo "PASS installed skill file"
else
  echo "FAIL installed skill file" >&2
  failures=$((failures + 1))
fi
if [ ! -e "$WORKSPACE/.slowcode/skills/dry-only" ]; then
  echo "PASS dry run left no skill directory"
else
  echo "FAIL dry run left no skill directory" >&2
  failures=$((failures + 1))
fi
if [ -f "$WORKSPACE/.slowcode/skills/renamed-demo/SKILL.md" ] \
  && grep -aq "^name: renamed-demo$" "$WORKSPACE/.slowcode/skills/renamed-demo/SKILL.md"; then
  echo "PASS renamed skill file has the rewritten frontmatter"
else
  echo "FAIL renamed skill file has the rewritten frontmatter" >&2
  failures=$((failures + 1))
fi

SESSION_FILE="$(find "$SESSIONS" -maxdepth 1 -name 'session-*.jsonl' -type f | head -n 1)"
if [ -z "$SESSION_FILE" ]; then
  echo "FAIL no durable session was written" >&2
  failures=$((failures + 1))
else
  SESSION_ID="$(basename "$SESSION_FILE" .jsonl)"
  RESUME_TRANSCRIPT="$TMP/resume.txt"
  RESUME_SCREEN="$TMP/resume-screen.txt"
  (
    sleep 2
    printf '/quit\r'
    sleep 1
  ) | env \
    DSH_SESSION_ROOT="$SESSIONS" \
    DEEPSEEK_API_KEY="$DEEPSEEK_API_KEY" \
    script -q /dev/null sh -c "stty rows 30 cols 100; exec '$BIN' --session '$SESSION_ID' --resume --workspace '$WORKSPACE'" \
    > "$RESUME_TRANSCRIPT" 2>&1
  python3 scripts/ansi-screen.py "$RESUME_TRANSCRIPT" 30 100 > "$RESUME_SCREEN"
  if grep -aq "slowcode" "$RESUME_SCREEN"; then
    echo "PASS resume $SESSION_ID"
  else
    echo "FAIL resume output" >&2
    failures=$((failures + 1))
  fi
fi

CTRL_TRANSCRIPT="$TMP/ctrl-c.txt"
CTRL_STATUS=0
(
  sleep 2
  printf '\003'
  sleep 1
) | env \
  DSH_SESSION_ROOT="$SESSIONS" \
  DEEPSEEK_API_KEY="$DEEPSEEK_API_KEY" \
  script -q /dev/null sh -c "stty rows 30 cols 100; exec '$BIN' --workspace '$WORKSPACE'" \
  > "$CTRL_TRANSCRIPT" 2>&1 || CTRL_STATUS=$?
if [ "$CTRL_STATUS" -eq 0 ]; then
  echo "PASS ctrl-c-exit"
else
  echo "FAIL ctrl-c-exit (status $CTRL_STATUS)" >&2
  failures=$((failures + 1))
fi

if [ "$failures" -ne 0 ]; then
  echo "e2e-tui failed: $failures check(s)" >&2
  if [ "${SLOWCODE_E2E_KEEP:-0}" != "1" ]; then
    echo "set SLOWCODE_E2E_KEEP=1 to keep the transcript" >&2
  fi
  exit 1
fi

echo "e2e-tui PASS"
