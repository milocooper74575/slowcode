# AGENTS.md

Guidance for coding agents and contributors working on slowcode.

## Project

slowcode is a Rust coding agent built around cognitive collaboration. The core
rule: **humans create the unknown, AI implements the known**. The product
treats cognitive load and loss of code ownership as first-class problems, not
automation percentage.

Current scope: a working Plan workflow (`/plan` through `/distill`), a ratatui
TUI, navigator/driver role separation, durable JSONL sessions, MCP, and local
skill installation.

## Layout

| Path | Contents |
|---|---|
| `crates/slowcode-kernel` | Vendored dsh-rs non-UI kernel. Read `UPSTREAM.md` before editing. |
| `crates/slowcode-roles` | navigator/driver assembly, Cordis fibers, session continuation, cache reports. |
| `crates/slowcode-runtime` | Run state, transitions, budget, approvals, model-call archive. |
| `crates/slowcode-cognition` | Plan validation, cognitive task table, artifact rendering. |
| `crates/slowcode-app` | Session assembly, command registry, Plan workflow, model switching. |
| `crates/slowcode-cli` | CLI entry point, binary `slowcode`. |
| `crates/slowcode-tui` | ratatui user interface. |
| `crates/slowcode-context` | AGENTS.md state, GLOSSARY stable prefix, HANDOFF pointer. |
| `crates/slowcode-mcp` | MCP config merge, stdio and HTTP connections, tool calls. |
| `crates/slowcode-skills` | Local skill install, validation, lock file, removal. |
| `crates/slowcode-feedback` | Issue-ready feedback bundles from a session journal. |
| `crates/slowcode-update` | Release checks and verified self-replace. |
| `crates/slowcode-audit` | Offline session audit: tokens, repeated reads, idle retries, plan churn. |
| `crates/slowcode-tests` | Cross-crate integration tests. |

Public references:

- `README.md` for users.
- `GLOSSARY.md` for domain terms.
- `CODING_STANDARDS.md` for code style and review rules.
- `crates/slowcode-kernel/UPSTREAM.md` for vendored changes and local deltas.

## Private local paths

These paths are gitignored and must never be added to the repository:

- `docs/`, `HANDOFF.md`, `.scratch/`, `.jcode/`, `excerise/`, `skills-lock.json`
- `.env`, `.slowcode/`, `.agents/`, `feedback/`, `.metrics/`

They may exist in a local checkout. Do not force-add them, do not paste their
contents into issues, and do not reference them from public documentation.

## Build and test

```bash
cargo fmt --all -- --check
scripts/clippy.sh
cargo test --workspace --exclude slowcode-kernel
```

Kernel changes:

```bash
cargo test -p slowcode-kernel --lib
```

Fast local loop, using `cargo-nextest` when installed so test binaries run in
parallel and falling back to `cargo test`:

```bash
scripts/test-fast.sh                 # workspace without the vendored kernel
scripts/test-fast.sh slowcode-tui    # one crate
scripts/test-fast.sh --full          # workspace including the kernel
```

Real provider tests are `#[ignore]` and need `DEEPSEEK_API_KEY`:

```bash
cargo test -p slowcode-cli --test e2e_deepseek -- --ignored --nocapture
cargo test -p slowcode-cli --test plan_flow_deepseek -- --ignored --nocapture
```

Do not call a change verified without running the relevant tests.

## Running

```bash
# TUI
cargo run -p slowcode-cli -- --workspace <dir>

# headless turn
cargo run -p slowcode-cli -- run --workspace <dir> "task"

# REPL
cargo run -p slowcode-cli -- repl --workspace <dir>

# approval before writes and shell
cargo run -p slowcode-cli -- --ask --workspace <dir>

# read-only
cargo run -p slowcode-cli -- --read-only --workspace <dir> "task"
```

## Releases

- Pushing a tag matching `v*` runs `.github/workflows/release.yml`.
- The workflow builds macOS arm64/x86_64 and Linux musl arm64/x86_64, packages
  `slowcode-<target>.tar.gz` with a sha256 file, and publishes a GitHub Release.
- `install.sh` downloads those assets from `milocooper74575/slowcode` by
  default. `SLOWCODE_REPO=owner/repo` overrides it for forks and mirrors.
- Do not add a release target without testing the produced binary on that
  platform.

## Conventions

- Rust 2024, `rust-version = 1.88`. No new warnings.
- One decision lives in one place. Derive instead of duplicating.
- Validate external data once at the seam: CLI args, config, JSONL, MCP
  responses, skill frontmatter.
- Prefer enums and newtypes over boolean soup. Make illegal states
  unrepresentable.
- Durable writes use a temporary path plus `fs::rename`, with rollback.
- `thiserror` for error enums. Do not add `anyhow`.
- Unit tests live beside the code under `#[cfg(test)]`. Cross-crate tests live
  in `crates/slowcode-tests/tests/`.
- Tests assert observable behavior through public interfaces. No network in
  unit tests.
- Batch test runs per module. Finish every edit in one module before running
  its tests; do not re-run the suite after each edit. Re-run on a completed
  change set, or on a failure that needs a narrower check.
- Commit in small ordered commits. Say why, what changed, and how it was
  proved.
- Vendored kernel files follow `crates/slowcode-kernel/UPSTREAM.md`. Record a
  local delta there before editing one.
- Do not commit secrets, session JSONL, generated plans, scratch fixtures, or
  local skill installs.

## Plan workflow facts

- `/plan` creates `.scratch/workflows/<workflow_id>/` with `state.json` and
  `events.jsonl`. The plan artifact under `.scratch/plans/` is a view.
- Phase gates are enforced in code: approval before scaffold/test, user
  implementation before continue, green tests before distill.
- Test contracts lock framework and test files by hash. Changing them blocks
  the workflow.
- `/distill` requires `## 理解漏洞报告` with the four required sections and
  one repair attempt.

## Feedback

- Every `/feedback` issue must become regression coverage before the fix is
  complete: a unit test for the failing seam and an e2e test for the
  user-visible workflow. Do not close the issue on manual validation alone.

## Security

- Keys come from `.env` or `DEEPSEEK_API_KEY`. Never print or commit them.
- MCP project servers are dormant until `/mcp enable`. Tools are risk-graded.
- Feedback bundles can contain conversation content. Review before sharing.
- The model never installs skills or approves its own writes.
