use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use super::{path_str, Vcs};

pub struct GitBackend;

impl Vcs for GitBackend {
    fn detect_trunk(&self, project_dir: &Path) -> Result<String> {
        let ok = Command::new("git")
            .args(["-C", &path_str(project_dir), "rev-parse", "--verify", "origin/master"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success());

        Ok(if ok { "master".into() } else { "main".into() })
    }

    fn create_workspace(&self, project_dir: &Path, ws_dir: &Path, ws_id: &str, trunk: &str) -> Result<()> {
        eprintln!("Creating git worktree {ws_id}...");
        let status = Command::new("git")
            .args([
                "-C", &path_str(project_dir),
                "worktree", "add",
                "--detach",
                &path_str(ws_dir),
                &format!("origin/{trunk}"),
            ])
            .status()
            .context("failed to create git worktree")?;

        if !status.success() {
            bail!("failed to create git worktree");
        }
        Ok(())
    }

    fn pre_copy_sync(&self, _project_dir: &Path) {
        // git worktrees have their own index; no sync needed.
    }

    fn has_uncommitted_changes(&self, _ws_id: &str, _project_dir: &Path, ws_dir: &Path) -> bool {
        Command::new("git")
            .args(["-C", &path_str(ws_dir), "status", "--porcelain"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map(|o| !o.stdout.is_empty())
            .unwrap_or(false)
    }

    fn save_work(&self, ws_id: &str, project_dir: &Path, ws_dir: &Path) -> Result<()> {
        // Stage everything and commit in the worktree
        let status = Command::new("git")
            .args(["-C", &path_str(ws_dir), "add", "-A"])
            .status()
            .context("failed to stage changes")?;
        if !status.success() {
            bail!("git add failed");
        }

        let status = Command::new("git")
            .args([
                "-C", &path_str(ws_dir),
                "commit", "-m", &format!("wip: workon/{ws_id}"),
            ])
            .status()
            .context("failed to commit changes")?;
        if !status.success() {
            bail!("git commit failed");
        }

        // Get the commit hash
        let output = Command::new("git")
            .args(["-C", &path_str(ws_dir), "rev-parse", "HEAD"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .context("failed to get commit hash")?;
        let hash = String::from_utf8_lossy(&output.stdout).trim().to_string();

        // Create a branch in the main repo pointing at that commit
        let status = Command::new("git")
            .args([
                "-C", &path_str(project_dir),
                "branch", &format!("workon/{ws_id}"), &hash,
            ])
            .status()
            .context("failed to create branch")?;
        if !status.success() {
            bail!("git branch creation failed");
        }

        eprintln!("Saved as branch workon/{ws_id}");
        Ok(())
    }

    fn forget_workspace(&self, _ws_id: &str, project_dir: &Path, ws_dir: &Path) {
        let _ = Command::new("git")
            .args([
                "-C", &path_str(project_dir),
                "worktree", "remove", "--force",
                &path_str(ws_dir),
            ])
            .stderr(Stdio::null())
            .status();
        eprintln!("Removed git worktree");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_repo_with_remote(tmp: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let origin = tmp.join("origin.git");
        let repo = tmp.join("repo");

        // Create a bare origin with explicit default branch
        Command::new("git").args(["init", "--bare", "--initial-branch=main", &path_str(&origin)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        // Clone it to get a working repo with origin remote
        Command::new("git").args(["clone", &path_str(&origin), &path_str(&repo)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        // Configure user for CI environments where global config is absent
        Command::new("git").args(["-C", &path_str(&repo), "config", "user.email", "test@test.com"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "config", "user.name", "Test"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        // Create initial commit and push
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
    fn has_uncommitted_changes_clean_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());

        let backend = GitBackend;
        assert!(!backend.has_uncommitted_changes("ws-test", &repo, &repo));
    }

    #[test]
    fn has_uncommitted_changes_dirty_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());

        std::fs::write(repo.join("new_file.txt"), "dirty").unwrap();

        let backend = GitBackend;
        assert!(backend.has_uncommitted_changes("ws-test", &repo, &repo));
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

        // Make a change in the worktree
        std::fs::write(ws_dir.join("work.txt"), "important work").unwrap();

        backend.save_work("ws-abc123", &repo, &ws_dir).unwrap();

        // Verify branch exists in main repo
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
