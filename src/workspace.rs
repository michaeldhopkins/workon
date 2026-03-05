use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use rand::Rng;

use crate::claude_trust;
use crate::session;

pub fn ensure_jj(project_dir: &Path) -> Result<()> {
    let has_git = project_dir.join(".git").is_dir();
    let has_jj = project_dir.join(".jj").is_dir();

    if has_git && !has_jj {
        eprintln!("Initializing jj colocated repo in {}...", project_dir.display());
        run_cmd("jj", &["git", "init", "--colocate", "-R", &path_str(project_dir)])?;

        let main_branch = detect_trunk(project_dir);

        run_cmd(
            "jj",
            &[
                "bookmark",
                "track",
                &format!("{main_branch}@origin"),
                "-R",
                &path_str(project_dir),
            ],
        )?;

        run_cmd(
            "jj",
            &[
                "config",
                "set",
                "--repo",
                "remotes.origin.auto-track-bookmarks",
                "glob:*",
                "-R",
                &path_str(project_dir),
            ],
        )?;

        eprintln!("jj initialized, tracking {main_branch}@origin");
    }
    Ok(())
}

pub fn run_workspace(project_dir: &Path, project_name: &str, layout: &Path) -> Result<()> {
    let ws_id = generate_ws_id();
    let ws_dir = home_dir()?
        .join(".worktrees")
        .join(format!("{project_name}-{ws_id}"));
    let tab_name = format!("{project_name}-{ws_id}");

    std::fs::create_dir_all(home_dir()?.join(".worktrees"))?;

    let main_branch = detect_trunk_jj(project_dir)?;

    eprintln!("Creating jj workspace {ws_id}...");
    let status = Command::new("jj")
        .args([
            "-R",
            &path_str(project_dir),
            "workspace",
            "add",
            &path_str(&ws_dir),
            "--name",
            &ws_id,
            "-r",
            &main_branch,
        ])
        .status()
        .context("failed to create jj workspace")?;

    if !status.success() {
        bail!("failed to create jj workspace");
    }

    let claude_dir = project_dir.join(".claude");
    if claude_dir.is_dir() {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&claude_dir, ws_dir.join(".claude"))
            .context("failed to symlink .claude")?;
    }

    let env_file = project_dir.join(".env");
    if env_file.is_file() {
        std::fs::copy(&env_file, ws_dir.join(".env"))?;
    }

    let mut created_db = None;
    if ws_dir.join("config/database.yml").is_file() {
        created_db = setup_rails_db(project_name, &ws_id, &ws_dir);
    }

    let _ = claude_trust::approve_workspace(&ws_dir);

    session::launch(&tab_name, layout, &ws_dir)?;

    cleanup(&ws_id, project_dir, &ws_dir, created_db.as_deref())
}

fn cleanup(
    ws_id: &str,
    project_dir: &Path,
    ws_dir: &Path,
    created_db: Option<&str>,
) -> Result<()> {
    eprintln!();
    eprintln!("Cleaning up workspace {ws_id}...");

    if has_uncommitted_changes(ws_id, project_dir) {
        eprintln!("Workspace has uncommitted changes.");
        eprint!("Auto-bookmark as workon/{ws_id}? [y/N] ");
        std::io::stderr().flush()?;

        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim().eq_ignore_ascii_case("y") {
            let _ = run_cmd(
                "jj",
                &[
                    "-R",
                    &path_str(project_dir),
                    "bookmark",
                    "set",
                    &format!("workon/{ws_id}"),
                    "-r",
                    &format!("{ws_id}@"),
                ],
            );
            eprintln!("Bookmarked as workon/{ws_id}");
        }
    }

    let _ = Command::new("jj")
        .args(["-R", &path_str(project_dir), "workspace", "forget", ws_id])
        .stderr(Stdio::null())
        .status();
    eprintln!("Forgot jj workspace {ws_id}");

    if let Some(db) = created_db {
        let _ = Command::new("dropdb")
            .arg(db)
            .stderr(Stdio::null())
            .status();
        eprintln!("Dropped test database {db}");
    }

    let _ = std::fs::remove_dir_all(ws_dir);
    eprintln!("Removed workspace directory");

    Ok(())
}

fn has_uncommitted_changes(ws_id: &str, project_dir: &Path) -> bool {
    Command::new("jj")
        .args([
            "-R",
            &path_str(project_dir),
            "log",
            "--ignore-working-copy",
            "-r",
            &format!("{ws_id}@"),
            "--no-graph",
            "-T",
            r#"if(empty, "", "changes")"#,
            "--limit",
            "1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| !o.stdout.is_empty() && String::from_utf8_lossy(&o.stdout).contains("changes"))
        .unwrap_or(false)
}

fn setup_rails_db(project_name: &str, ws_id: &str, ws_dir: &Path) -> Option<String> {
    let db_name = format!("{}_{}_test", project_name, ws_id).replace('-', "_");
    eprintln!("Creating test database {db_name}...");

    let ok = Command::new("createdb")
        .arg(&db_name)
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());

    if ok {
        let env_content = format!("DATABASE_URL=postgresql://localhost/{db_name}");
        let _ = std::fs::write(ws_dir.join(".env.test.local"), env_content);

        eprintln!("Loading schema...");
        let _ = Command::new("bundle")
            .args(["exec", "rails", "db:schema:load"])
            .env("RAILS_ENV", "test")
            .env(
                "DATABASE_URL",
                format!("postgresql://localhost/{db_name}"),
            )
            .current_dir(ws_dir)
            .status();

        Some(db_name)
    } else {
        eprintln!("Warning: could not create test database {db_name}");
        None
    }
}

fn generate_ws_id() -> String {
    let bytes: [u8; 3] = rand::rng().random();
    format!("ws-{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2])
}

fn detect_trunk(project_dir: &Path) -> String {
    let ok = Command::new("git")
        .args(["-C", &path_str(project_dir), "rev-parse", "--verify", "origin/master"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());

    if ok { "master".into() } else { "main".into() }
}

fn detect_trunk_jj(project_dir: &Path) -> Result<String> {
    let output = Command::new("jj")
        .args([
            "-R",
            &path_str(project_dir),
            "log",
            "-r",
            "trunk()",
            "--no-graph",
            "-T",
            "bookmarks",
            "--limit",
            "1",
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

fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {program}"))?;
    if !status.success() {
        bail!("{program} exited with status {status}");
    }
    Ok(())
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn home_dir() -> Result<PathBuf> {
    crate::home::home_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_id_format() {
        let id = generate_ws_id();
        assert!(id.starts_with("ws-"));
        assert_eq!(id.len(), 9); // "ws-" + 6 hex chars
        assert!(id[3..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn ws_id_is_random() {
        let a = generate_ws_id();
        let b = generate_ws_id();
        assert_ne!(a, b);
    }
}
