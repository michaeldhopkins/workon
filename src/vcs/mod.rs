mod git;
mod jj;

use std::path::Path;

use anyhow::{bail, Result};
use vcs_runner::{jj_available, run_git_utf8};

pub use self::git::GitBackend;
pub use self::jj::JjBackend;

pub trait Vcs: Send + Sync {
    fn detect_trunk(&self, project_dir: &Path) -> Result<String>;

    /// Create the workspace branched from `trunk` and return the **immutable
    /// commit id** it was branched from. Teardown diffs against this pinned base
    /// rather than re-resolving `trunk` — a long session can outlive several
    /// fetches, and a trunk bookmark that moved would otherwise make unrelated
    /// upstream commits show up as the workspace's own "changed" files.
    fn create_workspace(&self, project_dir: &Path, ws_dir: &Path, ws_id: &str, trunk: &str) -> Result<String>;
    fn pre_copy_sync(&self, project_dir: &Path);

    /// Files in the workspace whose work would be lost on teardown — i.e.
    /// changes not already reachable from a bookmark/branch or a remote. Work
    /// you've already named or pushed is excluded, so a clean "finish, bookmark,
    /// leave" flow reports nothing and the save prompt stays silent. `base` is
    /// the pinned branch-point commit returned by `create_workspace`.
    fn changed_files(&self, ws_id: &str, base: &str, project_dir: &Path, ws_dir: &Path) -> Vec<String>;

    /// Bookmark/branch the unsaved in-stack work under `workon/<ws_id>`.
    fn save_work(&self, ws_id: &str, base: &str, project_dir: &Path, ws_dir: &Path) -> Result<()>;
    fn forget_workspace(&self, ws_id: &str, project_dir: &Path, ws_dir: &Path);

    /// One-line `<commit-id>  <desc>` rows for non-empty commits that **this
    /// workspace** stranded off its stack and that are still unsaved (no
    /// bookmark/branch, not pushed, unreachable). Attribution is per-workspace
    /// via the workspace's own pointer history — jj's operation log, git's
    /// per-worktree HEAD reflog — so a concurrent workspace's orphans are never
    /// misattributed here. `base` is the pinned branch point. Default: none.
    fn stranded_work(&self, _ws_id: &str, _base: &str, _project_dir: &Path, _ws_dir: &Path) -> Vec<String> {
        Vec::new()
    }

    /// Bookmark a single stranded commit under `workon/<ws_id>-<id>` so it
    /// survives teardown. Default: no-op.
    fn save_stranded(&self, _project_dir: &Path, _ws_id: &str, _commit_id: &str) -> Result<()> {
        Ok(())
    }

    /// Stop a workon-generated file (e.g. `.env.test.local`) from surfacing as
    /// a phantom change in the workspace. The file is per-workspace and must
    /// never be committed. Best-effort: failures are warned about, not fatal.
    fn ignore_generated_file(&self, project_dir: &Path, ws_dir: &Path, relpath: &str);
}

/// Detect VCS backend. jj preferred; git fallback when jj unavailable.
pub fn detect(project_dir: &Path) -> Result<Box<dyn Vcs>> {
    if project_dir.join(".jj").is_dir() {
        return Ok(Box::new(JjBackend));
    }

    let has_git = project_dir.join(".git").is_dir();
    if !has_git {
        bail!("not a git or jj repository");
    }

    if jj_available() {
        jj::init_jj(project_dir)?;
        return Ok(Box::new(JjBackend));
    }

    Ok(Box::new(GitBackend))
}

pub(crate) fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Append `pattern` to the repo's shared `info/exclude` if not already present.
///
/// Worktrees and jj workspaces share the common git dir's `info/exclude`, so a
/// pattern added here applies to every workspace. That's exactly right for
/// per-workspace generated files like `.env.test.local` — they should never be
/// tracked anywhere. Idempotent: a pattern already on its own line is left alone.
pub(crate) fn append_git_exclude(project_dir: &Path, pattern: &str) -> Result<()> {
    let git_dir = run_git_utf8(project_dir, &["rev-parse", "--absolute-git-dir"])
        .map_err(|e| anyhow::anyhow!("could not locate .git directory: {e}"))?;
    let exclude = Path::new(git_dir.trim()).join("info").join("exclude");

    let existing = std::fs::read_to_string(&exclude).unwrap_or_default();
    if existing.lines().any(|l| l.trim() == pattern) {
        return Ok(());
    }

    if let Some(parent) = exclude.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut content = existing;
    if !content.is_empty() && !content.ends_with('\n') {
        content.push('\n');
    }
    content.push_str(pattern);
    content.push('\n');
    std::fs::write(&exclude, content)?;
    Ok(())
}

/// Returns the name of the first git remote (usually "origin", but could be anything).
pub(crate) fn detect_git_remote(project_dir: &Path) -> String {
    run_git_utf8(project_dir, &["remote"])
        .ok()
        .and_then(|s| s.lines().next().map(|l| l.to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "origin".into())
}
