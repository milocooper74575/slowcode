# Coding Standards

This file is the source of truth for how code in this repository should be
written. Where it endorses something a default smell baseline would flag, this
file wins.

It deliberately does not restate process skills. Where a rule has a fuller
owner elsewhere, the owner is named.

## Design

### One decision lives in one place

If two places must agree, make one derive from the other or collapse them into
one. Duplicated decisions drift.

### Shape before logic

Get the data shape right before writing behaviour. If a field can hold a value
the code must reject, fix the type first.

### Parse at the seam

Validate and convert external data once, where it enters the system (CLI
arguments, config files, JSONL sessions, MCP responses, skill frontmatter).
Inside that seam, trust the type. Do not re-validate the same value in every
caller.

Terminology follows `codebase-design`. Call the place a **seam**, not a
boundary.

### Make illegal states unrepresentable

Prefer an enum or newtype over a struct of booleans and options that can
describe a state the system cannot handle. Do not silence the compiler to
avoid modelling the case.

### Make operations idempotent

Re-running an install, a sync, or a write after a crash must land in the same
state. Durable writes go through a temporary path and `fs::rename`, with
rollback when the rename fails, as `slowcode-skills` does.

### Do not share mutable state

If two components do not need the same mutable value, give each its own. If
they do, allow one writer and enforce that in the type.

### Subtract before adding

Remove complexity before adding a feature. A new abstraction needs a second
real caller, not a hypothetical one.

## Rust

- `thiserror` for error enums in every crate. Do not add `anyhow`.
- Workspace edition is 2024, `rust-version` is 1.88. New code must build on
  that toolchain without warnings.
- `slowcode-kernel` keeps `#![deny(unsafe_code)]`. Any new `unsafe` anywhere
  needs an explicit design note in the pull request.
- Every crate root carries a `//!` module comment stating what the crate does
  and what it owns.
- Public error types name the failure, not the cause. Preserve the underlying
  error with `#[source]` instead of flattening it into a string.
- Vendored files under `crates/slowcode-kernel` follow `UPSTREAM.md`. Record a
  local change there before editing one.

## Tests

- Unit tests live beside the code under `#[cfg(test)]`.
- Cross-crate integration tests live in `crates/slowcode-tests/tests/`.
- Tests assert observable behaviour through the public interface. A test that
  still passes when every function returns nothing is not a test.
- Seams are agreed before tests are written. That gate belongs to `tdd`, not
  to this file.
- Unit tests do not hit the network. Real-provider tests are `#[ignore]` and
  are run explicitly.
- `cargo test --workspace` is the claim of done. Do not report a change as
  verified without it.

## Process

- Commit in small ordered commits. Each commit says why, what changed, and how
  it was proved. The commit sequence is the action log, so do not produce a
  separate one.
- Keep branches small and short-lived. Do not push straight to the main line
  while a change is still under review.
- When `ub` is on PATH, `scripts/quality.sh` is the measured gate for CRAP,
  duplication, and mutation. When it is absent, say the gate is unavailable
  rather than estimating a number.
- The quality gates cover slowcode-owned source only:
  `scripts/quality.sh crap|dry|mutate` runs `ub` over `crates/*/src` for the
  crates this repository owns. `scripts/clippy.sh` runs the clippy gate with
  `--exclude slowcode-kernel --no-deps`. The vendored kernel and `scripts/`
  are out of scope for a normal pass; scan them explicitly when the question
  is about them. Kernel debt is recorded in
  `crates/slowcode-kernel/UPSTREAM.md`.
- Architecture decisions that are hard to reverse need a written design note
  in the pull request. Do not relitigate an accepted decision inside an
  unrelated refactor.

## Where a rule lives

| concern | owner |
|---|---|
| failing test first, seam agreement | TDD practice |
| module depth, seam placement | codebase design review |
| domain terms | `GLOSSARY.md` |
| architecture decisions | pull request design note |
| reproductions and root cause | bug diagnosis |
| measured quality gates | `scripts/quality.sh` |
| diff review | code review |
