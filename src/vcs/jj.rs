use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};

use super::{path_str, run_cmd, Vcs};

pub struct JjBackend;

/// One-time jj initialization for a git repo that doesn't have .jj yet.
pub(crate) fn init_jj(project_dir: &Path) -> Result<()> {
    eprintln!("Initializing jj colocated repo in {}...", project_dir.display());
    run_cmd("jj", &["git", "init", "--colocate", "-R", &path_str(project_dir)])?;

    let main_branch = detect_trunk_git(project_dir);

    run_cmd(
        "jj",
        &[
            "bookmark", "track",
            &format!("{main_branch}@origin"),
            "-R", &path_str(project_dir),
        ],
    )?;

    run_cmd(
        "jj",
        &[
            "config", "set", "--repo",
            "remotes.origin.auto-track-bookmarks", "glob:*",
            "-R", &path_str(project_dir),
        ],
    )?;

    eprintln!("jj initialized, tracking {main_branch}@origin");
    Ok(())
}

fn detect_trunk_git(project_dir: &Path) -> String {
    let ok = Command::new("git")
        .args(["-C", &path_str(project_dir), "rev-parse", "--verify", "origin/master"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());

    if ok { "master".into() } else { "main".into() }
}

impl Vcs for JjBackend {
    fn detect_trunk(&self, project_dir: &Path) -> Result<String> {
        let output = Command::new("jj")
            .args([
                "-R", &path_str(project_dir),
                "log", "-r", "trunk()",
                "--no-graph", "-T", "bookmarks",
                "--limit", "1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .context("failed to detect trunk branch")?;

        let raw = String::from_utf8_lossy(&output.stdout);
        let branch = raw.split('@').next().unwrap_or("main").trim();
        if branch.is_empty() {
            Ok("main".into())
        } else {
            Ok(branch.into())
        }
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
        Ok(())
    }

    fn pre_copy_sync(&self, project_dir: &Path) {
        // Snapshot ensures the git index is in sync with jj's working copy,
        // so git ls-files --ignored returns accurate results.
        let _ = Command::new("jj")
            .args(["-R", &path_str(project_dir), "snapshot"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }

    fn has_uncommitted_changes(&self, ws_id: &str, project_dir: &Path, _ws_dir: &Path) -> bool {
        Command::new("jj")
            .args([
                "-R", &path_str(project_dir),
                "log", "--ignore-working-copy",
                "-r", &format!("{ws_id}@"),
                "--no-graph",
                "-T", r#"if(empty, "", "changes")"#,
                "--limit", "1",
            ])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .map(|o| !o.stdout.is_empty() && String::from_utf8_lossy(&o.stdout).contains("changes"))
            .unwrap_or(false)
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
        eprintln!("Forgot jj workspace {ws_id}");
    }
}
