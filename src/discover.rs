//! Live inference of workspace structure from the filesystem plus git/jj, so the
//! headless subcommands need no registry. The worktrees under `~/.worktrees` are
//! the source of truth; this module reads facts back off them. Anything stored
//! (in `.workon.json`) is only what can't be inferred — see
//! specs/non-interactive-workspaces.md.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use vcs_runner::run_git_utf8;

/// How a workspace reference on the command line is interpreted.
#[derive(Debug, PartialEq, Eq)]
pub enum WsRef {
    /// No reference given — use the workspace the cwd sits inside.
    Cwd,
    /// A filesystem path to the worktree.
    Path(PathBuf),
    /// A ws_id or a `--name` nickname to look up under `~/.worktrees`.
    Token(String),
}

/// `~/.worktrees` — the flat directory every workon workspace lives under.
pub fn worktrees_dir() -> Result<PathBuf> {
    Ok(crate::home::home_dir()?.join(".worktrees"))
}

/// Classify a CLI reference. A value containing `/` or starting with `.`/`~` is
/// a path; anything else is a bare token (ws_id or nickname). `None` means cwd.
pub fn classify_ref(reference: Option<&str>) -> WsRef {
    match reference {
        None => WsRef::Cwd,
        Some(s) if looks_like_path(s) => WsRef::Path(PathBuf::from(s)),
        Some(s) => WsRef::Token(s.to_string()),
    }
}

fn looks_like_path(s: &str) -> bool {
    s.contains('/') || s.starts_with('.') || s.starts_with('~')
}

/// Recover the ws_id from a worktree dir by stripping the `<project_name>-`
/// prefix its name carries (e.g. `mbc-ws-abc123-fix` under project `mbc` ->
/// `ws-abc123-fix`). `None` if the name doesn't carry that prefix.
pub fn ws_id_of(ws_dir: &Path, project_name: &str) -> Option<String> {
    let base = ws_dir.file_name()?.to_str()?;
    base.strip_prefix(&format!("{project_name}-")).map(String::from)
}

/// The project directory a worktree belongs to: the parent of its common git
/// dir. Works for git worktrees and for jj workspaces, which workon backs with a
/// git worktree pointer so `git -C <ws> rev-parse` resolves to the main repo.
pub fn project_dir_of(ws_dir: &Path) -> Result<PathBuf> {
    let common = run_git_utf8(ws_dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .with_context(|| format!("{} is not inside a git/jj worktree", ws_dir.display()))?;
    let common = PathBuf::from(common.trim());
    common
        .parent()
        .map(Path::to_path_buf)
        .with_context(|| format!("git common dir {} has no parent", common.display()))
}

/// The immediate directory entries under `~/.worktrees` (each a workspace).
/// Empty when the directory doesn't exist yet.
pub fn list_worktree_dirs() -> Result<Vec<PathBuf>> {
    Ok(list_dirs_in(&worktrees_dir()?))
}

/// Immediate subdirectories of `root`, sorted. Empty if `root` is missing.
/// Split from `list_worktree_dirs` so callers (and tests) can scan an arbitrary
/// worktrees root.
pub fn list_dirs_in(root: &Path) -> Vec<PathBuf> {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return Vec::new(),
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// Refuse any path that isn't a proper subdirectory of the canonicalized
/// `~/.worktrees`. This is the guard that keeps `destroy` from `rm -rf`-ing an
/// arbitrary location: `..` segments and symlink escapes are resolved away
/// before the check. Requires `ws_dir` to exist (so it can be canonicalized).
pub fn assert_under_worktrees(ws_dir: &Path) -> Result<()> {
    let root = std::fs::canonicalize(worktrees_dir()?)
        .context("~/.worktrees does not exist")?;
    let target = std::fs::canonicalize(ws_dir)
        .with_context(|| format!("cannot resolve workspace path {}", ws_dir.display()))?;
    if target == root || !target.starts_with(&root) {
        bail!(
            "refusing to operate on {} — not a workspace under {}",
            target.display(),
            root.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_ref_distinguishes_path_id_and_cwd() {
        assert_eq!(classify_ref(None), WsRef::Cwd);
        assert_eq!(classify_ref(Some("ws-abc123")), WsRef::Token("ws-abc123".into()));
        assert_eq!(classify_ref(Some("fix-bug")), WsRef::Token("fix-bug".into()));
        assert_eq!(classify_ref(Some("/abs/path")), WsRef::Path("/abs/path".into()));
        assert_eq!(classify_ref(Some("./rel")), WsRef::Path("./rel".into()));
        assert_eq!(classify_ref(Some("a/b")), WsRef::Path("a/b".into()));
        assert_eq!(classify_ref(Some("~/w")), WsRef::Path("~/w".into()));
    }

    #[test]
    fn ws_id_of_strips_project_prefix() {
        assert_eq!(
            ws_id_of(Path::new("/w/mbc-ws-abc123"), "mbc"),
            Some("ws-abc123".to_string())
        );
        // Nickname suffix stays part of the ws_id.
        assert_eq!(
            ws_id_of(Path::new("/w/mbc-ws-abc123-fix-bug"), "mbc"),
            Some("ws-abc123-fix-bug".to_string())
        );
    }

    #[test]
    fn ws_id_of_none_when_prefix_absent() {
        // A hyphenated project name must still match as a whole prefix, not a
        // partial one.
        assert_eq!(ws_id_of(Path::new("/w/other-ws-abc"), "mbc"), None);
        assert_eq!(ws_id_of(Path::new("/w/mb-ws-abc"), "mbc"), None);
    }

    #[test]
    fn assert_under_worktrees_rejects_traversal_and_outside() {
        // Outside ~/.worktrees entirely.
        assert!(assert_under_worktrees(Path::new("/etc")).is_err());
        // A `..` escape that lexically looks nested but resolves outside — this
        // is the case a naive starts_with would wrongly allow.
        let escape = worktrees_dir().unwrap().join("..").join("..").join("etc");
        assert!(assert_under_worktrees(&escape).is_err());
    }
}
