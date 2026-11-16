#!/usr/bin/env bash
# One-command real DeepSeek acceptance run for slowcode.
#
# Covers: normal read/write/bash, /plan pause, answer -> scaffold -> execute
# -> distill statuses, REPL role switch, and non-zero exits on failure.

set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
MODEL="${SLOWCODE_E2E_MODEL:-deepseek-flash}"
KEEP="${SLOWCODE_E2E_KEEP:-0}"

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

pass() {
  echo "PASS: $*"
}

show_output() {
  local label=$1
  local file=$2
  echo "--- $label output ---" >&2
  cat "$file" >&2
  echo "--- end $label output ---" >&2
}

run_stage() {
  local label=$1
  shift
  if ! "$@" >"$BASE/$label.out" 2>&1; then
    show_output "$label" "$BASE/$label.out"
    fail "$label failed"
  fi
}

run_stage_retry() {
  local label=$1
  local attempts=$2
  shift 2
  local attempt
  for ((attempt = 1; attempt <= attempts; attempt++)); do
    if "$@" >"$BASE/$label.out" 2>&1; then
      return 0
    fi
    echo "WARN: $label attempt $attempt failed" >&2
  done
  show_output "$label" "$BASE/$label.out"
  fail "$label failed after $attempts attempts"
}

require_contains() {
  local file=$1
  local needle=$2
  local label=$3
  [[ -f "$file" ]] || fail "$label: missing file $file"
  grep -qF -- "$needle" "$file" || fail "$label: $file does not contain: $needle"
}

require_file() {
  local file=$1
  local label=$2
  [[ -f "$file" ]] || fail "$label: missing file $file"
}

if [[ -z "${DEEPSEEK_API_KEY:-}" ]]; then
  if ! grep -q '^DEEPSEEK_API_KEY=' "$ROOT/.env" 2>/dev/null; then
    fail "DEEPSEEK_API_KEY is not set and $ROOT/.env has no DEEPSEEK_API_KEY"
  fi
fi

echo "Building slowcode..."
cargo build -q -p slowcode-cli --manifest-path "$ROOT/Cargo.toml"
BIN="$ROOT/target/debug/slowcode"
[[ -x "$BIN" ]] || fail "binary is missing: $BIN"

BASE="$(mktemp -d "${TMPDIR:-/tmp}/slowcode-acceptance-XXXXXX")"
cleanup() {
  if [[ "$KEEP" == "1" ]]; then
    echo "Kept acceptance workspace: $BASE"
  else
    rm -rf "$BASE"
  fi
}
trap cleanup EXIT

export DSH_SESSION_ROOT="$BASE/session-store"
mkdir -p "$DSH_SESSION_ROOT"
chmod 700 "$DSH_SESSION_ROOT"

# 1. Normal task: read, edit, bash, and verify the result.
NORMAL_WS="$BASE/normal"
mkdir -p "$NORMAL_WS"
printf 'hello\n' >"$NORMAL_WS/hello.txt"
run_stage normal \
  "$BIN" --workspace "$NORMAL_WS" --model "$MODEL" --max-tokens 2048 \
  "Read hello.txt. Append a new line containing exactly goodbye to it. Then use the bash tool to run: wc -l hello.txt > result.txt. Finish with a one-line summary."
require_contains "$NORMAL_WS/hello.txt" "goodbye" "normal task"
require_file "$NORMAL_WS/result.txt" "normal task"
grep -q '2' "$NORMAL_WS/result.txt" || fail "normal task: result.txt does not contain 2"
pass "normal task read, edit, bash"

# 2. /plan must produce a task table and the unknown-domain pause.
PLAN_WS="$BASE/plan"
mkdir -p "$PLAN_WS"
printf 'alpha\nbeta\n' >"$PLAN_WS/notes.txt"
run_stage_retry plan 2 \
  "$BIN" --workspace "$PLAN_WS" --model "$MODEL" --max-tokens 32768 \
  "/plan 为 notes.txt 增加一个统计行数的函数，并说明哪些核心逻辑需要用户自己实现。"
PLAN_FILE="$(ls -1t "$PLAN_WS"/.scratch/plans/*.md 2>/dev/null | head -n 1 || true)"
[[ -n "$PLAN_FILE" ]] || fail "plan task: no artifact under .scratch/plans"
require_contains "$PLAN_FILE" "认知任务分配表" "plan task"
require_contains "$PLAN_FILE" "status: paused" "plan task"
require_contains "$PLAN_FILE" "validation: ok" "plan task"
pass "plan pauses with a task table and unknown-domain pause"

# 3. The active workflow is readable and resumable through the public CLI.
run_stage status \
  "$BIN" --workspace "$PLAN_WS" --model "$MODEL" --max-tokens 2048 \
  "/status"
require_contains "$BASE/status.out" "workflow: plan-" "status"
require_contains "$BASE/status.out" "phase:" "status"
require_contains "$BASE/status.out" "approvals:" "status"
run_stage next \
  "$BIN" --workspace "$PLAN_WS" --model "$MODEL" --max-tokens 2048 \
  "/next"
require_contains "$BASE/next.out" "next" "next"
pass "workflow status and next action"

# 4. Headless --ask without a TTY refuses and exits with approval_required.
ASK_WS="$BASE/ask"
mkdir -p "$ASK_WS"
set +e
"$BIN" --workspace "$ASK_WS" --ask --model "$MODEL" --max-tokens 2048 \
  "Create ask.txt containing exactly blocked." >"$BASE/ask.out" 2>&1
ASK_CODE=$?
set -e
[[ "$ASK_CODE" -eq 2 ]] || {
  show_output ask "$BASE/ask.out"
  fail "ask: expected exit 2, got $ASK_CODE"
}
require_contains "$BASE/ask.out" "approval_required" "ask"
[[ ! -f "$ASK_WS/ask.txt" ]] || fail "ask: wrote ask.txt without approval"
pass "headless --ask refuses without a TTY"

# 5. The same REPL keeps the session while navigator is read-only and driver writes.
ROLE_WS="$BASE/role"
mkdir -p "$ROLE_WS"
printf 'alpha\n' >"$ROLE_WS/alpha.txt"
printf 'alpha\n' >"$ROLE_WS/count.txt"
# Write with the driver first, then switch to navigator and prove it cannot
# write. The final refusal cannot bias the earlier driver turn.
printf '%s\n' \
  '/role navigator' \
  'Read alpha.txt and reply with its contents only.' \
  '/role driver' \
  'Append a new line containing exactly beta to count.txt.' \
  '/role navigator' \
  'Use the write tool to create blocked.txt with the word gamma.' \
  'exit' |
  "$BIN" repl --workspace "$ROLE_WS" --model "$MODEL" --max-tokens 2048 \
    >"$BASE/role.out" 2>&1 ||
  {
    show_output role "$BASE/role.out"
    fail "role switch failed"
  }
require_contains "$BASE/role.out" "role: navigator" "role switch"
require_contains "$BASE/role.out" "role: driver" "role switch"
[[ ! -f "$ROLE_WS/blocked.txt" ]] || fail "role switch: navigator wrote blocked.txt"
require_contains "$ROLE_WS/count.txt" "beta" "role switch"
pass "role switch keeps the REPL, driver writes, and navigator cannot write"

echo "All slowcode acceptance checks passed."
