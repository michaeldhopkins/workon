//! Multiplexer backends. zellij is the default. tmux and zmx are selected with
//! `--mux` or `WORKON_MUX`.
//!
//! Interactive zellij (the no-flag `workon` and `workon -w` path) stays in
//! [`crate::session`]. This module is the headless surface, and the interactive
//! path for anything that is not zellij.

mod kind;
mod panes;
mod tmux;
mod zellij;
mod zmx;

use std::collections::HashMap;
use std::path::Path;
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use vcs_runner::{Cmd, RunOutput};

pub use kind::{parse_mux, MuxKind};

/// What one `run` / attach needs. The layout text and its path are the file
/// workon already resolved.
pub struct Open<'a> {
    pub name: &'a str,
    pub dir: &'a Path,
    pub layout_path: &'a Path,
    pub layout: &'a str,
    pub extra_env: &'a HashMap<String, String>,
}

/// Create, drive, and close one session. Interactive attach inherits the terminal.
pub trait Multiplexer {
    fn exists(&self, name: &str) -> Result<bool>;
    fn list(&self) -> Result<Vec<String>>;
    fn ensure(&self, open: &Open<'_>) -> Result<()>;
    fn send_once(&self, name: &str, text: &str) -> Result<()>;
    fn history(&self, name: &str, lines: usize) -> Result<String>;
    fn kill(&self, name: &str) -> Result<()>;
    fn attach(&self, name: &str, dir: &Path) -> Result<()>;
}

static CURRENT: OnceLock<MuxKind> = OnceLock::new();

/// Remember the multiplexer for this process. A flag wins over `WORKON_MUX`.
/// Only `main` calls this; tests drive a backend directly or spawn the binary.
pub fn install(flag: Option<MuxKind>) -> Result<()> {
    let kind = match flag {
        Some(kind) => kind,
        None => match std::env::var("WORKON_MUX") {
            Ok(value) if !value.is_empty() => MuxKind::from_name(&value).map_err(anyhow::Error::msg)?,
            _ => MuxKind::Zellij,
        },
    };
    let _ = CURRENT.set(kind);
    Ok(())
}

pub fn current() -> MuxKind {
    CURRENT.get().copied().unwrap_or(MuxKind::Zellij)
}

fn backend() -> Box<dyn Multiplexer> {
    match current() {
        MuxKind::Zellij => Box::new(zellij::Zellij),
        MuxKind::Tmux => Box::new(tmux::Tmux),
        MuxKind::Zmx => Box::new(zmx::Zmx),
    }
}

/// The selected multiplexer has to be on `PATH`, and so does every layout command.
pub fn require(layout: &str) -> Result<()> {
    match current() {
        MuxKind::Zellij => crate::deps::check_all(layout),
        MuxKind::Tmux => crate::deps::check_mux("tmux", "brew install tmux", layout),
        MuxKind::Zmx => crate::deps::check_mux("zmx", "brew install neurosnap/tap/zmx", layout),
    }
}

/// Create the session if it is missing, then attach the terminal.
pub fn open_in_terminal(
    name: &str,
    layout_path: &Path,
    dir: &Path,
    force_new: bool,
    extra_env: &HashMap<String, String>,
) -> Result<()> {
    let layout = std::fs::read_to_string(layout_path).with_context(|| format!("reading {}", layout_path.display()))?;
    let mux = backend();
    if force_new && mux.exists(name)? {
        mux.kill(name)?;
    }
    if !mux.exists(name)? {
        mux.ensure(&Open { name, dir, layout_path, layout: &layout, extra_env })?;
    }
    mux.attach(name, dir)
}

/// Create the session in the background if it does not exist, then type `command`.
pub fn run_background(
    name: &str,
    dir: &Path,
    layout_path: &Path,
    extra_env: &HashMap<String, String>,
    command: Option<&str>,
) -> Result<()> {
    let layout = std::fs::read_to_string(layout_path).with_context(|| format!("reading {}", layout_path.display()))?;
    let mux = backend();
    if !mux.exists(name)? {
        mux.ensure(&Open { name, dir, layout_path, layout: &layout, extra_env })?;
    }
    if let Some(command) = command {
        send_soon(mux.as_ref(), name, command)?;
    }
    Ok(())
}

pub fn send(name: &str, text: &str) -> Result<()> {
    let mux = backend();
    require_running(mux.as_ref(), name)?;
    send_soon(mux.as_ref(), name, text)
}

pub fn history(name: &str, lines: usize) -> Result<String> {
    let mux = backend();
    require_running(mux.as_ref(), name)?;
    mux.history(name, lines)
}

pub fn wait_for(name: &str, text: &str, timeout: Duration) -> Result<()> {
    let mux = backend();
    require_running(mux.as_ref(), name)?;
    wait_until(name, text, timeout, || mux.history(name, 200))
}

fn require_running(mux: &dyn Multiplexer, name: &str) -> Result<()> {
    if mux.exists(name)? {
        Ok(())
    } else {
        bail!("session '{name}' is not running")
    }
}

/// The pane may not be ready in the moment after `ensure`. Retry a few seconds.
fn send_soon(mux: &dyn Multiplexer, name: &str, text: &str) -> Result<()> {
    let start = Instant::now();
    let limit = Duration::from_secs(5);
    loop {
        match mux.send_once(name, text) {
            Ok(()) => return Ok(()),
            Err(err) if start.elapsed() >= limit => return Err(err),
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
}

pub(crate) fn wait_until(
    name: &str,
    text: &str,
    timeout: Duration,
    mut history: impl FnMut() -> Result<String>,
) -> Result<()> {
    let start = Instant::now();
    loop {
        if history()?.contains(text) {
            return Ok(());
        }
        if start.elapsed() >= timeout {
            let secs = timeout.as_secs();
            bail!("timed out after {secs}s waiting for {text:?} in session '{name}'");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

pub(crate) fn run(cmd: Cmd) -> Result<RunOutput> {
    cmd.run().map_err(Into::into)
}

pub(crate) fn first_fields(stdout: &str) -> Vec<String> {
    stdout.lines().filter_map(|line| line.split_whitespace().next().map(str::to_string)).collect()
}

/// The last `lines` of `text`. Whitespace-only rows under the cursor are the
/// pane's unused height (`capture-pane` prints every row), not output.
pub(crate) fn last_lines(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    let end = all.iter().rposition(|line| !line.trim().is_empty()).map_or(0, |index| index + 1);
    let kept = &all[..end];
    let start = kept.len().saturating_sub(lines);
    kept[start..].join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_mux_is_zellij() {
        assert_eq!(current(), MuxKind::Zellij);
    }

    #[test]
    fn first_field_of_each_line_is_a_session_name() {
        assert_eq!(first_fields("demo [Created 1s]\nother\n"), vec!["demo".to_string(), "other".to_string()]);
        assert!(first_fields("").is_empty());
    }

    #[test]
    fn last_lines_keeps_the_tail() {
        assert_eq!(last_lines("a\nb\nc", 2), "b\nc");
        assert_eq!(last_lines("a\nb", 5), "a\nb");
        assert_eq!(last_lines("a\nb", 0), "");
    }

    #[test]
    fn last_lines_drops_blank_rows_below_the_cursor() {
        // tmux pads each unused row out to the pane width.
        let pane = "printf 'workon-marker'\nworkon-marker\n$ \n        \n        \n";
        assert_eq!(last_lines(pane, 20), "printf 'workon-marker'\nworkon-marker\n$ ");
        assert_eq!(last_lines(pane, 1), "$ ");
        assert_eq!(last_lines("a\n\nb\n\n", 20), "a\n\nb");
        assert_eq!(last_lines("   \n\n", 5), "");
    }

    #[test]
    fn wait_until_returns_when_the_text_shows_up() {
        let mut calls = 0;
        wait_until("demo", "marker", Duration::from_secs(2), || {
            calls += 1;
            if calls == 1 {
                Ok("not yet".to_string())
            } else {
                Ok("workon-marker\n".to_string())
            }
        })
        .unwrap();
        assert_eq!(calls, 2);
    }

    #[test]
    fn wait_until_names_the_session_and_the_text() {
        let err = wait_until("demo", "marker", Duration::from_secs(0), || Ok("nope".to_string())).unwrap_err();
        assert_eq!(err.to_string(), "timed out after 0s waiting for \"marker\" in session 'demo'");
    }
}
