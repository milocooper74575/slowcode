#!/usr/bin/env bash
# Fast local test loop.
#
# By default this runs the workspace without the vendored kernel, matching the
# main CI gate. It uses cargo-nextest when available because nextest runs test
# binaries in parallel; plain cargo test runs one binary at a time.
#
# Usage:
#   scripts/test-fast.sh                    # workspace, excluding slowcode-kernel
#   scripts/test-fast.sh slowcode-app       # one or more crates
#   scripts/test-fast.sh --full             # workspace, including slowcode-kernel
#   scripts/test-fast.sh --cargo            # force cargo test
#   scripts/test-fast.sh --nextest          # fail instead of falling back
#
# Nextest does not run doctests. Use --cargo when changing documentation tests.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

include_kernel=0
force_cargo=0
require_nextest=0
crates=()

for argument in "$@"; do
  case "$argument" in
    --full|--kernel) include_kernel=1 ;;
    --cargo) force_cargo=1 ;;
    --nextest) require_nextest=1 ;;
    -h|--help)
      sed -n '2,15p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//'
      exit 0
      ;;
    -*)
      echo "unknown option: $argument" >&2
      exit 2
      ;;
    *) crates+=("$argument") ;;
  esac
done

scope=()
if [ "${#crates[@]}" -gt 0 ]; then
  for crate in "${crates[@]}"; do
    scope+=(-p "$crate")
  done
else
  scope=(--workspace)
  if [ "$include_kernel" -eq 0 ]; then
    scope+=(--exclude slowcode-kernel)
  fi
fi

nextest_available=0
if cargo nextest --version >/dev/null 2>&1; then
  nextest_available=1
fi

if [ "$force_cargo" -eq 0 ] && [ "$nextest_available" -eq 1 ]; then
  exec cargo nextest run "${scope[@]}"
fi

if [ "$require_nextest" -eq 1 ]; then
  echo "cargo-nextest is required but was not found" >&2
  exit 127
fi

exec cargo test "${scope[@]}"
