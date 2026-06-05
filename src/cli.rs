use clap::Parser;

#[derive(Parser)]
#[command(name = "workon", version, about = "Development workspace launcher with Zellij")]
#[allow(clippy::struct_excessive_bools)]
pub struct Cli {
    /// Name the session — and, with -w, the worktree. Defaults to the directory name.
    #[arg(long)]
    pub name: Option<String>,

    /// Ephemeral jj workspace mode
    #[arg(short = 'w')]
    pub workspace: bool,

    /// Force new session (delete existing, recover from hung server)
    #[arg(long, conflicts_with = "workspace")]
    pub new_session: bool,

    /// Skip copying gitignored files into the workspace
    #[arg(long, requires = "workspace")]
    pub skip_copy_ignored: bool,

    /// Resume a Claude session by ID (printed when a workspace exits)
    #[arg(short = 'r', long, requires = "workspace")]
    pub resume: Option<String>,

    /// Named config to load from ~/.config/workon/configs/<name>.kdl
    #[arg(short = 'c', long = "config")]
    pub config: Option<String>,

    /// Reserved. Was a `-n` alias for forcing a new session; now an inert
    /// no-op so the old reflex can't silently recreate a session. The
    /// deliberate spelling is `--new-session`.
    #[arg(short = 'n', hide = true)]
    pub reserved_n: bool,
}
