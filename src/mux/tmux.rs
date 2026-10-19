//! tmux sessions. Splits are created, without the layout's direction or sizes.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use vcs_runner::{Cmd, RunError};

use super::{last_lines, panes, run, Open};

pub struct Tmux;

/// A session by its exact name. A bare name matches by prefix (`demo` finds
/// `demo2`), and a `.` in it reads as a pane separator. `=name:` is the session
/// alone, whatever the name holds.
pub(crate) fn target(name: &str) -> String {
    format!("={}:", session_name(name))
}

/// The name tmux stores: it turns `.` and `:` into `_` (older tmux always did, and
/// they are its pane and window separators). Asking for the stored name keeps
/// `exists` honest on every version.
pub(crate) fn session_name(name: &str) -> String {
    name.replace(['.', ':'], "_")
}

impl Tmux {
    fn cmd(&self) -> Cmd {
        Cmd::new("tmux").timeout(std::time::Duration::from_secs(10))
    }
}

impl super::Multiplexer for Tmux {
    fn exists(&self, name: &str) -> Result<bool> {
        match self.cmd().args(["has-session", "-t", target(name).as_str()]).run() {
            Ok(_) => Ok(true),
            Err(RunError::NonZeroExit { .. }) => Ok(false),
            Err(err) => Err(err.into()),
        }
    }

    fn list(&self) -> Result<Vec<String>> {
        match self.cmd().args(["list-sessions", "-F", "#{session_name}"]).run() {
            Ok(out) => Ok(super::first_fields(&out.stdout_lossy())),
            // No server, or a server with no sessions: there is nothing to list.
            Err(RunError::NonZeroExit { .. }) => Ok(Vec::new()),
            Err(err) => Err(err.into()),
        }
    }

    fn ensure(&self, open: &Open<'_>) -> Result<()> {
        if self.exists(open.name)? {
            return Ok(());
        }
        let mut leaves = panes::leaves(open.layout)?;
        if leaves.is_empty() {
            leaves.push(panes::Leaf::shell());
        }
        let focused = panes::focused_index(&leaves);
        for (index, leaf) in leaves.iter().enumerate() {
            let args = pane_args(open.name, open.dir, open.extra_env, leaf, index == 0);
            run(self.cmd().args(args))?;
        }
        if focused != 0 {
            let target = format!("{}0.{focused}", target(open.name));
            run(self.cmd().args(["select-pane", "-t", &target]))?;
        }
        Ok(())
    }

    fn send_once(&self, name: &str, text: &str) -> Result<()> {
        run(self.cmd().args(["send-keys", "-t", target(name).as_str(), "-l", "--", text]))?;
        run(self.cmd().args(["send-keys", "-t", target(name).as_str(), "Enter"]))?;
        Ok(())
    }

    fn history(&self, name: &str, lines: usize) -> Result<String> {
        let args = capture_args(name, lines);
        let out = run(self.cmd().args(args))?;
        Ok(last_lines(&out.stdout_lossy(), lines))
    }

    fn kill(&self, name: &str) -> Result<()> {
        run(self.cmd().args(["kill-session", "-t", target(name).as_str()]))?;
        Ok(())
    }

    fn attach(&self, name: &str, dir: &Path) -> Result<()> {
        std::process::Command::new("tmux")
            .args(["attach-session", "-t", target(name).as_str()])
            .current_dir(dir)
            .status()?;
        Ok(())
    }
}

pub(crate) fn pane_args(
    name: &str,
    dir: &Path,
    extra: &HashMap<String, String>,
    leaf: &panes::Leaf,
    first: bool,
) -> Vec<String> {
    let mut args = if first {
        vec!["new-session".to_string(), "-d".to_string(), "-s".to_string(), session_name(name)]
    } else {
        vec!["split-window".to_string(), "-d".to_string(), "-t".to_string(), target(name)]
    };
    args.push("-c".to_string());
    args.push(dir.display().to_string());
    let mut keys: Vec<&String> = extra.keys().collect();
    keys.sort();
    for key in keys {
        args.push("-e".to_string());
        args.push(format!("{key}={}", extra[key]));
    }
    if let Some(program) = &leaf.program {
        args.push("--".to_string());
        args.push(program.clone());
        args.extend(leaf.args.iter().cloned());
    }
    args
}

pub(crate) fn capture_args(name: &str, lines: usize) -> Vec<String> {
    vec![
        "capture-pane".to_string(),
        "-t".to_string(),
        target(name),
        "-p".to_string(),
        "-S".to_string(),
        format!("-{lines}"),
    ]
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use super::*;

    #[test]
    fn a_shell_session_is_detached_and_has_no_command() {
        let args = pane_args("demo", Path::new("/work"), &HashMap::new(), &panes::Leaf::shell(), true);
        assert_eq!(args, vec!["new-session", "-d", "-s", "demo", "-c", "/work"]);
    }

    #[test]
    fn a_later_pane_splits_and_keeps_the_command() {
        let leaf = panes::Leaf {
            program: Some("claude".to_string()),
            args: vec!["--resume".to_string(), "abc".to_string()],
            focus: true,
        };
        let mut env = HashMap::new();
        env.insert("B".to_string(), "2".to_string());
        env.insert("A".to_string(), "1".to_string());
        let args = pane_args("demo", Path::new("/work"), &env, &leaf, false);
        assert_eq!(
            args,
            vec![
                "split-window",
                "-d",
                "-t",
                "=demo:",
                "-c",
                "/work",
                "-e",
                "A=1",
                "-e",
                "B=2",
                "--",
                "claude",
                "--resume",
                "abc"
            ]
        );
    }

    #[test]
    fn a_dot_or_colon_in_a_name_reads_as_tmux_stores_it() {
        assert_eq!(session_name("my.app:2"), "my_app_2");
        assert_eq!(target("my.app"), "=my_app:");
    }

    #[test]
    fn capture_starts_at_the_requested_history() {
        assert_eq!(capture_args("demo", 200), vec!["capture-pane", "-t", "=demo:", "-p", "-S", "-200"]);
    }
}
