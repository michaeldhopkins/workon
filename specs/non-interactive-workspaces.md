# Non-interactive workspaces

Status: proposed (for review)
Target version: 0.17.0

## Goal

Expose workon's workspace machinery — jj/git worktree creation, the APFS
`clonefile` gitignored-file copy, Rails DB setup, and the bookmark-aware
teardown — as composable commands that work **without a zellij TUI**, so scripts
and other agents can create a worktree, operate in it, and tear it down.

Non-goal: replacing the interactive flow. `workon -w` stays exactly as it is.

## Principles

1. **No central registry.** Structural facts (project dir, ws_id, trunk, age,
   the workspace list) are inferred live from jj/git plus the existing
   `~/.worktrees/<project>-<ws_id>` convention — the worktrees themselves are the
   source of truth, so there is nothing separate to go stale. The few facts that
   *aren't* reconstructible (what provision chose/did) live in a small file
   **inside the worktree**, which `rm -rf` removes with it — so even that can't
   outlive its subject.
2. **cwd is the project.** Consistent with 0.16.0 (the directory positional and
   `~/workspace/<name>` lookup are gone).
3. **Infer what's derivable; persist only provenance, where it can't outlive its
   subject.**
4. **Reuse, don't fork.** All modes compose the same `provision` / `attach` /
   `teardown` internals.

## CLI surface

```
workon                       # session in cwd                              (unchanged)
workon -w                    # ephemeral workspace; quit -> destroy        (unchanged, legacy)
workon create [--name N] [-c CFG] [--skip-copy-ignored] [--json]
workon attach [REF] [-c CFG]
workon destroy [REF] [--no-save] [--json]
workon list [--json]
```

`create` / `attach` / `destroy` / `list` are subcommands; `-w` and the other
flags remain top-level for the default/legacy path. clap parses both grammars
with `args_conflicts_with_subcommands = true` (verified — `workon` with a bare
flag is still the default mode, `workon create` dispatches to the subcommand).

`REF` is a workspace id, a `--name` nickname, or a path; omit it to use cwd.

### Lifecycle model

`-w` is the ephemeral shorthand (create + attach, **destroy on quit**). The
persistent lifecycle is `create -> (attach <-> quit)* -> destroy`, where quitting
zellij from an `attach` session **detaches** (the workspace survives). There is
no `--detach` verb: detach is just "quit zellij", and whether quit destroys is
decided by which mode you invoked.

### clap shape

```rust
#[command(args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    session: SessionArgs,   // existing flags: -w, --name, --new-session,
                            // --skip-copy-ignored, -r/--resume, -c/--config, -n
}

enum Command {
    Create  { #[arg(long)] name: Option<String>,
              #[arg(short='c', long)] config: Option<String>,
              #[arg(long)] skip_copy_ignored: bool,
              #[arg(long)] json: bool },
    Attach  { reference: Option<String>,
              #[arg(short='c', long)] config: Option<String> },
    Destroy { reference: Option<String>,
              #[arg(long)] no_save: bool,
              #[arg(long)] json: bool },
    List    { #[arg(long)] json: bool },
}
```

## Per-worktree metadata: `<ws_dir>/.workon.json`

Written by `provision`, excluded from jj/git via the existing
`ignore_generated_file` machinery (same as `.env.test.local`), and added to
`GENERATED_FILES`. Removed implicitly when `destroy` does `rm -rf <ws_dir>`.

Holds **only** what provision chose or did — i.e. what can't be inferred later:

```json
{
  "config": "opencode",                 // layout used; null = default
  "name": "fix bug",                    // original (un-slugified) label; null if none
  "created_db": "mbc_ws_abc123_test"    // the db provision created; null if none
}
```

Why each is here rather than inferred:
- `config` — the layout choice; not derivable from jj/git state.
- `name` — the slug is in the dir name, but the original label isn't.
- `created_db` — records that *we* created it. A probe ("does a db named
  `mbc_ws_abc123_test` exist?") can't tell our db from a coincidentally-named
  one, so storing the fact is safer than reconstructing it.

There is deliberately **no orphan baseline** here. Stranded-commit rescue is
attributed per-workspace from the workspace's own pointer history — jj's
operation log, git's per-worktree HEAD reflog (`Vcs::stranded_work`, shipped in
0.16.1) — which is queryable at *destroy* time in any process, concurrency-proof,
with nothing to persist. (An earlier draft stored an `orphans_before` snapshot;
it was dropped because a repo-wide baseline goes stale against concurrent
worktrees — that was the 0.16.1 bug fix.)

Everything structural (`project_dir`, `ws_id`, `trunk`, `vcs`, age) stays
inferred live — storing those is what reintroduces staleness (e.g. a moved
project makes a stored `project_dir` lie, while an inferred one adapts).

## Command semantics

### `workon` / `workon -w` (unchanged)

Default: normal session in cwd. `-w`: `provision -> attach -> teardown` — same
observable behavior as today, now through the extracted phases.

### `workon create`

1. `project = cwd`.
2. `ws = provision(project, {name, config, skip_copy_ignored})` — writes
   `.workon.json`.
3. Print `ws.ws_dir` to **stdout** (so `WS=$(workon create)` works); ws_id +
   hints to stderr. `--json` -> `{ "ws_id", "path", "db" }` on stdout.
4. Exit 0. No session, no teardown — the workspace persists.

`provision` is today's steps minus the claude session id, layout, and
`session::launch`: ws_id, ensure `~/.worktrees`, `detect_trunk`,
`create_workspace` (jj/git + git plumbing), `copy_gitignored_files` (unless
`--skip-copy-ignored`), `trust_mise_configs`, `setup_rails_db`, write
`.workon.json` + `ignore_generated_file` for it and the env file.

### `workon attach [REF]`

1. `ws = load_workspace(REF)`.
2. `mise_env(ws.ws_dir)`, resolve layout (`-c`, else `ws.config` from
   `.workon.json`, else default), generate a claude session id,
   `session::launch(ws.ws_dir)` — blocks.
3. On return (zellij quit): exit 0. **No teardown** — the workspace persists.

### `workon destroy [REF] [--no-save]`

1. `ws = load_workspace(REF)`.
2. **Safety:** assert `ws.ws_dir` is under `~/.worktrees/`; refuse otherwise.
3. `teardown(ws, save_mode)`, `save_mode = if --no-save { NoSave } else { Save }`.
4. Print outcome to stderr; `--json` -> `{ "ws_id", "saved":[..], "dropped_db":.. }`.

Tolerates a missing dir or db (skip, don't error).

### `workon list`

Scan `~/.worktrees/` (flat). For each entry, infer its `project_dir`; keep those
whose `project_dir` is at or under cwd. So:

- in a repo -> that repo's workspaces;
- in a parent of repos (`~/projects`) -> all descendants' workspaces;
- elsewhere -> none.

No `--all`, no child-scan — the flat `~/.worktrees/` scan plus the
project-under-cwd filter is the whole mechanism (the worktrees are centralized,
not nested under each project).

Columns: `WORKSPACE` (ws_id), `NAME` (from `.workon.json`, else `-`), `AGE`,
`PROJECT`, `STATUS`. `--json`: array of
`{ ws_id, name, age_seconds, project, ws_dir, status }`.

`STATUS`: `active`, or `stale` when the dir's project can't be resolved / jj no
longer tracks it (a leak from a `create` without a matching `destroy`).

Per-entry `project_dir` lookup is cheap (read the worktree's `.git` pointer file
directly, or `git rev-parse --git-common-dir`); parallelize if the count grows.

## Inference

A small `discover` module. All verified against jj 0.39 / git.

| Need | How |
|---|---|
| `~/.worktrees` path | `home_dir()/.worktrees` |
| `project_dir` from a worktree | parent of `git rev-parse --path-format=absolute --git-common-dir` (works because `setup_git_worktree` points back), or parse the `.git` pointer file |
| `ws_id` from a worktree | `ws_dir.basename().strip_prefix(format!("{project_name}-"))` |
| `trunk`, `vcs` | existing `detect_trunk` / `vcs::detect` |
| age | working-copy commit timestamp, or `ws_dir` mtime |
| stranded commits (for rescue) | `Vcs::stranded_work(ws_id, project_dir, ws_dir)` — op-log / reflog attribution (0.16.1) |
| `config`, `name`, `created_db` | read `<ws_dir>/.workon.json` |

### `load_workspace(ref) -> Result<Workspace>`

```
ws_dir =
  None            -> the worktree cwd is inside (walk cwd ancestors; must be
                     under ~/.worktrees/), else error "not inside a workspace —
                     pass an id or name"
  Some(path dir)  -> that path
  Some(token)     -> scan ~/.worktrees/ for a dir whose ws_id == token or whose
                     .workon.json name slug == token; error on none; error on
                     ambiguous (list candidates, suggest the ws_id)
project_dir  = git_common_parent(ws_dir)
project_name = project_dir.file_name()
ws_id        = strip_prefix(ws_dir.basename(), project_name + "-")
trunk        = detect_trunk(project_dir)
meta         = read .workon.json (config, name, created_db)
```

## Refactor

Split today's `run_workspace` into three reusable phases plus an in-memory
handle. The handle is rebuilt by `load_workspace` in a fresh process; its
provenance fields come from `.workon.json`, the rest are inferred.

```rust
pub struct Workspace {
    ws_id: String,
    name: Option<String>,
    project_dir: PathBuf,
    project_name: String,
    ws_dir: PathBuf,
    created_db: Option<String>,
    trunk: String,
}

pub enum SaveMode { Prompt, Save, NoSave }

fn provision(project, opts, vcs) -> Result<Workspace>;   // writes .workon.json
fn attach(ws, config, vcs) -> Result<()>;                // claude+layout+launch (blocks)
fn teardown(ws, save: SaveMode, vcs) -> Result<()>;      // detect+save+forget+dropdb+rm

fn run_ephemeral(project, opts, vcs) {                   // -w
    let ws = provision(project, opts, vcs)?;
    attach(&ws, opts.config, vcs)?;
    teardown(&ws, SaveMode::Prompt, vcs)
}
fn cmd_create(...)  { let ws = provision(...)?; print(ws.ws_dir); }
fn cmd_attach(ref)  { let ws = load_workspace(ref)?; attach(&ws, config, vcs)?; }   // no teardown
fn cmd_destroy(ref) { let ws = load_workspace(ref)?; assert_under_worktrees(&ws.ws_dir)?;
                      teardown(&ws, save_mode, vcs)?; }
```

`teardown` rescues stranded commits via `Vcs::stranded_work` (op-log / reflog
attribution, 0.16.1), which works the same in any process — so orphan rescue is
uniform across interactive and non-interactive with no baseline to thread.
`SaveMode` only governs the in-stack save decision (Prompt = `[Y/n]`, Save =
default-yes, NoSave = skip).

## Save semantics

- **Default save** on `destroy` (matches the interactive `[Y/n]` default-yes);
  `--no-save` skips.
- In-stack detection is bookmark/branch/remote-aware (shipped 0.15.1): a
  workspace whose work is bookmarked or pushed reports nothing and saves nothing.
- **Stranded-commit rescue applies to both flows**, via per-workspace attribution
  (`stranded_work`, 0.16.1) — queryable at destroy time, correct under
  concurrency, no baseline required.

## Safety / edge cases

- `destroy` refuses any `ws_dir` not under `~/.worktrees/`.
- `destroy` tolerates a missing dir or db.
- `create` that fails partway leaves a partial worktree; `list` surfaces it as
  `stale`. No automatic rollback in the first cut.
- `attach` into a workspace another zellij session holds: defer to zellij.
- A project named `create`/`attach`/`destroy`/`list` is shadowed by the
  subcommand — acceptable, documented.

## Testing

Pure unit:
- ref/token classification (path vs id vs nickname).
- `ws_id_of` (strip `{project}-` prefix), incl. a nickname suffix.
- `.workon.json` (de)serialization round-trip.
- `SaveMode` selection from flags.

Integration (jj/git temp repos, gated on `jj_available()` where needed):
- `provision` creates a worktree and writes/excludes `.workon.json`;
  `load_workspace` from cwd recovers `project_dir`, `ws_id`, and metadata.
- ref resolution by id, nickname, cwd; ambiguous nickname errors.
- `destroy` in-stack save -> `workon/<id>` bookmark; stranded rescue via
  `stranded_work`; `--no-save` skips; forget + rm happen.
- `destroy` safety: refuses a path outside `~/.worktrees/`.
- `list`: in a repo shows its workspaces; in a parent dir shows descendants';
  flags a stale dir.

CLI (`tests/cli.rs`):
- help lists `create`/`attach`/`destroy`/`list`; `-w` and legacy flags still parse.
- `create --name foo` and `destroy x --no-save` parse.

`attach`'s `session::launch` drives zellij and can't be unit-tested; cover the
resolve + layout-prep parts and leave the launch to manual verification.

## Out of scope (future)

- `workon prune` to sweep `stale` worktrees in one command.
- Addressing a workspace by partial/fuzzy id.
