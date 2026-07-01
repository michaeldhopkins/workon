# workon

Development workspace launcher using Zellij.

Opens a project directory in a Zellij session using a layout you pick. The default layout is 3 panes:
- **Left top (80%)**: Claude CLI
- **Left bottom (20%)**: Terminal
- **Right (50%)**: branchdiff

You can switch to a different layout per invocation with `-c <name>` — see [Custom configs](#custom-configs).

## Dependencies

- [zellij](https://zellij.dev/) - Terminal multiplexer (always required)
- [jj](https://martinvonz.github.io/jj/) - Required for `-w` (workspace) mode

The remaining dependencies are derived from whichever config you launch. The default config requires:

- [claude](https://claude.ai/code) - Claude CLI
- [branchdiff](https://github.com/michaeldhopkins/branchdiff) - Git/jj diff TUI

A different config (see [Custom configs](#custom-configs)) may require different binaries — workon will detect them by parsing the active layout and tell you what's missing.

## Installation

```bash
brew install michaeldhopkins/tap/workon
```

Or build from source:

```bash
cargo install workon
```

## Usage

workon always operates on the **current directory** — `cd` into your project first.

```bash
workon                       # open the current directory with the default config
workon --new-session         # force new session (destroys existing)
workon --name api            # name the session "api" instead of the directory name
workon -w                    # ephemeral jj workspace (parallel session)
workon -c opencode           # open with the "opencode" custom config
workon -w --name fix-bug -c opencode  # named workspace using a custom config
```

The session is named after the current directory by default. Pass `--name` to
override it — handy for running a second session of the same project under a
distinct name. With `-w`, `--name` also labels the worktree and the saved bookmark.

## Workspace mode (`-w`)

Creates an ephemeral jj workspace in `~/.worktrees/` for running a second independent session on the same repo. The workspace is cleaned up when the Zellij session closes.

What it does:
1. Initializes jj (colocated) if the project only has git
2. Creates a jj workspace branched from trunk (main/master)
3. Clones gitignored files (build artifacts, `node_modules/`, `target/`, etc.) using APFS `clonefile(2)` on macOS for near-instant copy-on-write directory cloning, with cross-platform reflink fallback via [clonetree](https://crates.io/crates/clonetree)
4. For Rails apps: creates an isolated test database and loads the schema
5. Launches a Zellij session in the workspace
6. On exit: prompts to bookmark uncommitted work, forgets the jj workspace, drops any test database, and removes the directory in the background

The primary session (plain `workon`) is unaffected — it works directly in the project directory as before.

### Limitations

- The workspace shares the development database with the primary session. Don't run migrations or the Rails server from a workspace.
- `parallel_rspec` uses shared test databases. Use `bundle exec rspec` in the workspace for isolated specs.

## Headless workspaces

`-w` is the interactive shorthand: create, attach, and destroy on quit. The same machinery is exposed as subcommands so scripts and agents can drive a workspace without a Zellij TUI. The lifecycle is `create → (attach ⇄ quit)* → destroy`; quitting an `attach` session detaches, and the workspace survives until `destroy`.

```bash
WS=$(workon create --name fix-bug)   # provision; prints the worktree path to stdout
workon list                          # workspaces at or under the cwd
workon attach fix-bug                # open it in a session (survives on quit)
workon destroy fix-bug               # tear down, saving rescued work
```

- `create` provisions the worktree (jj/git workspace, gitignored-file copy, Rails DB, mise) and prints its path to stdout. It does not start a session. `--json` prints `{ ws_id, path, db }`.
- `attach [REF]` opens an existing workspace and returns when the session quits — no teardown. `REF` is a ws_id, a `--name` nickname (given as stored or slugified), or a path; omit it to use the workspace the cwd is inside.
- `destroy [REF]` bookmarks rescued work under `workon/<ws_id>` and removes the worktree. `--no-save` discards instead. `--json` prints `{ ws_id, saved, dropped_db }`. It refuses any path that isn't under `~/.worktrees`.
- `list` shows workspaces whose project is at or under the cwd, plus any `stale` worktrees (a leaked `create` with no matching `destroy`), which are shown from anywhere. `--json` prints an array.

Structure is inferred live from jj/git and the `~/.worktrees/<project>-<ws_id>` layout — there's no registry to go stale. The only persisted state is a small `.workon.json` inside each worktree recording what can't be inferred (the pinned base commit, the config, the nickname, the created DB); `rm -rf` on the worktree takes it with it.

A workspace whose project directory has been deleted can't be removed with `destroy` (its structure is no longer inferable) — remove its `~/.worktrees/` directory by hand.

## Custom configs

A "config" is a named Zellij layout file. Pick one with `-c <name>`; the default is used when `-c` is omitted (or `-c default` is passed explicitly).

### Creating a config

Configs are zellij layout files (`.kdl`) stored in `~/.config/workon/configs/`. To make one:

**1. Create the directory** (one-time, if it doesn't exist yet):

```bash
mkdir -p ~/.config/workon/configs
```

**2. Pick a name** — letters, digits, `-`, or `_` only. The filename must match what you'll pass to `-c`. For example, to make a config called `opencode`:

```bash
$EDITOR ~/.config/workon/configs/opencode.kdl
```

**3. Write the layout.** Here's a 4-pane starter (opencode + branchdiff + specdiff) you can adapt:

```kdl
default_mode "locked"

layout {
    tab {
        pane split_direction="vertical" {
            pane split_direction="horizontal" size="50%" {
                pane command="opencode" size="80%" focus=true
                pane size="20%"
            }
            pane split_direction="horizontal" size="50%" {
                pane command="branchdiff" size="80%"
                pane command="specdiff" size="20%"
            }
        }
    }
}

on_force_close "quit"
session_serialization false
```

**4. Trust it.** workon refuses to run a config until you've blessed it by hand — see [Trusting configs](#trusting-configs). Add an entry to `~/.config/workon/trusted.toml`:

```toml
[[trusted]]
path = "/Users/you/.config/workon/configs/opencode.kdl"
sha256 = "…"   # shasum -a 256 ~/.config/workon/configs/opencode.kdl
```

**5. Launch it:**

```bash
workon -c opencode
```

**Tips:**

- Mark exactly one pane with `focus=true`. workon uses it to detect which config a running session was launched with. Without it, the [layout-mismatch guard](#layout-mismatch-guard) can't reliably distinguish your configs.
- Every `command="..."` in the layout must be a binary on your `PATH`. workon checks before launching and tells you what's missing.
- For the full layout syntax, see the [zellij layout docs](https://zellij.dev/documentation/creating-a-layout.html).

### Where configs are loaded from

The default config (no `-c` flag, or `-c default`) is resolved in this order:

1. `~/.config/workon/configs/default.kdl` — your override, if present
2. `~/.config/workon/layout.kdl` — legacy single-config path, still honored
3. The embedded default layout (claude + branchdiff)

Named lookup (`-c foo`) only checks `~/.config/workon/configs/foo.kdl` and errors if the file is missing.

The embedded default (option 3) ships inside the binary and is always trusted. Any config read from disk — `default.kdl`, the legacy `layout.kdl`, or a named config — must be trusted first.

### Trusting configs

A config is a Zellij layout, and a layout can launch arbitrary commands in its panes:

```kdl
pane command="bash" {
    args "-c" "rm -rf ~"
}
```

So anything that can write a `.kdl` into `~/.config/workon/configs/` could get a command to run the next time you launch that config. workon will not run an on-disk config unless you have pinned it in `~/.config/workon/trusted.toml`:

```toml
[[trusted]]
path = "/Users/you/.config/workon/configs/opencode.kdl"
sha256 = "9f2b…"
```

Each entry pins a config's absolute path to the sha256 of its contents. workon honors the file only while its hash still matches, so editing a config un-trusts it until you review the change and update the `sha256`. Get the hash with `shasum -a 256 <file>`; when a config isn't trusted, the error prints the path and hash and the exact block to paste.

Trust is granted only by hand-editing `trusted.toml`. workon never writes that file and has no `trust` subcommand, so a process running under workon can't bless a config on its own. This holds only while `trusted.toml` stays outside the write scope of whatever workon launches — if an agent can rewrite it, it can trust anything.

To skip trusting altogether, launch with no `-c` (or `-c default`) and keep no `default.kdl`/`layout.kdl` on disk; the embedded default runs without a pin.

### Layout-mismatch guard

A zellij session is named after its project directory, and zellij ignores the layout when attaching to an existing session. To prevent silently re-attaching with the wrong layout, workon inspects the running session's process tree and refuses to attach if the requested config's focused command isn't present:

```
$ workon -c opencode    # but a default-config session is already running
Error: zellij session 'workon' is already running in the main worktree with a
different layout. Zellij keeps the original layout when reattaching, so
'opencode' would be ignored.

To open it as a separate workspace, run: workon -w -c opencode
To replace the running session instead:  workon --new-session -c opencode
```

This works in both directions — bare `workon` against a session you started with `-c opencode` will also be refused.

### `--resume` requires a claude config

`-r/--resume` injects `--session-id` into the layout's `claude` pane. Configs without a `command="claude"` pane are rejected up-front:

```
$ workon -w --resume <id> -c opencode
Error: --resume only works with claude-based configs (active config: opencode)
```

## Session management

Sessions are named after the directory basename. Running `workon` twice in the same directory reattaches to the existing session. Use `--new-session` to start fresh.

If a zellij server is hung, workon detects the unresponsive IPC (5s timeout), kills only the server bound to your project's session (other sessions are left alone), removes the stale socket, and launches a fresh session. Recovery runs whether or not you pass `-n`. Pre-flight checks before `attach` and `launch` ensure the no-timeout interactive zellij commands won't block on a wedged server.

Workspace sessions are named `<project>-ws-<id>` (e.g., `mbc-ws-a1b2c3`) and don't collide with primary sessions.

## Claude Code setup

To skip the workspace trust prompt and auto-allow file operations in worktrees, add to `~/.claude/settings.json`:

```json
{
  "permissions": {
    "allow": [
      "Edit(~/.worktrees/**)",
      "Write(~/.worktrees/**)",
      "Read(~/.worktrees/**)"
    ]
  }
}
```

Workspace trust is also pre-seeded in `~/.claude.json` automatically on each `workon -w` launch.

## Tips

- **Click URLs**: `Cmd+Shift+Click` to open hyperlinks (Shift bypasses zellij mouse handling)
- **Locked mode**: Zellij starts in locked mode. Press `Ctrl+G` to unlock for Zellij features.
