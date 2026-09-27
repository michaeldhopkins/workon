# workon

Rust CLI tool — development workspace launcher with Zellij, Claude CLI, and branchdiff.

## Build & Test

```bash
cargo check --locked        # type-check
cargo test --locked         # run tests
cargo clippy --locked -- -D warnings   # lint (warnings are errors)
cargo deny check licenses   # license audit
cargo install --path .      # install locally so the user can test
```

Always run `cargo install --path .` after tests and clippy pass. The user expects the locally installed binary to reflect the latest changes.

## Versioning

Bump the version in `Cargo.toml` with every change. Use semver: patch for fixes/improvements, minor for new features, major for breaking changes. Do this before running the build & test steps so the lockfile and installed binary reflect the new version.

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
- `tests/file_length.rs` fails when a file under `src` passes 400 production lines (inline test modules are not counted). Files already over are pinned at their size and may only shrink; new code goes in a new module, never into a pinned file.

## Fuzzing

cargo-fuzz targets live in `fuzz/`, a standalone workspace built only by
`cargo +nightly fuzz` (the `rust-fuzzing` skill has the general method). CI:
`fuzz-replay.yml` replays each target's saved corpus on every push to `main` and
every PR (the gate); `fuzz.yml` gives each target ~180s of mutation on each push
to `main` through `fuzz/burst.sh` and saves the grown corpus (not a gate, no
schedule; `workflow_dispatch` takes a longer budget). `tests/fuzz_targets_wired.rs`
fails if a `[[bin]]` in `fuzz/Cargo.toml` is missing from either workflow.

```sh
cargo +nightly fuzz build
fuzz/burst.sh fuzz/target/aarch64-apple-darwin/release/layout_inject layout_inject 60
fuzz/target/aarch64-apple-darwin/release/layout_inject -runs=0 fuzz/corpus/layout_inject   # replay
```

Crate-private functions are reached through `src/fuzz_api.rs`, compiled only
under `--cfg fuzzing` (which cargo-fuzz sets). A new target that needs a private
function gets a wrapper there.

| Target | Kind | Asserts |
|---|---|---|
| `layout_inject` | roundtrip + structural | A config (`layout\0arg\0arg…`) that parses yields a zellij layout that parses with no `workon` block. Injecting args keeps one agent pane and the same pane commands, leaves every other pane byte-identical, and the agent pane's first `args` node reads back as its old entries followed by exactly the injected args. With no agent pane, injection changes nothing. The oracle is the kdl 4 crate, the KDL v1 parser zellij also uses. |
| `repo_and_tool_text` | never-panics + shape | Every parser over a repo file (`database.yml`, `schema.prisma`, `phpunit.xml`, `alembic.ini`, `mix.exs`, a `.csproj` tag), `jj log -T bookmarks`, `<tool> --version`, raw `ps` text, and a layout's lines. The `mix.exs` app name is an identifier; the trunk bookmark is one word of the input without `*`/`?`/`@git`; layout commands are non-empty, quote-free and unique. |
| `ps_tree` | structural invariant | A process table built from the input (three bytes per process: parent, name, `comm` form) is rendered as `ps -A -o pid=,ppid=,comm=` and parsed; the result is exactly the names reachable from the root. Parents may cycle. |
| `encoders` | roundtrip | For fuzzed `a\0b\0c\0d`: the test DB name and Phoenix partition are identifiers within Postgres's 63 bytes and keep the ws id; `postgresql://user:pass@host:port/db` reads back (via the `url` crate) as exactly that user, password, host (IPv6 included) and port; the whole Npgsql connection string (host, port, user, password) reads back field for field under the ADO.NET rules; the `pgrep` pattern matches this session's server and not a name one character off; slugs are `[a-z0-9-]`, have no empty segments, and are idempotent. |

**Bugs found, each with a unit test and a `seed-*`:**
- `encoders` (2026-09-26): an Npgsql value with leading or trailing non-ASCII
  whitespace went unquoted, and ADO.NET trims it.
- `layout_inject` (2026-09-27, first CI burst): kdl 4.7.1 panics building the
  error for `(true` (an unclosed type annotation at the end of input), so a
  malformed config crashed workon instead of being reported. `layout::parse_kdl`
  turns the panic into the parse error; kdl 4.x has no newer release to take.

**Where the `comm` forms come from.** `ps` output captured on macOS
(2026-09-26): a bare `claude`, full paths, a login shell as
`-/opt/homebrew/bin/zsh`, an app-bundle path with spaces, and argv-like entries
such as `npm exec @playwright/mcp@latest --headless`. That last shape reads back
as `mcp@latest --headless` (the text after the last `/`); `ps_tree` does not
generate it, since workon only asks whether a known command such as `claude` is
running.

**Vocabulary.** `fuzz/dict/layout_inject.dict` is KDL v1 syntax plus the node
names in `src/layout.rs`, `src/agent.rs` and `layouts/workon.kdl`;
`fuzz/dict/repo_and_tool_text.dict` is the literals the parsers in
`src/provision/*.rs`, `src/deps.rs`, `src/vcs/jj.rs` and `src/layout.rs` match.
Update them when a parser starts matching a new token. Repo-file seeds are the
fixtures under `tests/fixtures/`.

**What the frames hide.**
- `encoders` leaves out credentials with control characters (the Npgsql oracle
  does not model .NET's position-dependent handling of them) and session names
  with `/`, whitespace or control characters (they cannot be the last path
  element of a socket). The `pgrep` oracle is Rust's `regex`, not POSIX ERE; the
  two agree on every character `regex_escape` escapes.
- `layout_inject` checks what kdl reads back, not what zellij does with it.
- The URL's host is drawn from IPv6 literals and `[A-Za-z0-9.-]` names: a
  `PGHOST` with other characters is not a host libpq could reach either. The
  Npgsql string takes any host and port text, since it quotes every field.
- Known limits, not bugs: a socket-path `PGHOST` becomes `localhost` in the URL
  and Npgsql string (URL-driven clients need TCP), and a libpq multi-host
  `PGHOST` (`h1,h2`) is passed through as one host.

**Not fuzzed, and why.**
- `~/.claude.json`, `.workon.json` and `mise env --json`: parsed with
  `serde_json` into `Value` or derived structs, with no hand-written parsing to
  fuzz. `mise env` is read as JSON because its shell form quotes values for a
  shell (`'it'\''s'`) and spreads a multi-line value over several lines; the
  line parser it replaced corrupted both (captured 2026-09-26, mise 2026.2.21).
  `--json` is in every release named `mise` (checked at tag v2024.1.0, the first
  after the rename from rtx), so there is no fallback; output that is not JSON
  prints a warning rather than silently dropping the env (`src/mise_env.rs`).
- `trusted.toml`: parsed by `toml` into a derived struct.
- vcs-runner's own parsing: that crate is fuzzed in its own repo.
- `python_venv` repair reads and rewrites files on disk by plain substring
  replacement; there is no parser, and a filesystem harness would test `std::fs`.

## Release process

Pushing to `main` triggers `.github/workflows/release.yml` which:
- Checks if the version in `Cargo.toml` has a corresponding git tag
- If not, builds cross-platform binaries, publishes to crates.io, creates a GitHub release, and triggers a Homebrew tap update
- Version bumps in `Cargo.toml` are what trigger releases — no manual tagging needed
