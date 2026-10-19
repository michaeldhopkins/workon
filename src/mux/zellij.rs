//! Headless zellij. Interactive launch and attach stay in `session`.

use std::path::Path;

use anyhow::{bail, Context, Result};
use vcs_runner::Cmd;

use super::{last_lines, run, Open};
use crate::zellij_reply::{read_listing, Listing};

pub struct Zellij;

impl Zellij {
    fn cmd(&self) -> Cmd {
        Cmd::new("zellij").timeout(std::time::Duration::from_secs(10))
    }

    /// The focused terminal pane, or the first terminal pane.
    ///
    /// `terminal_0` when `list-panes` does not answer with JSON (a stub, or an
    /// older zellij). JSON that lists no terminal yet is an error when `required`,
    /// so sending can wait for the shell; reading can still fall back.
    fn terminal_pane(&self, name: &str, required: bool) -> Result<String> {
        let out = run(self.cmd().args(["--session", name, "action", "list-panes", "--json"]))
            .with_context(|| format!("failed to list panes of zellij session '{name}'"))?;
        match pane_target(&out.stdout_lossy()) {
            PaneTarget::Id(id) => Ok(id),
            PaneTarget::Fallback => Ok("terminal_0".to_string()),
            PaneTarget::Missing if required => bail!("zellij session '{name}' has no terminal pane yet"),
            PaneTarget::Missing => Ok("terminal_0".to_string()),
        }
    }
}

impl super::Multiplexer for Zellij {
    fn exists(&self, name: &str) -> Result<bool> {
        let result =
            self.cmd().args(["list-sessions", "--no-formatting"]).run().map(|out| out.stdout_lossy().into_owned());
        match read_listing(name, result)? {
            Listing::Found(found) => Ok(found),
            Listing::Hung => bail!("zellij did not answer list-sessions"),
        }
    }

    fn list(&self) -> Result<Vec<String>> {
        let result = self.cmd().args(["list-sessions", "--no-formatting"]).run();
        match result {
            Ok(out) => Ok(super::first_fields(&out.stdout_lossy())),
            Err(err) if err.stderr().is_some_and(|text| text.contains("No active zellij sessions")) => Ok(Vec::new()),
            Err(err) if err.is_timeout() => bail!("zellij did not answer list-sessions"),
            Err(err) => Err(err.into()),
        }
    }

    fn ensure(&self, open: &Open<'_>) -> Result<()> {
        if self.exists(open.name)? {
            return Ok(());
        }
        let args = create_args(open.name, open.layout_path, open.dir);
        let mut cmd = self.cmd();
        cmd = cmd.args(args).envs(open.extra_env.iter().map(|(k, v)| (k.clone(), v.clone())));
        run(cmd).with_context(|| format!("failed to create zellij session '{}'", open.name))?;
        Ok(())
    }

    fn send_once(&self, name: &str, text: &str) -> Result<()> {
        let pane = self.terminal_pane(name, true)?;
        // `write-chars` goes to that pane. `paste` follows focus, and a floating
        // plugin (the first-run wizard) would swallow it.
        run(self.cmd().args(["--session", name, "action", "write-chars", "--pane-id", &pane, "--", text]))
            .with_context(|| format!("failed to type into zellij session '{name}'"))?;
        run(self.cmd().args(["--session", name, "action", "send-keys", "--pane-id", &pane, "Enter"]))
            .with_context(|| format!("failed to send Enter to zellij session '{name}'"))?;
        Ok(())
    }

    fn history(&self, name: &str, lines: usize) -> Result<String> {
        let pane = self.terminal_pane(name, false)?;
        let out = run(self.cmd().args(["--session", name, "action", "dump-screen", "--pane-id", &pane, "--full"]))
            .with_context(|| format!("failed to read zellij session '{name}'"))?;
        Ok(last_lines(&out.stdout_lossy(), lines))
    }

    fn kill(&self, name: &str) -> Result<()> {
        run(self.cmd().args(["delete-session", name, "--force"]))
            .with_context(|| format!("failed to delete zellij session '{name}'"))?;
        Ok(())
    }

    fn attach(&self, name: &str, dir: &Path) -> Result<()> {
        let status = std::process::Command::new("zellij")
            .args(["attach", name])
            .current_dir(dir)
            .status()
            .context("failed to attach to zellij session")?;
        if !status.success() {
            bail!("zellij exited {status} while attached to session '{name}'");
        }
        Ok(())
    }
}

/// Where a zellij pane id came from.
pub(crate) enum PaneTarget {
    Id(String),
    /// `list-panes` returned a JSON array with no terminal pane.
    Missing,
    /// The reply was not a JSON array.
    Fallback,
}

/// Pane id of the focused terminal, else the first terminal. Plugins are skipped.
pub(crate) fn focused_terminal(stdout: &str) -> Option<String> {
    let panes = serde_json::from_str::<serde_json::Value>(stdout).ok()?;
    let panes = panes.as_array()?;
    let is_terminal =
        |pane: &serde_json::Value| pane.get("is_plugin").and_then(serde_json::Value::as_bool) == Some(false);
    let focused = |pane: &serde_json::Value| pane.get("is_focused").and_then(serde_json::Value::as_bool) == Some(true);
    let pick = panes
        .iter()
        .find(|pane| is_terminal(pane) && focused(pane))
        .or_else(|| panes.iter().find(|pane| is_terminal(pane)))?;
    let id = pick.get("id").and_then(serde_json::Value::as_u64)?;
    Some(format!("terminal_{id}"))
}

pub(crate) fn pane_target(stdout: &str) -> PaneTarget {
    if let Some(id) = focused_terminal(stdout) {
        return PaneTarget::Id(id);
    }
    match serde_json::from_str::<serde_json::Value>(stdout) {
        Ok(value) if value.as_array().is_some() => PaneTarget::Missing,
        _ => PaneTarget::Fallback,
    }
}

pub(crate) fn create_args(name: &str, layout: &Path, dir: &Path) -> Vec<String> {
    vec![
        "attach".to_string(),
        "--create-background".to_string(),
        name.to_string(),
        "options".to_string(),
        "--default-layout".to_string(),
        layout.display().to_string(),
        "--default-cwd".to_string(),
        dir.display().to_string(),
    ]
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn the_focused_terminal_wins_over_a_plugin_and_an_unfocused_shell() {
        let stdout = r#"[
            {"id": 1, "is_plugin": true, "is_focused": true},
            {"id": 2, "is_plugin": false, "is_focused": false},
            {"id": 0, "is_plugin": false, "is_focused": true}
        ]"#;
        assert_eq!(focused_terminal(stdout).as_deref(), Some("terminal_0"));
        assert_eq!(focused_terminal("[]"), None);
        assert_eq!(focused_terminal("not json"), None);
        assert!(matches!(pane_target(stdout), PaneTarget::Id(ref id) if id == "terminal_0"));
        assert!(matches!(pane_target("[]"), PaneTarget::Missing));
        assert!(matches!(pane_target("not json"), PaneTarget::Fallback));
        assert!(matches!(pane_target(""), PaneTarget::Fallback));
    }

    #[test]
    fn create_args_name_the_session_layout_and_directory() {
        let args = create_args("demo", Path::new("/tmp/layout.kdl"), Path::new("/work"));
        assert_eq!(
            args,
            vec![
                "attach",
                "--create-background",
                "demo",
                "options",
                "--default-layout",
                "/tmp/layout.kdl",
                "--default-cwd",
                "/work",
            ]
        );
    }
}
