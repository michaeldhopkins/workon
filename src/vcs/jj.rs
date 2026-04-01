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
        eprintln!("Forgot jj workspace {ws_id}");
    }
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
}
