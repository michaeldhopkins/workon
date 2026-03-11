use std::io::Read as _;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;
use wait_timeout::ChildExt;

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
    let output = match zellij_with_timeout(&["list-sessions", "--no-formatting"], true) {
        Ok(output) => output,
        Err(_) => return Ok(false),
    };

    let stdout = String::from_utf8_lossy(&output);
    Ok(stdout.lines().any(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(|first| first == name)
    }))
}

fn delete_session(name: &str) -> Result<()> {
    let _ = zellij_with_timeout(&["delete-session", name, "--force"], false);
    Ok(())
}

/// Run a zellij command with a timeout. If it hangs, kill the stuck server.
fn zellij_with_timeout(args: &[&str], capture_stdout: bool) -> Result<Vec<u8>> {
    let mut cmd = Command::new("zellij");
    cmd.args(args)
        .stdout(if capture_stdout { Stdio::piped() } else { Stdio::null() })
        .stderr(Stdio::null());

    match run_with_timeout(&mut cmd, ZELLIJ_TIMEOUT)? {
        Some(stdout) => Ok(stdout),
        None => {
            kill_zellij_server();
            anyhow::bail!("zellij command timed out; killed hung server")
        }
    }
}

/// Spawn a command and wait up to `timeout`. Returns `Some(stdout)` on success,
/// `None` if the command was killed after timing out.
fn run_with_timeout(cmd: &mut Command, timeout: Duration) -> Result<Option<Vec<u8>>> {
    let mut child = cmd.spawn().context("failed to spawn command")?;

    match child.wait_timeout(timeout)? {
        Some(_status) => {
            let mut stdout = Vec::new();
            if let Some(mut pipe) = child.stdout.take() {
                let _ = pipe.read_to_end(&mut stdout);
            }
            Ok(Some(stdout))
        }
        None => {
            let _ = child.kill();
            let _ = child.wait();
            Ok(None)
        }
    }
}

fn kill_zellij_server() {
    eprintln!("Warning: zellij server appears hung, killing it...");
    let _ = Command::new("pkill")
        .args(["-9", "-f", "zellij-server"])
        .status();
    if let Ok(uid_output) = Command::new("id").arg("-u").output() {
        let uid = String::from_utf8_lossy(&uid_output.stdout).trim().to_string();
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

    #[test]
    fn run_with_timeout_returns_stdout_on_success() {
        let mut cmd = Command::new("echo");
        cmd.arg("hello").stdout(Stdio::piped()).stderr(Stdio::null());

        let result = run_with_timeout(&mut cmd, Duration::from_secs(5)).unwrap();
        let stdout = result.expect("should complete within timeout");
        assert_eq!(String::from_utf8_lossy(&stdout).trim(), "hello");
    }

    #[test]
    fn run_with_timeout_returns_none_on_hang() {
        let mut cmd = Command::new("sleep");
        cmd.arg("60").stdout(Stdio::null()).stderr(Stdio::null());

        let start = std::time::Instant::now();
        let result = run_with_timeout(&mut cmd, Duration::from_secs(1)).unwrap();
        let elapsed = start.elapsed();

        assert!(result.is_none(), "should return None on timeout");
        assert!(elapsed < Duration::from_secs(3), "should not wait much longer than the timeout");
    }
}
