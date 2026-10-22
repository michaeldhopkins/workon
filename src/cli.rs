use std::ffi::{OsStr, OsString};

use clap::{Args, Parser, Subcommand};
use clap_complete::engine::ArgValueCandidates;

use crate::workspace;

#[derive(Parser, Debug)]
#[command(name = "workon", version, about = "Development workspace launcher with Zellij")]
#[command(args_conflicts_with_subcommands = true)]
pub struct Cli {
    /// Multiplexer for the session (zellij, tmux, or zmx)
    #[arg(long, global = true, value_parser = crate::mux::parse_mux, value_name = "MUX")]
    pub mux: Option<crate::mux::MuxKind>,

    #[command(subcommand)]
    pub command: Option<Command>,

    #[command(flatten)]
    pub session: SessionArgs,
}

/// Flags for the default session / ephemeral `-w` flow. Flattened onto the root
/// so `workon`, `workon -w`, and `workon -c foo` keep parsing as before; they
/// conflict with the subcommands via `args_conflicts_with_subcommands`.
#[derive(Args, Debug)]
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

    /// Resume an agent session by ID (printed when a workspace exits)
    #[arg(short = 'r', long, requires = "workspace")]
    pub resume: Option<String>,

    /// Named config to load from `~/.config/workon/configs/<name>.kdl`
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
#[derive(Subcommand, Debug)]
pub enum Command {
    /// Provision a persistent workspace and print its path (no session)
    #[command(
        after_help = "Exits 3 when a setup step failed: the workspace is kept, not ready, and `workon attach` opens it."
    )]
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
        /// Branch from this jj revision instead of the detected trunk, e.g. `default@-`
        #[arg(long, value_name = "REV")]
        base: Option<String>,
        /// Print `{ ws_id, path, dbs, failed_steps }` as JSON on stdout
        #[arg(long)]
        json: bool,
    },
    /// Open an existing workspace in a session (workspace survives on quit)
    Attach {
        /// Workspace id, --name nickname, or path; omit for the cwd's workspace
        #[arg(add = ArgValueCandidates::new(workspace::ref_candidates))]
        reference: Option<String>,
        /// Override the recorded config
        #[arg(short = 'c', long = "config")]
        config: Option<String>,
    },
    /// Tear down a workspace, saving rescued work unless --no-save
    Destroy {
        /// Workspace id, --name nickname, or path; omit for the cwd's workspace
        #[arg(add = ArgValueCandidates::new(workspace::ref_candidates))]
        reference: Option<String>,
        /// Discard unsaved work instead of bookmarking it
        #[arg(long)]
        no_save: bool,
        /// Print `{ ws_id, saved, dropped }` as JSON on stdout
        #[arg(long)]
        json: bool,
    },
    /// List workspaces at or under the current directory
    List {
        /// Print an array of workspace objects as JSON on stdout
        #[arg(long)]
        json: bool,
    },
    /// Print a workspace's directory, for `cd "$(workon path REF)"` (no session)
    Path {
        /// Workspace id, --name nickname, or path; omit for the cwd's workspace
        #[arg(add = ArgValueCandidates::new(workspace::ref_candidates))]
        reference: Option<String>,
    },
    /// Run a command in the workspace
    #[command(after_help = "Exits with the command's status.\nExample: workon exec feature-x -- bin/rails console")]
    Exec {
        /// Workspace id, --name nickname, or path; omit for the cwd's workspace
        #[arg(add = ArgValueCandidates::new(workspace::ref_candidates))]
        reference: Option<String>,
        /// The command and its arguments, after `--`
        #[arg(last = true, required = true, value_name = "COMMAND")]
        command: Vec<OsString>,
    },
    /// Start the session in the background
    Run {
        /// Name the session. Defaults to the directory name.
        #[arg(long)]
        name: Option<String>,
        /// Named config to load from `~/.config/workon/configs/<name>.kdl`
        #[arg(short = 'c', long = "config")]
        config: Option<String>,
        /// Run this in the session once it is up
        #[arg(long)]
        command: Option<String>,
    },
    /// Type text into the running session
    Send {
        /// Text to type, followed by Enter
        text: String,
        /// Name the session. Defaults to the directory name.
        #[arg(long)]
        name: Option<String>,
    },
    /// Print recent output from the session
    History {
        /// Name the session. Defaults to the directory name.
        #[arg(long)]
        name: Option<String>,
        /// How many lines to print
        #[arg(long, default_value_t = 200)]
        lines: usize,
    },
    /// Wait until the session output contains a line of text
    Wait {
        /// Text to wait for
        #[arg(long = "for")]
        text: String,
        /// Name the session. Defaults to the directory name.
        #[arg(long)]
        name: Option<String>,
        /// Give up after this many seconds
        #[arg(long, default_value_t = 30)]
        timeout: u64,
    },
}

/// `--name ""` counts as no name, so the caller falls back to its default
/// (the directory name, or an unlabelled ws_id).
pub fn given_name(name: Option<&str>) -> Option<&str> {
    name.filter(|s| !s.is_empty())
}

/// Move a leading `--mux` to the end.
///
/// Session flags conflict with subcommands (`args_conflicts_with_subcommands`),
/// and clap applies that to every root flag, including this global one. Putting
/// `--mux` last lets `workon --mux tmux run` parse as `workon run --mux tmux`.
pub fn normalize_argv<I, T>(args: I) -> Vec<OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    let Some((program, rest)) = args.split_first() else {
        return args;
    };
    let mut out = Vec::with_capacity(args.len());
    out.push(program.clone());
    let mut mux = Vec::new();
    let mut literal = false;
    let mut rest = rest.iter();
    while let Some(arg) = rest.next() {
        if !literal && arg == "--" {
            literal = true;
            out.push(arg.clone());
            continue;
        }
        let equals = arg.to_string_lossy().starts_with("--mux=");
        if !literal && mux.is_empty() && (arg == OsStr::new("--mux") || equals) {
            mux.push(arg.clone());
            if arg == OsStr::new("--mux")
                && let Some(value) = rest.next()
            {
                mux.push(value.clone());
            }
            continue;
        }
        out.push(arg.clone());
    }
    out.extend(mux);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mux_on_a_subcommand_reaches_the_root() {
        let cli = Cli::try_parse_from(normalize_argv(["workon", "run", "--mux", "tmux"])).unwrap();
        assert_eq!(cli.mux, Some(crate::mux::MuxKind::Tmux));
        let cli = Cli::try_parse_from(normalize_argv(["workon", "--mux", "zmx", "history"])).unwrap();
        assert_eq!(cli.mux, Some(crate::mux::MuxKind::Zmx));
    }

    #[test]
    fn an_unknown_mux_names_the_three() {
        let err = Cli::try_parse_from(["workon", "--mux", "screen"]).unwrap_err();
        assert!(err.to_string().contains("unknown multiplexer 'screen'. workon supports zellij, tmux, and zmx."));
    }

    #[test]
    fn empty_name_is_no_name() {
        assert_eq!(given_name(Some("fix-bug")), Some("fix-bug"));
        assert_eq!(given_name(Some("")), None);
        assert_eq!(given_name(None), None);
    }

    proptest::proptest! {
        /// A name survives exactly when it is non-empty, and then comes back unchanged.
        #[test]
        fn given_name_drops_only_the_empty_name(name in "\\PC{0,12}") {
            let expected = if name.is_empty() { None } else { Some(name.as_str()) };
            proptest::prop_assert_eq!(given_name(Some(&name)), expected);
        }
    }
}
