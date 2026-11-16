#!/usr/bin/env bash
# Clippy gate for slowcode-owned crates.
#
# `slowcode-kernel` is a vendored copy of deepseek-harness-rs. Its accepted
# lint debt is recorded in crates/slowcode-kernel/UPSTREAM.md and is not a
# signal about slowcode's own code. Exclude it here so one command produces a
# meaningful pass/fail. Run clippy on the kernel explicitly when the question
# is about the kernel.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# `--no-deps` is required: cargo still builds workspace members that are path
# dependencies, and clippy does not cap their lints. Without it the kernel's
# diagnostics come back through the dependency graph even with `--exclude`.
exec cargo clippy \
  --workspace \
  --exclude slowcode-kernel \
  --no-deps \
  --all-targets \
  "$@" \
  -- -D warnings
