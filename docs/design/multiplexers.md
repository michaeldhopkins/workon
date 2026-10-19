# Multiplexers

What workon asks a terminal session to do, which programs can do it from a script, and which of those workon drives.

Surveyed 2026-10-18. Adoption is judged from what ships in base systems and distro repositories, and from which tools show up in scripts. No download counts are claimed. On this Mac that day: zellij 0.45.1, GNU screen 4.00.03 at `/usr/bin/screen`. tmux, zmx, WezTerm, kitty, herdr, abduco, and dtach were not on `PATH`.

## What a script needs

workon has to create a detached session in a directory, turn a layout into panes, type a line into the focused pane, read recent output, wait until that output contains some text, attach the user's terminal, list sessions, and kill one. The layout workon already stores is KDL, the dialect zellij loads.

## Ranked

| Program | Adoption | What a script can do | Distinctive part | Decision |
|---|---|---|---|---|
| tmux | Highest. Packaged everywhere, and the program most shell scripts already call. | `new-session -d`, `split-window`, `select-pane`, `send-keys -l`, `capture-pane`, `has-session`, `list-sessions`, `kill-session`, `attach-session`. `-e` sets pane environment (tmux 3.2+). | The pane layout and the long-lived server. | Support |
| GNU screen | Still preinstalled on many Unix machines, including this one. New work rarely picks it. | `screen -dmS`, `-X stuff`, `-X hardcopy`, `-r`, `-ls`, `-X quit`. | Detachable sessions, from before tmux. | Later |
| zellij | High among current terminal tools. Already workon's backend. | `attach --create-background`, `action write-chars --pane-id`, `action send-keys --pane-id`, `action dump-screen --pane-id --full`, `action list-panes --json`, `list-sessions`, `delete-session --force`, `attach`. Layout files are its own KDL. | Layout files, plus the session workon already launches. | Support (existing) |
| kitty | Popular terminal. The mux is the terminal process. | `kitten @ launch`, `send-text`, `get-text`, while kitty is running with remote control allowed. | Remote control of a live GUI. | Later |
| WezTerm | Popular terminal, with its own mux. | `wezterm cli spawn`, `send-text`, `get-text`, `list`, while the GUI mux is running. | A mux inside the terminal, with a CLI. | Later |
| Ghostty | Popular terminal. Upstream has no multiplexer and no remote-control API. | Nothing stable to call. Community projects and Rex (a separate mux) are not Ghostty. | The terminal itself, not a session manager. | Leave out |
| zmx 0.8.1 | Small. | `run NAME -d COMMAND` (the name is the first argument; `-d` returns once the command is accepted), `send` (no carriage return added), `history`, `wait`, `list --short`, `kill --force`, `attach`. One PTY per session. `ZMX_DIR` is the socket directory. | A persistent shell with headless run, send, history, and wait, and no second full-screen UI. | Support |
| herdr 0.9.5 | Small. | Workspace, tab, pane, and agent as objects. Layout JSON, an agent wait, a local JSON socket. | Agent-aware panes, not just PTYs. | Later |
| shpool | Small. | A daemon that pools shells and an attach command. | Persistent shells, thinner CLI than zmx. | Later |
| Rex | New (about 2026-10). A scriptable mux, not part of Ghostty. | Its own CLI. Too new to treat as widely installed. | Scriptable panes from the Ghostty author. | Later |
| abduco, dtach | Small, long-standing. | Create a session and reattach. No panes, no send, no history. | Detach and nothing else. | Leave out |
| byobu | A front end that runs on tmux or screen. | Whatever tmux or screen can do. | Profiles and a status line. | Leave out |

## Supported

workon drives zellij, tmux, and zmx. The default stays zellij. `--mux` or `WORKON_MUX` selects one of the three.

zellij keeps the path it already had: an interactive `workon` or `workon -w` still calls `zellij --new-session-with-layout` or `zellij attach`. Headless `run` creates with `attach --create-background` and the layout file. `send` writes the line into the terminal pane (`write-chars --pane-id`) and sends Enter there. A focused plugin, such as the first-run wizard, does not receive it. `history` is `dump-screen` of that same pane.

tmux builds one window. The first layout leaf is `new-session -d`; each further leaf is `split-window -d`. A focused leaf other than the first is selected. Split direction and sizes are not applied. `send` is `send-keys -l` plus Enter. `history` is `capture-pane -p`. The server is the one `TMUX_TMPDIR` points at, or the user's default server.

zmx has one PTY. `run` starts a shell and types the focused leaf's command, or `true` when the leaf is a shell. Further panes are ignored. `send` includes the carriage return zmx does not add. `history` prints `zmx history`. `wait` in workon polls that history; it does not call `zmx wait`, which waits for a run task rather than for text.

`run`, `send`, `history`, and `wait` work on whichever of the three is selected. `run` creates the session if it is missing and does not replace one that already exists. `--command` is typed once the session is up.

## Left for later

Recorded in `TODO.md`: GNU screen, herdr, WezTerm, kitty, Rex, and shpool; tmux split direction and percentages; a layout model that replaces the two line scanners; the `mux` key in a workon config; zmx labels and `zmx wait`; herdr's agent wait; persisting the multiplexer and the agent session id; live multiplexer state on `workon list`; the workspace context file for the agent.
