# Vendored dsh-rs kernel

This directory contains a vendored copy of the non-UI core of
[`deepseek-harness-rs`](https://github.com/xizheyin/deepseek-harness-rs).

## Upstream

- Repository: https://github.com/xizheyin/deepseek-harness-rs
- Commit: `3c7e978b7a5e0885b8e9dfdf30e35d911cce7370`
- Commit date: 2026-08-29
- License: MIT (see `LICENSE`)

## What was copied

The `agent`, `model`, `provider`, `session`, and `tools` modules, plus the
top-level modules they depend on: `attachment`, `entropy`, `goal`, `json_value`,
`plan_mode`, `resident_credit`, `skills`, `time_context`, `user_question`,
`workspace_authority`, and `workspace_instructions`.

`cli/`, `tui/`, and `main.rs` were not copied. The `lib.rs` module list was
edited to match. `tests/fixtures/` was copied because vendored unit tests
include those JSON fixtures by relative path.

## Local changes

- Package renamed to `slowcode-kernel`, library named `slowcode_kernel`.
- `cli` and `tui` removed from `lib.rs`.
- Added `capability.rs` and `skill_roots.rs`; both are slowcode-owned, not
  upstream. `capability` is re-exported as `pub mod capability`, `skill_roots`
  as `pub mod skill_roots`.
- `capability.rs`: added `CapabilitySet::route_ref`, a borrowing lookup used by
  `slowcode-roles` so `ToolExecutor::execute` and `prepare` can dispatch through
  one `select` without tying the returned future to a cloned `Arc` temporary.
  This file is slowcode-owned, so no upstream diff replay is involved.
- `capability.rs`: `CapabilityRouter::shutdown` now attempts every route's
  shutdown and returns the first error. The previous early `?` left later
  contributors unclosed when one route failed. Added two unit tests covering
  first-error propagation and the all-success path.
- Added `#[allow(dead_code, unused_imports)]` to vendored modules because the
  removed UI layers were their only consumers. `capability` and `skill_roots`
  are not suppressed.
- `skills.rs`: shared roots are now `.slowcode/skills` and `.agents/skills`;
  `.dsh/skills` was removed. `SkillRuntime::from_authority_with_roots` prepends
  validated role-contributed roots before the shared roots, and
  `SkillRuntime::open` opens the workspace internally.
- `agent/mod.rs`: added `AgentLoop::install_skill_roots`, the controlled
  public seam for installing a Skill runtime from a workspace plus
  `SkillRoots`.
- `tools/registry.rs`: `ReadOnlyToolRegistry`, `LocalToolRegistry`, and
  `WorkspaceToolRegistry` gained `open_with_skill_roots`. The read-only
  registry now carries a `SkillRuntime`, declares the `skill` tool, and
  dispatches it. Tool order for the writable registry is unchanged.
- `tools/process.rs`: added the private `register_async_fd` helper, wrapping
  `AsyncFd::register` in the kernel's only `#[allow(unsafe_code)]` block. Both
  `reactor_preflight` and `adapt_pipe` use it, replacing the deprecated
  `AsyncFd::new`. The helper documents the ownership invariant; see
  `docs/adr/0017-async-fd-registration-seam.md`. A reactor test covers the
  registration path.
- `lib.rs`: exports the new `skill_roots` module.
- Unit tests that used `.dsh/skills` as a discovery root now use
  `.slowcode/skills`. A few tests still create `.dsh/skills` fixtures only to
  assert that the root is not scanned. The unused upstream fixture
  `tests/fixtures/tools/upstream_phase35_project_skills.json` still names the
  upstream roots.
- Raised `MAX_SESSION_EVENTS` from 4_096 to 16_384 in `session/codec.rs` so one
  long `/plan` answer and its tool events fit in a single session.
- Bounded `SourceSeqBitmap::from_sources` by the bitmap's own 4_096-source
  capacity instead of `MAX_SESSION_EVENTS` in `session/observer.rs`. The raised
  session limit exposed an out-of-bounds index in that check.
- `session/store.rs`: added the public `SessionStore::open_new_session` and
  `SessionStore::resume_session` seams plus `new_session_id` and
  `validate_session_id`. These wrap the existing crate-private prepare and
  resume flows for the CLI; no journal format or recovery logic changed.
- `Cargo.toml`: enabled the `v4` feature on `uuid` for `new_session_id`.
- `model/stream.rs`: raised `MAX_PROVIDER_STREAM_CHUNKS` from 4_000 to
  16_384. DeepSeek streams long `/plan` answers as many small deltas and the
  4_000 chunk budget surfaced as `RESPONSE_TOO_LARGE` before any text arrived.
- `session/event.rs`: raised `MAX_SOURCE_EVENT_SEQS` from 4_096 to 16_384 to
  match `MAX_SESSION_EVENTS`. One long streamed assistant surface referenced
  7_579 source deltas and failed with `TooManySources`.
- Added `session/ui.rs` and the root `ui` module: a public `UiEvent` /
  `UiEventKind` projection over the existing committed UI observer. The
  internal `CommittedUiEvent` types stay crate-private.
- `session/mod.rs`: added the public `Session::attach_ui_events` seam.
- `session/mod.rs`: added the public `Session::replay_ui_events` seam. It
  projects the model-visible message surface into public `ui::UiEvent`s so the
  TUI can rebuild history after `/session switch` or `--resume`. Durable
  sessions do not retain their event prefix, so replay reads committed
  messages instead.
- `session/observer.rs`: text and reasoning deltas are now lossy when the
  bounded UI channel is full. Control events still fault and detach as before.
  The final assistant message remains authoritative.
- `workspace_instructions.rs` is now a public module with
  `WorkspaceInstructionRuntime::from_workspace`. `agent/mod.rs` exposes
  `AgentLoop::install_workspace_instruction_runtime` so the CLI can load
  AGENTS.md without reimplementing discovery.
- `session/store.rs`: `SessionMetadata` fields are readable through public
  accessors and `SessionStore::list_for_workspace` exposes bounded, read-only
  session listing for the TUI session picker.
- `skills.rs` is now a public module with `read_skill_metadata`, a read-only
  frontmatter parser used by `slowcode-skills` for install validation.
- `agent/mod.rs`: `AgentLoop::rename_session_title` is public so the TUI can
  rename the active session without a second state store.
- `session/store.rs`: added `SessionStore::read_journal`, a read-only snapshot
  of one durable journal at the current on-disk prefix. It reuses the existing
  NOFOLLOW/owner/mode/link-count validation and caps the snapshot at 64 MiB.
  The caller flushes a live writer first.
- `agent/mod.rs`: `AgentLoop::session_mut` exposes the durable session for the
  flush before a feedback snapshot. No journal format or recovery logic changed.
- `tools/readonly_shell.rs`: new slowcode-owned tool for navigator, coach and
  auditor profiles. It accepts strict argv only, binds commands to one
  workspace, and exposes `shell.readonly` metadata to the application layer.
  Git inspection commands force `--no-pager`, `--no-ext-diff` and
  `--no-textconv`. `tools/mod.rs` exports it as `tools::readonly_shell`.
- `Cargo.toml`: enabled the `process` feature on `tokio` so the read-only shell
  can spawn and reap bounded subprocesses with cancellation and a timeout.

Keep local changes to upstream files minimal. Record every edit here before
touching an upstream file. The goal is a short, auditable diff that can be
replayed against a future upstream commit.

## Patch policy

Checked on 2026-11-09: `3c7e978` was still the newest upstream commit, pushed
2026-08-29, with no later activity. Treat this copy as ours to maintain rather
than waiting on upstream.

- Fix defects locally. Do not block a fix on an upstream release.
- Record every patch under Local changes before touching the file.
- Every patch brings its own test. Do not weaken or delete an upstream test to
  make a local change pass. If a test must change, say why in the entry.
- Prefer the smallest seam: a new public method over a rewrite of internals.
- If upstream resumes, diff against `3c7e978`, replay the Local changes, and
  drop any patch that upstream now contains.

Known upstream debt, accepted for now and not patched: several large functions
in `agent`, `session`, and `session/projection` sit well above complexity 10.
The five largest are `commit_tool_round`, `run_entered_turn`,
`try_commit_durable`, `compact_once`, and `run_step`. Patch one only when there
is a behavioural reason and a test, never to lower a score.

Known upstream lint debt, accepted for now and not patched: as of `3c7e978`,
`cargo clippy -p slowcode-kernel --all-targets` reports 65 unique diagnostic
locations. Measured on 2026-11-09: 37 `collapsible_if`, 10 `result_large_err`,
9 `large_enum_variant`, 7 `manual_saturating_arithmetic`, 3
`manual_is_multiple_of`, 2 `cloned_ref_to_slice_refs`, 1
`manual_unwrap_or_default`. The two `AsyncFd::new` deprecations were removed by
ADR-0017 and are no longer in this list. The remainder are style or cold-path
size overhead with no observed behavioural cost.

Do not batch-fix these lints to make a workspace gate green. The clippy gate in
`scripts/clippy.sh` excludes the kernel with `--exclude slowcode-kernel
--no-deps`. Fix a kernel lint only when the touched function changes for a
behavioural reason, and record the edit under Local changes.

## Syncing

1. Diff a newer upstream commit against `3c7e978`.
2. Apply the upstream changes to the copied files.
3. Re-apply the local changes listed above.
4. Run `cargo check` and `cargo test` at the workspace root.
