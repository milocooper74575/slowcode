# Security

## Reporting

Do not open a public issue for a vulnerability or a leaked credential.

Use GitHub private vulnerability reporting or a security advisory for this
repository. Include the affected version, a minimal reproduction, and the
impact. Do not include real API keys, session files, or private workspace
content.

## Secrets

- `DEEPSEEK_API_KEY` comes from `.env` or the environment. `.env` is ignored.
- Session JSONL, `.scratch/` workspaces, feedback bundles, and plan artifacts
  can contain prompts, code, and tool output. Review before sharing.
- MCP project servers stay dormant until `/mcp enable`. Tool calls are
  risk-graded and can require approval.
- The model cannot install skills or approve its own writes.

## Supported versions

Security fixes target the `main` branch. Pre-release versions are supported on
a best-effort basis.
