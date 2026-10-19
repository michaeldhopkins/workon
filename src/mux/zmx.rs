//! zmx sessions. One PTY: the focused leaf is typed into the shell zmx keeps.
//!
//! `zmx run` takes the session name first (`run NAME -d COMMAND`). `-d` is any
//! argument that starts with `-d`, so it has to be workon's own flag and not
//! part of the command. `send` joins its arguments with spaces and does not
//! append a carriage return; readline only accepts the line on CR.

use std::path::Path;

use anyhow::{bail, Context, Result};
use vcs_runner::Cmd;

use super::{last_lines, panes, run, Open};

pub struct Zmx;

impl Zmx {
    fn cmd(&self) -> Cmd {
        // A zmx client started inside a session would switch that session.
        Cmd::new("zmx").env_remove("ZMX_SESSION").timeout(std::time::Duration::from_secs(10))
    }
}

impl super::Multiplexer for Zmx {
    fn exists(&self, name: &str) -> Result<bool> {
        Ok(self.list()?.iter().any(|session| session == name))
    }

    fn list(&self) -> Result<Vec<String>> {
        let out = run(self.cmd().args(["list", "--short"])).context("failed to list zmx sessions")?;
        Ok(super::first_fields(&out.stdout_lossy()))
    }

    fn ensure(&self, open: &Open<'_>) -> Result<()> {
        if self.exists(open.name)? {
            return Ok(());
        }
        let leaves = panes::leaves(open.layout)?;
        let leaf = leaves.get(panes::focused_index(&leaves)).cloned().unwrap_or_else(panes::Leaf::shell);
        let args = run_args(open.name, &leaf);
        let mut cmd = self.cmd();
        cmd = cmd.args(args).envs(open.extra_env.iter().map(|(k, v)| (k.clone(), v.clone())));
        run(cmd).with_context(|| format!("failed to create zmx session '{}'", open.name))?;
        Ok(())
    }

    fn send_once(&self, name: &str, text: &str) -> Result<()> {
        let args = send_args(name, text);
        run(self.cmd().args(args)).with_context(|| format!("failed to type into zmx session '{name}'"))?;
        Ok(())
    }

    fn history(&self, name: &str, lines: usize) -> Result<String> {
        let out =
            run(self.cmd().args(["history", name])).with_context(|| format!("failed to read zmx session '{name}'"))?;
        Ok(last_lines(&out.stdout_lossy(), lines))
    }

    fn kill(&self, name: &str) -> Result<()> {
        run(self.cmd().args(["kill", name, "--force"]))
            .with_context(|| format!("failed to kill zmx session '{name}'"))?;
        Ok(())
    }

    fn attach(&self, name: &str, dir: &Path) -> Result<()> {
        let status = std::process::Command::new("zmx")
            .args(["attach", name])
            .env_remove("ZMX_SESSION")
            .current_dir(dir)
            .status()
            .context("failed to attach to zmx session")?;
        if !status.success() {
            bail!("zmx exited {status} while attached to session '{name}'");
        }
        Ok(())
    }
}

/// `zmx run NAME -d <leaf>`, or `true` when the leaf is a shell.
///
/// The shell zmx started stays up. The command is typed into it.
pub(crate) fn run_args(name: &str, leaf: &panes::Leaf) -> Vec<String> {
    let mut args = vec!["run".to_string(), name.to_string(), "-d".to_string()];
    match &leaf.program {
        Some(program) => {
            args.push(program.clone());
            args.extend(leaf.args.iter().cloned());
        }
        None => args.push("true".to_string()),
    }
    args
}

/// One argument, so zmx does not insert a space before the carriage return.
pub(crate) fn send_args(name: &str, text: &str) -> Vec<String> {
    let mut payload = text.to_string();
    payload.push('\r');
    vec!["send".to_string(), name.to_string(), payload]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_shell_session_runs_true_detached() {
        assert_eq!(run_args("demo", &panes::Leaf::shell()), vec!["run", "demo", "-d", "true"]);
    }

    #[test]
    fn a_focused_command_is_the_detached_run() {
        let leaf = panes::Leaf {
            program: Some("claude".to_string()),
            args: vec!["--resume".to_string(), "abc".to_string()],
            focus: true,
        };
        assert_eq!(run_args("demo", &leaf), vec!["run", "demo", "-d", "claude", "--resume", "abc"]);
    }

    #[test]
    fn send_carries_a_carriage_return_and_no_extra_argument() {
        let args = send_args("demo", "printf ready");
        assert_eq!(args.len(), 3);
        assert_eq!(args[0], "send");
        assert_eq!(args[1], "demo");
        assert_eq!(args[2], "printf ready\r");
    }
}
