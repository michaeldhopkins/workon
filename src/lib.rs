//! Library surface of the `workon` crate. The binary (`main.rs`) is a thin
//! shim over these modules, and `examples/generate_assets.rs` reaches
//! `cli::Cli` through here to render the man page and completion scripts.

pub mod agent;
pub mod background;
pub mod claude_trust;
pub mod cli;
pub mod deps;
pub mod discover;
pub mod home;
pub mod layout;
pub mod mise_env;
pub mod not_ready;
pub mod provision;
pub mod resolve;
mod save_prompt;
pub mod session;
mod spawn_retry;
pub mod trust;
pub mod vcs;
pub mod workspace;
mod zellij_reply;

#[cfg(fuzzing)]
#[doc(hidden)]
pub mod fuzz_api;

/// The stand-in scripts tests write and then run, in one place so they are all
/// written clear of the fork race (see [`executable`]).
#[cfg(test)]
pub(crate) mod stand_in {
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;
    use std::process::Command;

    /// An executable script at `path` holding `contents`, written by a
    /// short-lived `sh` of its own rather than by this process.
    ///
    /// Written by this process, the script would be open for writing inside it
    /// for a moment, and any child a sibling test thread forks in that window
    /// inherits the write descriptor and holds it until the child execs — so a
    /// stand-in run right away can fail with Linux's ETXTBSY, "Text file
    /// busy" (macOS does not show the race). No process forks from the writer,
    /// and a stand-in runs only after the writer has exited, so the descriptor
    /// is held by nothing a fork can carry it in: the race cannot happen.
    /// `printf` is a shell builtin, so the writer needs nothing but `/bin/sh`.
    pub(crate) fn write(path: &Path, contents: &str) {
        let status = Command::new("/bin/sh")
            .args(["-c", "printf %s \"$WORKON_STAND_IN\" > \"$1\"", "workon stand-in writer"])
            .arg(path)
            .env("WORKON_STAND_IN", contents)
            .status()
            .unwrap_or_else(|e| panic!("spawning the writer of {}: {e}", path.display()));
        assert!(status.success(), "writing {}: {status}", path.display());
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
            .unwrap_or_else(|e| panic!("making {} executable: {e}", path.display()));
    }
}
