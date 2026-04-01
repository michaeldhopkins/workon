use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use super::{detect_git_remote, path_str, run_cmd, Vcs};

pub struct JjBackend;

/// One-time jj initialization for a git repo that doesn't have .jj yet.
pub(crate) fn init_jj(project_dir: &Path) -> Result<()> {
    eprintln!("Initializing jj colocated repo in {}...", project_dir.display());
    run_cmd("jj", &["git", "init", "--colocate", "-R", &path_str(project_dir)])?;

    let (main_branch, remote) = detect_trunk_git(project_dir);

    run_cmd(
        "jj",
        &[
            "bookmark", "track",
            &format!("{main_branch}@{remote}"),
            "-R", &path_str(project_dir),
        ],
    )?;

    let auto_track_key = format!("remotes.{remote}.auto-track-bookmarks");
    run_cmd(
        "jj",
        &[
            "config", "set", "--repo",
            &auto_track_key, "glob:*",
            "-R", &path_str(project_dir),
        ],
    )?;

    eprintln!("jj initialized, tracking {main_branch}@{remote}");
    Ok(())
}

/// Extract the first non-@git bookmark from jj's `bookmarks` template output.
/// Returns the full form (e.g. "master@heroku") so it resolves as a jj revision
/// even when the bookmark isn't tracked locally.
fn first_real_bookmark(raw: &str) -> &str {
    raw.split_whitespace()
        .find(|b| !b.ends_with("@git"))
        .unwrap_or("")
}

fn detect_trunk_git(project_dir: &Path) -> (String, String) {
    let remote = detect_git_remote(project_dir);
    let has_master = Command::new("git")
        .args(["-C", &path_str(project_dir), "rev-parse", "--verify", &format!("{remote}/master")])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());

    let branch = if has_master { "master" } else { "main" };
    (branch.into(), remote)
}

impl Vcs for JjBackend {
    fn detect_trunk(&self, project_dir: &Path) -> Result<String> {
        // trunk() works when the remote is named "origin"; fall back to
        // searching all remotes for repos with non-standard remote names.
        let revsets = [
            "trunk()",
            r#"latest(remote_bookmarks("master") | remote_bookmarks("main"))"#,
        ];
        for revset in &revsets {
            let output = Command::new("jj")
                .args([
                    "-R", &path_str(project_dir),
                    "log", "-r", revset,
                    "--no-graph", "-T", "bookmarks",
                    "--limit", "1",
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .ok();

            if let Some(output) = output {
                let raw = String::from_utf8_lossy(&output.stdout);
                let bookmark = first_real_bookmark(&raw);
                if !bookmark.is_empty() {
                    return Ok(bookmark.to_string());
                }
            }
        }

        Ok("main".into())
    }

    fn create_workspace(&self, project_dir: &Path, ws_dir: &Path, ws_id: &str, trunk: &str) -> Result<()> {
        eprintln!("Creating jj workspace {ws_id}...");
        let status = Command::new("jj")
            .args([
                "-R", &path_str(project_dir),
                "workspace", "add",
                &path_str(ws_dir),
                "--name", ws_id,
                "-r", trunk,
            ])
            .status()
            .context("failed to create jj workspace")?;

        if !status.success() {
            bail!("failed to create jj workspace");
        }

        // jj workspaces don't have a .git directory, so git commands
        // (branchdiff, git log, etc.) fail inside the workspace. Set up a
        // git worktree reference so git works alongside jj.
        if let Err(e) = setup_git_worktree(project_dir, ws_dir, ws_id, trunk) {
            eprintln!("Warning: could not set up git worktree for workspace: {e}");
        }

        Ok(())
    }

    fn pre_copy_sync(&self, project_dir: &Path) {
        // Running any jj command triggers an automatic snapshot in modern jj,
        // which ensures the git index is in sync with jj's working copy so
        // that git ls-files --ignored returns accurate results.
        let _ = Command::new("jj")
            .args(["-R", &path_str(project_dir), "status"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    fn changed_files(&self, ws_id: &str, project_dir: &Path, _ws_dir: &Path) -> Vec<String> {
        Command::new("jj")
            .args([
                "-R", &path_str(project_dir),
                "diff", "--ignore-working-copy",
                "-r", &format!("{ws_id}@"),
                "--summary",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter_map(|line| line.split_once(' ').map(|(_, path)| path.to_string()))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn save_work(&self, ws_id: &str, project_dir: &Path, _ws_dir: &Path) -> Result<()> {
        run_cmd(
            "jj",
            &[
                "-R", &path_str(project_dir),
                "bookmark", "set",
                &format!("workon/{ws_id}"),
                "-r", &format!("{ws_id}@"),
            ],
        )?;
        eprintln!("Bookmarked as workon/{ws_id}");
        Ok(())
    }

    fn forget_workspace(&self, ws_id: &str, project_dir: &Path, _ws_dir: &Path) {
        let _ = Command::new("jj")
            .args(["-R", &path_str(project_dir), "workspace", "forget", ws_id])
            .stderr(Stdio::null())
            .status();

        // Clean up the git worktree reference we created alongside the jj workspace.
        if let Some(git_dir) = absolute_git_dir(project_dir) {
            let wt_dir = format!("{git_dir}/worktrees/{ws_id}");
            let _ = std::fs::remove_dir_all(wt_dir);
        }

        eprintln!("Forgot jj workspace {ws_id}");
    }
}

fn absolute_git_dir(project_dir: &Path) -> Option<String> {
    Command::new("git")
        .args(["-C", &path_str(project_dir), "rev-parse", "--absolute-git-dir"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Set up a git worktree reference in a jj workspace so that git commands work.
///
/// jj workspaces don't create a `.git` entry, which means git commands,
/// branchdiff, and tools that expect a git repo all fail inside the workspace.
/// This creates the minimal git worktree plumbing: a `.git` file pointing to a
/// worktree entry under the main repo's `.git/worktrees/` directory.
fn setup_git_worktree(project_dir: &Path, ws_dir: &Path, ws_id: &str, trunk: &str) -> Result<()> {
    let git_dir = absolute_git_dir(project_dir)
        .context("could not determine .git directory")?;
    let wt_git_dir = format!("{git_dir}/worktrees/{ws_id}");

    std::fs::create_dir_all(&wt_git_dir)?;
    std::fs::write(format!("{wt_git_dir}/gitdir"), format!("{}/.git\n", path_str(ws_dir)))?;
    std::fs::write(format!("{wt_git_dir}/commondir"), "../..\n")?;

    // Resolve trunk to a commit hash for the worktree HEAD.
    let trunk_branch = trunk.split('@').next().unwrap_or(trunk);
    let remote = detect_git_remote(project_dir);
    let head_output = Command::new("git")
        .args(["-C", &path_str(project_dir), "rev-parse", &format!("{remote}/{trunk_branch}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());

    let head = head_output.unwrap_or_else(|| {
        Command::new("git")
            .args(["-C", &path_str(project_dir), "rev-parse", "HEAD"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default()
    });

    std::fs::write(format!("{wt_git_dir}/HEAD"), format!("{head}\n"))?;

    // Point the workspace at this worktree so git commands work.
    std::fs::write(ws_dir.join(".git"), format!("gitdir: {wt_git_dir}\n"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_real_bookmark_picks_non_git_entry() {
        assert_eq!(first_real_bookmark("master@heroku master@git"), "master@heroku");
    }

    #[test]
    fn first_real_bookmark_returns_bare_name() {
        assert_eq!(first_real_bookmark("main"), "main");
    }

    #[test]
    fn first_real_bookmark_skips_git_only() {
        assert_eq!(first_real_bookmark("main@git"), "");
    }

    #[test]
    fn first_real_bookmark_empty_input() {
        assert_eq!(first_real_bookmark(""), "");
        assert_eq!(first_real_bookmark("   "), "");
    }

    #[test]
    fn setup_git_worktree_enables_git_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        Command::new("git")
            .args(["init", "--initial-branch=main", &path_str(&project)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        Command::new("git")
            .args(["-C", &path_str(&project), "config", "user.email", "t@t.com"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git")
            .args(["-C", &path_str(&project), "config", "user.name", "T"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        std::fs::write(project.join("README"), "hi").unwrap();
        Command::new("git")
            .args(["-C", &path_str(&project), "add", "."])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git")
            .args(["-C", &path_str(&project), "commit", "-m", "init"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        setup_git_worktree(&project, &ws, "test-ws", "main").unwrap();

        assert!(ws.join(".git").is_file(), ".git file should exist in workspace");

        let log = Command::new("git")
            .args(["-C", &path_str(&ws), "log", "--oneline", "-1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .unwrap();
        assert!(log.status.success(), "git log should work in workspace");
        let output = String::from_utf8_lossy(&log.stdout);
        assert!(output.contains("init"), "should see the commit");
    }
}
