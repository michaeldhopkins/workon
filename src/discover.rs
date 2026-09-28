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
    assert_under(&worktrees_dir()?, ws_dir)
}

/// [`assert_under_worktrees`] against any root, so tests need not touch `$HOME`.
fn assert_under(worktrees: &Path, ws_dir: &Path) -> Result<()> {
    let root = std::fs::canonicalize(worktrees)
        .with_context(|| format!("{} does not exist", worktrees.display()))?;
    let target = std::fs::canonicalize(ws_dir)
        .with_context(|| format!("cannot resolve workspace path {}", ws_dir.display()))?;
    if !is_strictly_inside(&target, &root) {
        bail!(
            "refusing to operate on {} — not a workspace under {}",
            target.display(),
            root.display()
        );
    }
    Ok(())
}

/// `target` is below `root` and is not `root` itself. `Path::starts_with`
/// compares whole components, so `/w/worktrees-evil` is not inside `/w/worktrees`.
fn is_strictly_inside(target: &Path, root: &Path) -> bool {
    target != root && target.starts_with(root)
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

    /// A fresh `worktrees` root with one workspace in it, plus a directory beside
    /// the root. Canonical paths, since macOS's temp dir is behind `/var` -> `/private/var`.
    fn worktrees_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(tmp.path()).unwrap();
        let root = base.join("worktrees");
        let ws = root.join("proj-ws-abc123");
        let outside = base.join("outside");
        for d in [&ws, &outside] {
            std::fs::create_dir_all(d).unwrap();
        }
        (tmp, root, ws, outside)
    }

    #[test]
    fn assert_under_accepts_a_workspace_and_refuses_the_root_itself() {
        let (_tmp, root, ws, _) = worktrees_fixture();
        assert!(assert_under(&root, &ws).is_ok(), "a workspace directly under the root");
        let nested = ws.join("src");
        std::fs::create_dir(&nested).unwrap();
        assert!(assert_under(&root, &nested).is_ok(), "anything below the root");
        let err = assert_under(&root, &root).unwrap_err().to_string();
        assert!(err.contains("refusing to operate"), "destroying the root would remove every workspace: {err}");
    }

    #[test]
    fn assert_under_refuses_outside_paths_siblings_and_escapes() {
        let (_tmp, root, ws, outside) = worktrees_fixture();
        let sibling = root.with_file_name("worktrees-evil").join("proj-ws-abc123");
        std::fs::create_dir_all(&sibling).unwrap();
        let escape = ws.join("..").join("..").join("outside");
        for (why, path) in [
            ("a directory beside the root", outside.clone()),
            ("a sibling sharing the root's name as a prefix", sibling),
            ("a `..` escape that lexically starts under the root", escape),
            ("the root's parent", root.parent().unwrap().to_path_buf()),
            ("the filesystem root", PathBuf::from("/")),
        ] {
            let err = assert_under(&root, &path).expect_err(why).to_string();
            assert!(err.contains("refusing to operate"), "{why}: {err}");
        }
    }

    #[test]
    #[cfg(unix)]
    fn assert_under_follows_symlinks_to_where_they_resolve() {
        let (_tmp, root, ws, outside) = worktrees_fixture();
        let link_out = root.join("link-out");
        std::os::unix::fs::symlink(&outside, &link_out).unwrap();
        assert!(assert_under(&root, &link_out).is_err(), "a link under the root pointing outside");
        let link_in = outside.join("link-in");
        std::os::unix::fs::symlink(&ws, &link_in).unwrap();
        assert!(assert_under(&root, &link_in).is_ok(), "a link outside pointing at a workspace");
    }

    #[test]
    fn assert_under_errors_when_either_path_is_missing() {
        let (_tmp, root, ws, _) = worktrees_fixture();
        assert!(assert_under(&root, &root.join("gone")).is_err());
        assert!(assert_under(&root.join("no-such-root"), &ws).is_err());
    }

    proptest::proptest! {
        /// Inside means: more components than the root, and the root's components
        /// as a prefix. A two-letter alphabet makes shared name prefixes
        /// (`ab` vs `abb`) common, which is the case a string prefix gets wrong.
        #[test]
        fn is_strictly_inside_is_a_proper_component_prefix(
            root in proptest::collection::vec("[ab]{1,3}", 0..4),
            target in proptest::collection::vec("[ab]{1,3}", 0..6),
        ) {
            let as_path = |parts: &[String]| parts.iter().fold(PathBuf::from("/"), |p, c| p.join(c));
            let expected = target.len() > root.len() && target[..root.len()] == root[..];
            proptest::prop_assert_eq!(is_strictly_inside(&as_path(&target), &as_path(&root)), expected);
        }
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
