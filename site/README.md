# site

Public documentation source and the repository's single task source.

- `docs.json` mounts every Markdown file in `content/` into a section. Every
  `content/*.md` must be mounted, and every page needs an H1.
- `tasks.json` is the only task status source. `HANDOFF` carries context, never
  a second task list.
- `content/roadmap.md` contains exactly one main-flow marker section.
- `dist/` is generated and committed. Edit the sources, then rebuild.

```bash
cargo run -p slowcode-docs -- check
cargo run -p slowcode-docs -- build
cargo run -p slowcode-docs -- build --check
```

Task rules: `done` needs `evidence`, `deprecated` needs `reason`,
`in_progress` needs `next_action`, dependencies must exist, and every task
references at least one mounted document.
