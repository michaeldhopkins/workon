# workon

Development workspace launcher using Zellij.

Opens a project directory in a 3-pane Zellij layout:
- **Left top (80%)**: Claude CLI
- **Left bottom (20%)**: Terminal
- **Right (50%)**: branchdiff (git diff viewer)

## Dependencies

- [zellij](https://zellij.dev/) - Terminal multiplexer
- [claude](https://claude.ai/code) - Claude CLI
- [branchdiff](../branchdiff) - Git diff TUI

Install all dependencies before using workon.

## Installation

Add to your `~/.zshrc` or `~/.bashrc`:

```bash
source /path/to/workon/shell/init.sh
```

Then reload your shell:

```bash
source ~/.zshrc
```

## Usage

```bash
# Open current directory
workon

# Open specific directory
workon ~/projects/myproject

# Force new session (deletes existing session with same name)
workon -n ~/projects/myproject
```

If any dependencies are missing, workon will list them and exit.

## Session Management

workon uses Zellij sessions named after the directory basename. This means:

- Running `workon ~/projects/foo` creates a session named "foo"
- Running `workon ~/projects/foo` again attaches to the existing "foo" session
- The terminal tab/window title shows the session name
- Use `workon -n` to delete an existing session and start fresh

## Tips

- **Click URLs**: Use `Cmd+Shift+Click` to open hyperlinks (Shift bypasses zellij's mouse handling)
- **Locked mode**: Zellij starts in locked mode to prevent accidental shortcuts. Press `Ctrl+G` to unlock when you need Zellij features.
