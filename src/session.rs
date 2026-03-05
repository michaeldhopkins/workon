use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{Context, Result};
use tempfile::NamedTempFile;

pub fn run(name: &str, layout: &Path, working_dir: &Path, force_new: bool) -> Result<()> {
    if session_exists(name)? {
        if force_new {
            delete_session(name)?;
            launch(name, layout, working_dir)
        } else {
            attach(name, working_dir)
        }
    } else {
        launch(name, layout, working_dir)
    }
}

fn session_exists(name: &str) -> Result<bool> {
    let output = Command::new("zellij")
        .args(["list-sessions", "--no-formatting"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .context("failed to list zellij sessions")?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout.lines().any(|line| {
        line.split_whitespace()
            .next()
            .is_some_and(|first| first == name)
    }))
}

fn delete_session(name: &str) -> Result<()> {
    Command::new("zellij")
        .args(["delete-session", name, "--force"])
        .status()
        .context("failed to delete zellij session")?;
    Ok(())
}

pub fn launch(name: &str, layout: &Path, working_dir: &Path) -> Result<()> {
    let config = locked_config()?;

    Command::new("zellij")
        .args([
            "--new-session-with-layout",
            &layout.to_string_lossy(),
            "--session",
            name,
        ])
        .env("ZELLIJ_CONFIG_FILE", config.path())
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
