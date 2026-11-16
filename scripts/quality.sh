#!/usr/bin/env bash
# Run the measured quality gate against slowcode-owned source only.
#
# The vendored kernel supplies most of the CRAP and duplication noise, and
# `excerise/` and `scripts/` are not product code. Restricting the source roots
# keeps the output actionable. Scan the kernel explicitly when the question is
# about the kernel.
#
# Usage:
#   scripts/quality.sh coverage [cargo llvm-cov args...]
#   scripts/quality.sh crap   [crapper args...]
#   scripts/quality.sh dry    [dryer args...]
#   scripts/quality.sh mutate [mutator args...]
#   scripts/quality.sh report [report args...]
#
# `coverage` writes the workspace lcov used by `crap`. It skips the vendored
# kernel tests because they dominate runtime and their coverage is not scored.
# Per-crate lcov files under `target/coverage/rust` are incomplete and override
# the workspace report, so `coverage` removes them.
#
# `ub` is not assumed to be on PATH:
#   UB=/path/to/ub-tools/bin/ub scripts/quality.sh crap
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

own_crates=(
  slowcode-app
  slowcode-cli
  slowcode-cognition
  slowcode-context
  slowcode-feedback
  slowcode-mcp
  slowcode-roles
  slowcode-runtime
  slowcode-skills
  slowcode-tui
)

sources=()
for crate in "${own_crates[@]}"; do
  sources+=(-s "crates/$crate/src")
done

command="${1:-crap}"
shift || true

case "$command" in
  coverage)
    if ! command -v cargo-llvm-cov >/dev/null 2>&1; then
      echo "cargo-llvm-cov not found; install it with 'cargo install cargo-llvm-cov'" >&2
      exit 127
    fi
    cargo llvm-cov --workspace --exclude slowcode-kernel --lcov \
      --output-path target/coverage/lcov.info "$@"
    rm -rf target/coverage/rust
    ;;
  crap | dry | mutate)
    ub="${UB:-ub}"
    if ! command -v "$ub" >/dev/null 2>&1; then
      echo "ub not found; set UB=/path/to/ub-tools/bin/ub" >&2
      exit 127
    fi
    exec "$ub" "$command" "${sources[@]}" "$@"
    ;;
  report)
    ub="${UB:-ub}"
    if ! command -v "$ub" >/dev/null 2>&1; then
      echo "ub not found; set UB=/path/to/ub-tools/bin/ub" >&2
      exit 127
    fi
    exec "$ub" report "$@"
    ;;
  *)
    echo "usage: $0 {coverage|crap|dry|mutate|report} [args...]" >&2
    exit 2
    ;;
esac
