use std::path::Path;

use anyhow::{Context, Result};
use vcs_runner::{run_git, run_git_utf8};

use super::{detect_git_remote, path_str, Vcs};

pub struct GitBackend;

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

    fn changed_files(&self, _ws_id: &str, _project_dir: &Path, ws_dir: &Path) -> Vec<String> {
        // Use run_git (not run_git_utf8) — we must NOT trim. `git status --porcelain`
        // emits lines like " M path" for modified-unstaged files; trimming the leading
        // space corrupts line.get(3..).
        run_git(ws_dir, &["status", "--porcelain"])
            .map(|out| {
                out.stdout_lossy()
                    .lines()
                    .filter_map(|line| line.get(3..).map(|p| p.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn save_work(&self, ws_id: &str, project_dir: &Path, ws_dir: &Path) -> Result<()> {
        run_git(ws_dir, &["add", "-A"]).context("failed to stage changes")?;
        run_git(ws_dir, &["commit", "-m", &format!("wip: workon/{ws_id}")]).context("failed to commit changes")?;

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
}
