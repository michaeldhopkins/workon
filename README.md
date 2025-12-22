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
source ~/projects/rust/workon/workon/shell/init.sh
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
```

If any dependencies are missing, workon will list them and exit.

## Tips

- **Click URLs**: Use `Cmd+Shift+Click` to open hyperlinks (Shift bypasses zellij's mouse handling)
- **Word navigation**: `Option+Left/Right` arrows work for moving between words
