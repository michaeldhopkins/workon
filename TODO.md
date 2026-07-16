# TODO

## Two line scanners still disagree with the structural matcher

`Config::parse` now has the layout parsed, and `count_panes_running` /
`inject_into` decide things structurally. Two older scanners still read the same
layout as *text* and can therefore contradict them:
`layout::command_in_line` (behind `focused_command`) and `deps::extract_commands`.

Concrete divergences, all currently possible:

- `pane command=r#"claude"# focus=true` — structural: matches. Line scan: finds
  nothing, so `focused_command` returns `None`, the session-mismatch guard
  (`src/session.rs`) silently passes, and `deps::check_all` never checks that
  `claude` is on PATH.
- `pane command="clau\u{64}e"` — structural: `claude`. Line scan: the literal
  `clau\u{64}e`, so `deps::check_all` hard-fails on a valid config.
- `// pane command="claude"` alongside a real `pane command="opencode"` —
  structural: no match. Line scan: yields `claude`, demanding it on PATH, and
  with no `focus=true` the fallback picks the commented-out pane as focused,
  producing a bogus layout-mismatch bail.
- `pane command="vim" command="claude"` — KDL says last wins, so zellij runs
  `claude`; the line scan resolves `vim`.

These predate the agent block (both scanners are documented as deliberately
naive), so this isn't a regression — but the justification in `src/deps.rs`
("a real KDL parser would be overkill") no longer holds now that `Config` holds
a `KdlDocument`. Derive the focused command and the dependency list from that
document and the whole class disappears.

## `workon attach` mints a session id it then throws away

Pre-existing, noticed while moving the agent knowledge into config. `cmd_attach`
(`src/workspace.rs`) calls `attach(&ws, &cfg, None)`, so `session_layout` mints a
fresh session id and injects it, then discards the return value. Re-attaching a
persistent workspace therefore starts the agent on a *new* session each time,
orphaning the previous transcript, and prints no hint for recovering it. Either
persist the id in `.workon.json` and reuse it on attach, or accept the fresh
session and say so. Decide before adding more session-id surface area.

## Agent generalization

The `workon { agent ... }` block moved an agent's *arguments* into config data.
Three Claude-specific behaviors are still hardcoded, deliberately: there is only
one real data point to generalize against, and guessing a second agent's shape
would bake in the wrong abstraction.

- **Transcript migration.** `migrate_claude_session` (`src/workspace.rs`) scans
  `~/.claude/projects/` for `<session_id>.jsonl` and copies it into a directory
  named by `encode_claude_project_path`. It runs only when the declared agent is
  `claude`. Generalizing means expressing one vendor's storage layout as config
  (a path template, plus its path-encoding scheme) — worth doing once a second
  agent needs it, not before.
- **Workspace trust.** `provision` calls `claude_trust::approve_workspace`
  unconditionally, writing `~/.claude.json` even for a config that drives codex
  or no agent. Harmless (best-effort, ignored on error) but untidy; it should be
  gated on the declared agent once `provision` has the parsed config.
- **`--remote-control`.** The block can already express it —
  `new "--remote-control" "{name}"` — but there is no `{name}` placeholder yet,
  and plain (non-`-w`) sessions never inject at all: `Config::resolve` is called
  without args, and only the workspace flow reaches
  `resolve_with_agent_args`. Wiring remote control means adding a `{name}`
  expansion and an injection point on the plain path. Decide one spelling for
  `{name}` first — the `-w` flow would supply the capitalized tab name
  (`Uptime-thing`) while a plain session supplies the project dir name
  (`uptime-thing`), and those should not diverge on a phone screen.
