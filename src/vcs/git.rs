use std::path::Path;

use anyhow::{Context, Result};
use vcs_runner::{run_git, run_git_utf8};

use super::{detect_git_remote, path_str, Vcs};

pub struct GitBackend;

impl GitBackend {
    /// The remote-tracking trunk ref the worktree was branched from, e.g.
    /// `origin/master`.
    fn trunk_ref(&self, project_dir: &Path) -> String {
        let remote = detect_git_remote(project_dir);
        let trunk = self.detect_trunk(project_dir).unwrap_or_else(|_| "main".into());
        format!("{remote}/{trunk}")
    }

    /// Whether the worktree's HEAD has commits ahead of trunk that no branch or
    /// remote ref contains — i.e. committed work that teardown would orphan.
    fn has_unnamed_commits(&self, project_dir: &Path, ws_dir: &Path) -> bool {
        let trunk = self.trunk_ref(project_dir);
        let ahead = run_git_utf8(ws_dir, &["rev-list", &format!("{trunk}..HEAD")])
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !ahead {
            return false;
        }
        // If any local branch or remote ref already contains HEAD, the work is
        // saved (named or pushed) and shouldn't prompt.
        let contained = run_git_utf8(
            ws_dir,
            &["for-each-ref", "--contains", "HEAD", "--format=%(refname)", "refs/heads", "refs/remotes"],
        )
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
        !contained
    }
}

impl Vcs for GitBackend {
    fn detect_trunk(&self, project_dir: &Path) -> Result<String> {
        let remote = detect_git_remote(project_dir);
        let ok = run_git(project_dir, &["rev-parse", "--verify", &format!("{remote}/master")]).is_ok();

        Ok(if ok { "master".into() } else { "main".into() })
    }

    fn create_workspace(&self, project_dir: &Path, ws_dir: &Path, ws_id: &str, trunk: &str) -> Result<()> {
        eprintln!("Creating git worktree {ws_id}...");
        let remote = detect_git_remote(project_dir);
        run_git(
            project_dir,
            &["worktree", "add", "--detach", &path_str(ws_dir), &format!("{remote}/{trunk}")],
        )
        .context("failed to create git worktree")?;
        Ok(())
    }

    fn pre_copy_sync(&self, _project_dir: &Path) {
        // git worktrees have their own index; no sync needed.
    }

    fn changed_files(&self, _ws_id: &str, project_dir: &Path, ws_dir: &Path) -> Vec<String> {
        // Two sources of would-vanish work in a worktree (jj only has the
        // second, since it auto-snapshots): a dirty tree, and commits on the
        // detached HEAD that no branch or remote names.
        let mut files: Vec<String> = Vec::new();

        // 1. Uncommitted changes. Use run_git (not run_git_utf8) — we must NOT
        // trim: `git status --porcelain` emits " M path" for modified-unstaged
        // files; trimming the leading space corrupts line.get(3..). Generated
        // files are already dropped via .git/info/exclude.
        if let Ok(out) = run_git(ws_dir, &["status", "--porcelain"]) {
            files.extend(out.stdout_lossy().lines().filter_map(|line| line.get(3..).map(|p| p.to_string())));
        }

        // 2. Committed work not reachable from any branch or remote — lost when
        // the worktree is removed (reflog only).
        if self.has_unnamed_commits(project_dir, ws_dir) {
            let trunk = self.trunk_ref(project_dir);
            if let Ok(out) = run_git(ws_dir, &["diff", "--name-only", &format!("{trunk}...HEAD")]) {
                files.extend(out.stdout_lossy().lines().map(str::to_string));
            }
        }

        files.sort();
        files.dedup();
        files
    }

    fn save_work(&self, ws_id: &str, project_dir: &Path, ws_dir: &Path) -> Result<()> {
        // Commit any uncommitted changes; a clean tree just means the work is
        // already committed on the detached HEAD (still unnamed until we branch).
        let _ = run_git(ws_dir, &["add", "-A"]);
        let _ = run_git(ws_dir, &["commit", "-m", &format!("wip: workon/{ws_id}")]);

        let hash = run_git_utf8(ws_dir, &["rev-parse", "HEAD"]).context("failed to get commit hash")?;

        run_git(project_dir, &["branch", &format!("workon/{ws_id}"), &hash])
            .context("failed to create branch")?;

        eprintln!("Saved as branch workon/{ws_id}");
        Ok(())
    }

    fn forget_workspace(&self, _ws_id: &str, project_dir: &Path, ws_dir: &Path) {
        let _ = run_git(project_dir, &["worktree", "remove", "--force", &path_str(ws_dir)]);
        eprintln!("Removed git worktree");
    }

    fn ignore_generated_file(&self, project_dir: &Path, _ws_dir: &Path, relpath: &str) {
        if let Err(e) = super::append_git_exclude(project_dir, relpath) {
            eprintln!("Warning: could not exclude {relpath} from git: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use super::*;

    fn init_repo_with_remote(tmp: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let origin = tmp.join("origin.git");
        let repo = tmp.join("repo");

        Command::new("git").args(["init", "--bare", "--initial-branch=main", &path_str(&origin)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        Command::new("git").args(["clone", &path_str(&origin), &path_str(&repo)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        Command::new("git").args(["-C", &path_str(&repo), "config", "user.email", "test@test.com"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "config", "user.name", "Test"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        std::fs::write(repo.join("README.md"), "hello").unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "add", "."])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "commit", "-m", "init"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "push"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        (origin, repo)
    }

    fn init_repo_with_named_remote(tmp: &Path, remote_name: &str, branch: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let origin = tmp.join("origin.git");
        let repo = tmp.join("repo");

        Command::new("git").args(["init", "--bare", &format!("--initial-branch={branch}"), &path_str(&origin)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        Command::new("git").args(["clone", "-o", remote_name, &path_str(&origin), &path_str(&repo)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        Command::new("git").args(["-C", &path_str(&repo), "config", "user.email", "test@test.com"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "config", "user.name", "Test"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        std::fs::write(repo.join("README.md"), "hello").unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "add", "."])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "commit", "-m", "init"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "push"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        (origin, repo)
    }

    #[test]
    fn detect_trunk_with_non_origin_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_named_remote(tmp.path(), "heroku", "master");

        let backend = GitBackend;
        let trunk = backend.detect_trunk(&repo).unwrap();
        assert_eq!(trunk, "master");
    }

    #[test]
    fn create_worktree_with_non_origin_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_named_remote(tmp.path(), "heroku", "master");
        let ws_dir = tmp.path().join("worktree");

        let backend = GitBackend;
        backend.create_workspace(&repo, &ws_dir, "ws-test", "master").unwrap();
        assert!(ws_dir.join("README.md").exists());

        backend.forget_workspace("ws-test", &repo, &ws_dir);
    }

    #[test]
    fn changed_files_clean_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());

        let backend = GitBackend;
        assert!(backend.changed_files("ws-test", &repo, &repo).is_empty());
    }

    #[test]
    fn changed_files_dirty_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());

        std::fs::write(repo.join("new_file.txt"), "dirty").unwrap();

        let backend = GitBackend;
        let files = backend.changed_files("ws-test", &repo, &repo);
        assert_eq!(files, vec!["new_file.txt"]);
    }

    /// Regression: `git status --porcelain` emits " M path" (leading space) for
    /// modified-unstaged files. Earlier versions trimmed the whole stdout, eating
    /// the leading space and corrupting line.get(3..).
    #[test]
    fn changed_files_modified_unstaged_preserves_leading_space() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());

        // README.md was committed by init_repo_with_remote — modify it without staging.
        std::fs::write(repo.join("README.md"), "modified content").unwrap();

        let backend = GitBackend;
        let files = backend.changed_files("ws-test", &repo, &repo);
        assert_eq!(files, vec!["README.md"]);
    }

    #[test]
    fn create_and_forget_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());
        let ws_dir = tmp.path().join("worktree");

        let backend = GitBackend;
        backend.create_workspace(&repo, &ws_dir, "ws-test", "main").unwrap();

        assert!(ws_dir.join("README.md").exists());

        backend.forget_workspace("ws-test", &repo, &ws_dir);
        assert!(!ws_dir.exists());
    }

    #[test]
    fn ignore_generated_file_excludes_from_git_status() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());

        std::fs::write(repo.join(".env.test.local"), "DATABASE_URL=postgresql://localhost/x").unwrap();
        // Before excluding, the generated file shows up as a change.
        let backend = GitBackend;
        assert_eq!(backend.changed_files("ws-test", &repo, &repo), vec![".env.test.local"]);

        backend.ignore_generated_file(&repo, &repo, ".env.test.local");

        assert!(
            backend.changed_files("ws-test", &repo, &repo).is_empty(),
            "excluded file should not surface in git status"
        );
    }

    #[test]
    fn ignore_generated_file_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());

        let backend = GitBackend;
        backend.ignore_generated_file(&repo, &repo, ".env.test.local");
        backend.ignore_generated_file(&repo, &repo, ".env.test.local");

        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        let occurrences = exclude.lines().filter(|l| l.trim() == ".env.test.local").count();
        assert_eq!(occurrences, 1, "pattern should be written exactly once");
    }

    #[test]
    fn save_work_creates_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());
        let ws_dir = tmp.path().join("worktree");

        let backend = GitBackend;
        backend.create_workspace(&repo, &ws_dir, "ws-abc123", "main").unwrap();

        std::fs::write(ws_dir.join("work.txt"), "important work").unwrap();

        backend.save_work("ws-abc123", &repo, &ws_dir).unwrap();

        let output = Command::new("git")
            .args(["-C", &path_str(&repo), "branch", "--list", "workon/ws-abc123"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .unwrap();
        let branches = String::from_utf8_lossy(&output.stdout);
        assert!(branches.contains("workon/ws-abc123"), "branch should exist in main repo");

        backend.forget_workspace("ws-abc123", &repo, &ws_dir);
    }

    fn git_c(dir: &Path, args: &[&str]) {
        let dir_path = path_str(dir);
        let mut full = vec!["-C", &*dir_path];
        full.extend_from_slice(args);
        Command::new("git").args(&full)
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
    }

    /// Regression for the git analog of the jj bug: an agent that commits in a
    /// worktree leaves a clean tree, so the old `git status` check saw nothing
    /// and `git worktree remove --force` would orphan the commit.
    #[test]
    fn changed_files_detects_unnamed_committed_work() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());
        let ws = tmp.path().join("wt");
        let backend = GitBackend;
        backend.create_workspace(&repo, &ws, "ws-x", "main").unwrap();

        std::fs::write(ws.join("feature.txt"), "work").unwrap();
        git_c(&ws, &["add", "."]);
        git_c(&ws, &["commit", "-m", "feature work"]);

        let changed = backend.changed_files("ws-x", &repo, &ws);
        assert!(changed.contains(&"feature.txt".to_string()), "unnamed committed work should be detected, got {changed:?}");

        backend.forget_workspace("ws-x", &repo, &ws);
    }

    /// Branch-aware: committed work that's been put on a branch is saved, so it
    /// must not prompt — the git analog of jj's bookmark-aware silence.
    #[test]
    fn changed_files_silent_for_branched_work() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());
        let ws = tmp.path().join("wt");
        let backend = GitBackend;
        backend.create_workspace(&repo, &ws, "ws-x", "main").unwrap();

        std::fs::write(ws.join("feature.txt"), "work").unwrap();
        git_c(&ws, &["add", "."]);
        git_c(&ws, &["commit", "-m", "feature work"]);
        git_c(&ws, &["checkout", "-b", "feature"]);

        assert!(backend.changed_files("ws-x", &repo, &ws).is_empty(), "work on a branch is saved");

        backend.forget_workspace("ws-x", &repo, &ws);
    }

    /// save_work must handle a clean tree (work already committed) without
    /// failing on "nothing to commit", and still branch the committed work.
    #[test]
    fn save_work_branches_already_committed_work() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());
        let ws = tmp.path().join("wt");
        let backend = GitBackend;
        backend.create_workspace(&repo, &ws, "ws-y", "main").unwrap();

        std::fs::write(ws.join("feature.txt"), "work").unwrap();
        git_c(&ws, &["add", "."]);
        git_c(&ws, &["commit", "-m", "feature work"]);
        let head = run_git_utf8(&ws, &["rev-parse", "HEAD"]).unwrap();

        backend.save_work("ws-y", &repo, &ws).unwrap();

        let branch_sha = run_git_utf8(&repo, &["rev-parse", "workon/ws-y"]).unwrap();
        assert_eq!(branch_sha.trim(), head.trim(), "branch should point at the committed work");

        backend.forget_workspace("ws-y", &repo, &ws);
    }
}
