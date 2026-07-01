use clap::{Args, Parser, Subcommand};

#[derive(Parser)]
#[command(name = "workon", version, about = "Development workspace launcher with Zellij")]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub session: SessionArgs,
}

/// Flags for the default session / ephemeral `-w` flow. Flattened onto the root
/// so `workon`, `workon -w`, and `workon -c foo` keep parsing as before; they
/// conflict with the subcommands via `args_conflicts_with_subcommands`.
#[derive(Args)]
#[allow(clippy::struct_excessive_bools)]
pub struct SessionArgs {
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

/// Headless workspace lifecycle: `create -> (attach <-> quit)* -> destroy`.
/// Quitting an `attach` session detaches; the workspace survives until
/// `destroy`.
#[derive(Subcommand)]
pub enum Command {
    /// Provision a persistent workspace and print its path (no session)
    Create {
        /// Nickname for the worktree
        #[arg(long)]
        name: Option<String>,
        /// Named config to record for later `attach`
        #[arg(short = 'c', long = "config")]
        config: Option<String>,
        /// Skip copying gitignored files into the workspace
        #[arg(long)]
        skip_copy_ignored: bool,
        /// Print `{ ws_id, path, db }` as JSON on stdout
        #[arg(long)]
        json: bool,
    },
    /// Open an existing workspace in a session (workspace survives on quit)
    Attach {
        /// Workspace id, --name nickname, or path; omit for the cwd's workspace
        reference: Option<String>,
        /// Override the recorded config
        #[arg(short = 'c', long = "config")]
        config: Option<String>,
    },
    /// Tear down a workspace, saving rescued work unless --no-save
    Destroy {
        /// Workspace id, --name nickname, or path; omit for the cwd's workspace
        reference: Option<String>,
        /// Discard unsaved work instead of bookmarking it
        #[arg(long)]
        no_save: bool,
        /// Print `{ ws_id, saved, dropped_db }` as JSON on stdout
        #[arg(long)]
        json: bool,
    },
    /// List workspaces at or under the current directory
    List {
        /// Print an array of workspace objects as JSON on stdout
        #[arg(long)]
        json: bool,
    },
}
