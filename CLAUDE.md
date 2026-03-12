# workon

Rust CLI tool — development workspace launcher with Zellij, Claude CLI, and branchdiff.

## Build & Test

```bash
cargo check --locked        # type-check
cargo test --locked         # run tests
cargo clippy --locked -- -D warnings   # lint (warnings are errors)
cargo deny check licenses   # license audit
```

## Pre-push checklist

CI runs all of the above with `--locked`, so the lockfile must be in sync. Before describing a commit that touches `Cargo.toml`:

1. `cargo check` — regenerates `Cargo.lock` if dependencies or version changed
2. `cargo test --locked` — make sure tests pass
3. `cargo clippy --locked -- -D warnings` — zero warnings policy
4. Verify `Cargo.lock` is included in the commit (`jj diff --stat`)

## Project structure

- Single binary crate, entry point at `src/main.rs`
- CLI parsing via `clap` (derive)
- Clippy lints configured in `Cargo.toml` under `[lints.clippy]` — several are set to `deny`
- `cargo-deny` config in `deny.toml`
- Changelog generation via `git-cliff` (`cliff.toml`)

## Release process

Pushing to `main` triggers `.github/workflows/release.yml` which:
- Checks if the version in `Cargo.toml` has a corresponding git tag
- If not, builds cross-platform binaries, publishes to crates.io, creates a GitHub release, and triggers a Homebrew tap update
- Version bumps in `Cargo.toml` are what trigger releases — no manual tagging needed
