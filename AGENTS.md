# workon

Rust CLI tool — development workspace launcher with Zellij, Claude CLI, and branchdiff.

## Build & Test

```bash
cargo check --locked        # type-check
cargo test --locked         # run tests
cargo fmt --all --check     # formatting (rustfmt.toml is set to the existing style)
cargo fmt --all --check --manifest-path fuzz/Cargo.toml   # the fuzz workspace too
cargo clippy --locked -- -D warnings   # lint (warnings are errors)
cargo deny check            # advisories, bans, licenses, sources
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
- `mise.toml` pins the runtimes the cycle tests' fixtures run on (Ruby, Node, PHP, .NET, Erlang, Elixir). ci.yml reads them in a step, and `tests/runtime_pin.rs` fails when a workflow names a version itself. Move a fixture to a new major there, and nowhere else.
- `tests/file_length.rs` fails when a file under `src` passes 400 production lines (inline test modules are not counted). Files already over are pinned at their size and may only shrink; new code goes in a new module, never into a pinned file.
- `tests/ci_rules.rs` fails when a workflow narrows `cargo deny check` (to `licenses`, say) or ci.yml or release.yml stops running `cargo deny check` or `cargo fmt --all --check`; when a workflow lacks a top-level concurrency group or `permissions`, or a job lacks `timeout-minutes`; when a workflow installs a moving `stable` over `rust-toolchain.toml`; when ci.yml stops building docs with `-D warnings`, running `cargo machete` or checking the MSRV; and when Dependabot appears or this file stops naming the upkeep job.
- `rust-toolchain.toml` pins an exact stable, and every workflow installs it with a bare `rustup toolchain install`. Move it to each new stable within 30 days, fixing what the new lints find, as its own commit. ci.yml's `msrv` job checks the declared `rust-version` on its own toolchain.
- Dependencies move through the `workon-deps` upkeep job on the maintainer's machine, never Dependabot: it prepares the update, checks it, and asks the owner; a yes pushes main through the push guard. It never bumps the version, so an update ships with the next release.

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
- `layout_inject` (2026-09-28, CI burst): counting and injection descended into
  a commanded pane that was not the agent, so a `pane command="claude"` nested
  inside `pane command="vim" { … }` got the args. zellij rejects a pane with
  both a `command` and nested panes, so that pane never runs. Any commanded pane
  is now a leaf (`layout::runs`). The crash input looked like a comment match,
  but it was a multi-line string followed by a real children block; comments and
  string contents never reach the structural matcher.

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

## Mutation testing

cargo-mutants (the `rust-mutation-testing` skill has the method).
`.github/workflows/mutants.yml`, not gating: on PRs and pushes to `main`,
`--in-diff` over the change (skipped with a warning above 25 selected mutants);
on pushes to `main` also one rotating slice, `--shard k/24` with
`k = run_number % 24`, so the whole tree is covered once every 24 pushes. Both
run `--jobs 2`; the slice summary prints how long cargo-mutants ran, which is
the number to re-choose 24 from. The slice stops itself after 15 minutes, under
the job's 25, so a hung mutant still gets a summary naming the newest logs and
an uploaded `mutants.out`; `gh workflow run mutants.yml -f slice=<k>` re-runs
one slice by number.

```sh
cargo mutants --list | wc -l                  # 658 on 2026-09-27
cargo mutants -j2 --no-shuffle --shard 3/24   # one slice, as CI runs it
```

**Choosing 24.** The suite is test-bound, and how test-bound depends on the
machine: slice 0/24 (28 mutants) took 19m39s on a laptop busy with other
builds, baseline 31s build + 86s test, while slice 2/48 (14) took 3m04s once it
was quiet, baseline 17s + 18s. The busy figure was `repairs_a_real_copied_venv…`
alone taking 80s (it builds a real venv with pip). A slice of ~28 is therefore
6-20 minutes locally; a hosted runner is uncontended, so 24 was chosen for
~10 minutes there with the 20-minute timeout as headroom. Re-choose from the
CI figure.

**Exclusions** (`.cargo/mutants.toml`):
- `gitignore = true`: build copies skip `fuzz/target` (1.1G) and the fixtures'
  installed deps (466M), so a local copy matches a fresh CI checkout.
- `src/fuzz_api.rs`: compiled only under `--cfg fuzzing`; `cargo test` never
  builds it, so every mutant would read MISSED.
- `DbEngine::create`, `Resource::teardown`, `mysqladmin`, and `setup` of every
  server-DB provisioner (Rails, Prisma, Alembic, Django, Laravel, EfCore,
  Phoenix). Without a live server each returns the same empty `Setup` or does
  nothing, so no test can observe a mutant; they are asserted by the DB-gated
  cycle tests in ci.yml's `provisioners` job (Postgres, MySQL, one toolchain per
  fixture). Giving the mutants jobs that environment would add those
  toolchains' setup to every run; the helpers they call (adapter parsing,
  `test_db_name`, the URL and Npgsql encoders) are still mutated.
- `session_layout`'s `agent.command == CLAUDE`: its only effect is copying a
  resumed transcript under `$HOME`, which a unit test cannot redirect while
  other tests read it in parallel.
- `Vcs::stranded_work -> vec![]`: the default body is `Vec::new()`.
- `session::run -> Ok(())`: it hands the terminal to a real zellij; its
  decisions (`session_exists`, `delete_session`) are tested with a stand-in.

jj must be on PATH (the workflow pins it): the jj-backed tests return early
without it, which reads as a pass and would turn their mutants into false
MISSED. Note that ci.yml's main job does not install jj, so those tests skip
there.

**Findings, 2026-09-27** (slice 0/24 and slice 2/48): 34 caught, 6 missed,
2 unviable before fixes (85%). All six were real gaps, now tested:
- `main.rs`: both `--name ""` filters (`delete !`). The rule moved to
  `cli::given_name`; `tests/cli.rs` runs `workon create --name … --json`.
- `deps::check_all -> Ok(())`, `deps::check_dep -> ()`: only `which` itself was
  tested, never the report.
- `claude_trust::approve_workspace -> Ok(())`, `home_dir -> Ok(Default)`: the
  trust write was tested below the function that picks `~/.claude.json`; the
  create test now reads it back from `$HOME`.

**2026-09-28:** three MISSED in `discover::assert_under_worktrees` (`==`→`!=`,
`||`→`&&`, delete `!`), the guard `workon destroy` runs before tearing a
workspace down. Its only test called it against the real `~/.worktrees`, which
a CI runner does not have, so every call failed at `canonicalize` and every
mutant passed. The check now runs against any root (`assert_under`) and a pure
`is_strictly_inside`, tested on a temp root (a workspace, the root itself, a
`worktrees-evil` sibling, `..` escapes, symlinks both ways) and by a proptest.

**2026-10-03** (CI run 37100256686): nine MISSED, all functions that read the
process environment or run `mise`, so no test could set what they read.
`layout::config_dir` and `DbEngine::url` (via `url_in`) now take the variables
through a lookup a test supplies; `url` itself is checked against `url_in` over
the real environment. `mise_env` takes the program to run, and its test runs a
stand-in script that reports its arguments and working directory.

**2026-10-03** (CI run 37149178056): four MISSED in `provision/mod.rs`, all
now tested: `auth_prefix`'s empty-password guard, `venv_python`, and the
`provisioners` registry order. `test_db_name`'s `>`→`>=` was equivalent (a
slice to the full length is the whole string); the comparison became
`min`, so the mutant no longer exists.

**2026-10-03** (CI run 37150772418): eight MISSED in `session.rs`. The match
guards of `session_exists` and `delete_session` could only be reached through a
real zellij; reading its reply moved to `zellij_reply` (`read_listing`,
`delete_hung`), tested with real timeout and non-zero-exit errors, which took
`session.rs` under the length limit. `parse_descendants`' `<`→`>` is killed by a
`comm` path with spaces, as macOS prints.

**2026-10-05** (CI runs 37339001486, 37339008529): sixteen MISSED. `session_exists`
and `delete_session` take the zellij program, as `mise_env` does, and a stand-in
logs what it was asked; `preflight_socket` and `locked_config` are tested with
`ZELLIJ_SOCKET_DIR` and `ZELLIJ_CONFIG_FILE` set under `ENV_MUTEX`;
`append_git_exclude`'s newline handling, `provision_in`'s `skip_copy_ignored`
and the `stranded_work` default each have a test. Two excluded (above).

**2026-10-06** (CI run 37498529534): four MISSED in `ref_candidates_from`'s
duplicate-nickname guard, `list_row` and `describe_workspace`'s cwd filter, all
now tested; `session::run`, picked by `--in-diff`, excluded (above).

## Dependencies

Dependencies move through the owner's `workon-deps` upkeep job, never Dependabot. It also adopts each new release of vcs-runner.

## Release process

Pushing to `main` triggers `.github/workflows/release.yml` which:
- Checks if the version in `Cargo.toml` has a corresponding git tag
- If not, builds cross-platform binaries, publishes to crates.io, creates a GitHub release, and triggers a Homebrew tap update
- Version bumps in `Cargo.toml` are what trigger releases — no manual tagging needed
