use std::path::Path;
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;
use vcs_runner::Cmd;

const ZELLIJ_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(name: &str, layout: &Path, working_dir: &Path, force_new: bool) -> Result<()> {
    let empty = std::collections::HashMap::new();
    if session_exists(name)? {
        if force_new {
            delete_session(name)?;
            launch(name, layout, working_dir, &empty)
        } else {
            attach(name, working_dir)
        }
    } else {
        launch(name, layout, working_dir, &empty)
    }
}

fn session_exists(name: &str) -> Result<bool> {
    let stdout = match zellij_with_timeout(&["list-sessions", "--no-formatting"]) {
        Ok(s) => s,
        Err(_) => return Ok(false),
    };

    Ok(stdout.lines().any(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(|first| first == name)
    }))
}

fn delete_session(name: &str) -> Result<()> {
    let _ = zellij_with_timeout(&["delete-session", name, "--force"]);
    Ok(())
}

fn zellij_with_timeout(args: &[&str]) -> Result<String> {
    match Cmd::new("zellij").args(args).timeout(ZELLIJ_TIMEOUT).run() {
        Ok(output) => Ok(output.stdout_lossy().into_owned()),
        Err(ref e) if e.is_timeout() => {
            kill_zellij_server();
            anyhow::bail!("zellij command timed out; killed hung server")
        }
        Err(e) => Err(e.into()),
    }
}

fn kill_zellij_server() {
    eprintln!("Warning: zellij server appears hung, killing it...");
    let _ = Cmd::new("pkill").args(["-9", "-f", "zellij-server"]).run();
    if let Ok(output) = Cmd::new("id").arg("-u").run() {
        let uid = output.stdout_lossy().trim().to_string();
        let socket_dir = format!("/tmp/zellij-{uid}");
        let _ = std::fs::remove_dir_all(socket_dir);
    }
}

pub fn launch(
    name: &str,
    layout: &Path,
    working_dir: &Path,
    extra_env: &std::collections::HashMap<String, String>,
) -> Result<()> {
    let config = locked_config()?;

    // Interactive: needs full TTY (stdin/stdout/stderr inherited from parent),
    // which procpilot's Cmd doesn't support — use std::process::Command directly.
    Command::new("zellij")
        .args([
            "--new-session-with-layout",
            &layout.to_string_lossy(),
            "--session",
            name,
        ])
        .env("ZELLIJ_CONFIG_FILE", config.path())
        .envs(extra_env)
        .current_dir(working_dir)
        .status()
        .context("failed to launch zellij session")?;
    Ok(())
}

fn attach(name: &str, working_dir: &Path) -> Result<()> {
    Command::new("zellij")
        .args(["attach", name])
        .current_dir(working_dir)
        .status()
        .context("failed to attach to zellij session")?;
    Ok(())
}

/// Create a temp config that layers `default_mode "locked"` on top of the
/// user's existing zellij config. Zellij's `ZELLIJ_CONFIG_FILE` env var
/// overrides the default config path, so we read the user's config and
/// prepend our override.
fn locked_config() -> Result<NamedTempFile> {
    let user_config = zellij_config_path();
    let mut content = String::new();

    if let Some(path) = &user_config
        && path.is_file()
    {
        content = std::fs::read_to_string(path)?;
    }

    if content.contains("default_mode") {
        content = content
            .lines()
            .map(|line| {
                if line.trim().starts_with("default_mode") || line.trim().starts_with("// default_mode") {
                    "default_mode \"locked\""
                } else {
                    line
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
    } else {
        content = format!("default_mode \"locked\"\n{content}");
    }

    let tmp = NamedTempFile::with_suffix(".kdl")?;
    std::fs::write(tmp.path(), &content)?;
    Ok(tmp)
}

fn zellij_config_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("ZELLIJ_CONFIG_FILE") {
        return Some(std::path::PathBuf::from(p));
    }
    let config_dir = std::env::var("XDG_CONFIG_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))
        .ok()?;
    Some(config_dir.join("zellij").join("config.kdl"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use vcs_runner::RunError;

    #[test]
    fn timed_run_returns_stdout_on_success() {
        let output = Cmd::new("echo")
            .arg("hello")
            .timeout(Duration::from_secs(5))
            .run()
            .unwrap();
        assert_eq!(output.stdout_lossy().trim(), "hello");
    }

    #[test]
    fn timed_run_returns_error_on_hang() {
        let start = std::time::Instant::now();
        let result = Cmd::new("sleep")
            .arg("60")
            .timeout(Duration::from_secs(1))
            .run();
        let elapsed = start.elapsed();

        assert!(matches!(result, Err(RunError::Timeout { .. })));
        assert!(elapsed < Duration::from_secs(3));
    }
}
