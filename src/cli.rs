use clap::Parser;

#[derive(Parser)]
#[command(name = "workon", version, about = "Development workspace launcher with Zellij")]
pub struct Cli {
    /// Project path or ~/workspace/<name>
    pub project: Option<String>,

    /// Force new session (delete existing, recover from hung server)
    #[arg(short = 'n', conflicts_with = "workspace")]
    pub new_session: bool,

    /// Ephemeral jj workspace mode
    #[arg(short = 'w')]
    pub workspace: bool,
}
