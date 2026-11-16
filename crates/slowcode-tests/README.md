# slowcode integration tests

This package holds cross-crate integration tests. It intentionally has no
library or binary; Cargo discovers the tests under `tests/`.

## Layout rules

Following the Cargo Book:

- Unit tests live with the code they test, under `#[cfg(test)]`.
- Integration tests live in a package's `tests/` directory.
- This package's tests live in `crates/slowcode-tests/tests/`.
- Vendored kernel tests stay inside `crates/slowcode-kernel` so upstream
  diffs remain small.

## Running

```bash
cargo test --workspace
cargo test -p slowcode-tests
```
