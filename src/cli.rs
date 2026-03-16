use clap::Parser;

#[derive(Parser)]
#[command(name = "workon", version, about = "Development workspace launcher with Zellij")]
#[allow(clippy::struct_excessive_bools)]
pub struct Cli {
    /// Project path or ~/workspace/<name>
    pub project: Option<String>,

    /// Force new session (delete existing, recover from hung server)
    #[arg(short = 'n', conflicts_with = "workspace")]
    pub new_session: bool,

    /// Ephemeral jj workspace mode, optionally with a label for the session
    #[arg(short = 'w', num_args = 0..=1, default_missing_value = "")]
    pub workspace: Option<String>,

    /// Skip copying gitignored files into the workspace
    #[arg(long, requires = "workspace")]
    pub skip_copy_ignored: bool,

    /// Resume a Claude session by ID (printed when a workspace exits)
    #[arg(short = 'r', long, requires = "workspace")]
    pub resume: Option<String>,
}
