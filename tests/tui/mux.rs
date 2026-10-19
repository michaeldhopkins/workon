//! Headless `run`, `send`, `history`, and `wait`.
//!
//! Stub tests record the argv workon passes. The three `*_run_send_and_history`
//! tests replace that stub with the real binary, when it is installed, and drive
//! it on a pseudo-terminal. A missing binary skips, and panics when `CI` is set.

use std::path::{Path, PathBuf};
use std::process::Command;

use predicates::prelude::*;

use crate::support::World;

const EMPTY_LAYOUT: &str = "layout {\n    pane\n}\n";

fn project(world: &World) -> PathBuf {
    let proj = world.path().join("proj");
    std::fs::create_dir_all(&proj).expect("a project");
    proj
}

/// A trusted `layout { pane }` named `empty`, so `run` does not load the embedded layout.
fn bless_empty(world: &World) {
    let workon = world.home().join(".config/workon");
    let configs = workon.join("configs");
    std::fs::create_dir_all(&configs).expect("a config dir");
    let file = configs.join("empty.kdl");
    std::fs::write(&file, EMPTY_LAYOUT).expect("a layout");
    let canon = std::fs::canonicalize(&file).expect("canonical layout");
    let entry = format!(
        "[[trusted]]\npath = {:?}\nsha256 = \"{}\"\n",
        canon.to_string_lossy(),
        workon::trust::sha256_hex(EMPTY_LAYOUT.as_bytes()),
    );
    std::fs::write(workon.join("trusted.toml"), entry).expect("a trust pin");
}

fn calls_starting(world: &World, program: &str, first: &str) -> Vec<crate::support::Call> {
    world.stubs.calls(program).into_iter().filter(|call| call.args.first().is_some_and(|arg| arg == first)).collect()
}

#[test]
fn zellij_run_types_the_command_into_a_new_session() {
    let world = World::new();
    let proj = project(&world);
    bless_empty(&world);

    world
        .workon(&proj)
        .args(["--mux", "zellij", "run", "--name", "demo", "--config", "empty", "--command", "printf ready"])
        .assert()
        .success();

    let create = calls_starting(&world, "zellij", "attach");
    assert_eq!(create.len(), 1, "{create:?}");
    assert_eq!(&create[0].args[..4], ["attach", "--create-background", "demo", "options"]);
    assert_eq!(create[0].args[4], "--default-layout");
    assert!(create[0].args[5].ends_with(".kdl"), "{:?}", create[0].args);
    assert_eq!(&create[0].args[6..], ["--default-cwd", proj.to_string_lossy().as_ref()]);

    let typed = calls_starting(&world, "zellij", "--session");
    let write: Vec<_> = typed
        .iter()
        .filter(|call| {
            call.args.get(2) == Some(&"action".to_string()) && call.args.get(3) == Some(&"write-chars".to_string())
        })
        .collect();
    let enter: Vec<_> = typed.iter().filter(|call| call.args.get(3) == Some(&"send-keys".to_string())).collect();
    assert_eq!(write.len(), 1, "{typed:?}");
    assert_eq!(enter.len(), 1, "{typed:?}");
    assert_eq!(
        write[0].args,
        ["--session", "demo", "action", "write-chars", "--pane-id", "terminal_0", "--", "printf ready"]
    );
    assert_eq!(enter[0].args, ["--session", "demo", "action", "send-keys", "--pane-id", "terminal_0", "Enter"]);
}

#[test]
fn tmux_run_creates_a_detached_session_and_types_the_command() {
    let world = World::new();
    let proj = project(&world);
    bless_empty(&world);
    // Exit 0 from `has-session` would mean the session already exists.
    world.stubs.set("tmux", "case \"$1\" in\n  has-session) exit 1 ;;\n  --version) exit 1 ;;\nesac\nexit 0");

    world
        .workon(&proj)
        .args(["--mux", "tmux", "run", "--name", "demo", "--config", "empty", "--command", "printf ready"])
        .assert()
        .success();

    let created = calls_starting(&world, "tmux", "new-session");
    assert_eq!(created.len(), 1, "{created:?}");
    let dir = proj.display().to_string();
    assert_eq!(created[0].args, ["new-session", "-d", "-s", "demo", "-c", dir.as_str()]);

    let typed = calls_starting(&world, "tmux", "send-keys");
    assert_eq!(typed.len(), 2, "{typed:?}");
    assert_eq!(typed[0].args, ["send-keys", "-t", "=demo:", "-l", "--", "printf ready"]);
    assert_eq!(typed[1].args, ["send-keys", "-t", "=demo:", "Enter"]);
}

#[test]
fn zmx_run_starts_a_shell_and_sends_a_carriage_return() {
    let world = World::new();
    let proj = project(&world);
    bless_empty(&world);

    world
        .workon(&proj)
        .args(["--mux", "zmx", "run", "--name", "demo", "--config", "empty", "--command", "printf ready"])
        .assert()
        .success();

    let started = calls_starting(&world, "zmx", "run");
    assert_eq!(started.len(), 1, "{started:?}");
    assert_eq!(started[0].args, ["run", "demo", "-d", "true"]);

    let typed = calls_starting(&world, "zmx", "send");
    assert_eq!(typed.len(), 1, "{typed:?}");
    assert_eq!(typed[0].args, ["send", "demo", "printf ready\r"]);
}

#[test]
fn wait_times_out_when_the_text_never_appears() {
    let world = World::new();
    let proj = project(&world);
    world.stubs.set("tmux", "case \"$1\" in\n  has-session) exit 0 ;;\nesac\nexit 0");

    world
        .workon(&proj)
        .args(["--mux", "tmux", "wait", "--for", "marker", "--timeout", "0", "--name", "demo"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("timed out after 0s waiting for \"marker\" in session 'demo'"));
}

#[test]
fn wait_says_when_the_session_is_not_running() {
    let world = World::new();
    let proj = project(&world);
    world.stubs.set("tmux", "case \"$1\" in\n  has-session) exit 1 ;;\nesac\nexit 0");

    world
        .workon(&proj)
        .args(["--mux", "tmux", "wait", "--for", "marker", "--timeout", "0", "--name", "demo"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("session 'demo' is not running"));
}

/// The real binary, looked up in fixed directories. The test process's `PATH` is not read.
fn installed(name: &str) -> Option<PathBuf> {
    let mut dirs = vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        PathBuf::from("/usr/bin"),
        PathBuf::from("/bin"),
    ];
    if let Ok(home) = std::env::var("HOME") {
        let home = PathBuf::from(home);
        dirs.push(home.join(".cargo/bin"));
        dirs.push(home.join(".local/bin"));
    }
    dirs.into_iter().map(|dir| dir.join(name)).find(|path| path.is_file())
}

fn require_installed(name: &str) -> Option<PathBuf> {
    if let Some(path) = installed(name) {
        return Some(path);
    }
    eprintln!("skipped: {name} is not installed");
    if std::env::var_os("CI").is_some() {
        panic!("{name} is not installed and CI must not skip");
    }
    None
}

fn session_name(world: &World) -> String {
    let path = world.path();
    let raw = path.file_name().expect("a directory name").to_string_lossy();
    let mut name: String = raw.chars().map(|ch| if ch.is_ascii_alphanumeric() { ch } else { '-' }).collect();
    name.insert(0, 's');
    name
}

/// Kills the session this test created, including when an assertion fails.
struct Stop {
    bin: PathBuf,
    mux: &'static str,
    name: String,
    socket: PathBuf,
}

impl Drop for Stop {
    fn drop(&mut self) {
        let mut cmd = Command::new(&self.bin);
        cmd.env_remove("TMUX").env_remove("TMUX_PANE").env_remove("ZELLIJ").env_remove("ZELLIJ_SESSION_NAME");
        cmd.env_remove("ZMX_SESSION");
        match self.mux {
            "tmux" => {
                cmd.env("TMUX_TMPDIR", &self.socket);
                cmd.args(["kill-session", "-t", &format!("={}:", self.name)]);
            }
            "zellij" => {
                cmd.env("ZELLIJ_SOCKET_DIR", &self.socket);
                cmd.args(["delete-session", &self.name, "--force"]);
            }
            "zmx" => {
                cmd.env("ZMX_DIR", &self.socket);
                cmd.args(["kill", &self.name, "--force"]);
            }
            _ => return,
        }
        let _ = cmd.output();
    }
}

/// Waits for `marker` in the session, then returns what `history` printed.
/// The typed command is echoed by the shell, so each command builds its marker
/// with `%s` and the marker only exists in the command's output.
fn wait_and_read(world: &World, proj: &Path, mux: &'static str, name: &str, marker: &str) -> String {
    let shell = [("SHELL", "/bin/sh")];
    let mut wait =
        world.tui_with(proj, &["--mux", mux, "wait", "--for", marker, "--timeout", "8", "--name", name], &shell);
    let waited = wait.wait_for_exit();
    let wait_screen = wait.screen();
    let mut history = world.tui_with(proj, &["--mux", mux, "history", "--lines", "20", "--name", name], &shell);
    assert!(history.wait_for_exit(), "{}", history.screen());
    let seen = history.screen();
    assert!(waited, "wait for {marker} failed. history:\n{seen}\n---\n{wait_screen}");
    assert!(seen.contains(marker), "history:\n{seen}");
    seen
}

fn drive(mux: &'static str, bin: &Path) {
    let world = World::new();
    let proj = project(&world);
    bless_empty(&world);
    let name = session_name(&world);
    let socket = world.socket_dir(mux);
    let _stop = Stop { bin: bin.to_path_buf(), mux, name: name.clone(), socket: socket.clone() };
    let text = bin.display().to_string();
    assert!(!text.contains('\''), "{text}");
    world.stubs.set(mux, &format!("exec '{text}' \"$@\""));

    let shell = [("SHELL", "/bin/sh")];
    let mut run = world.tui_with(
        &proj,
        &["--mux", mux, "run", "--name", &name, "--config", "empty", "--command", "printf 'from-run-%s\\n' one"],
        &shell,
    );
    assert!(run.wait_for_exit(), "{}", run.screen());
    wait_and_read(&world, &proj, mux, &name, "from-run-one");

    let mut send =
        world.tui_with(&proj, &["--mux", mux, "send", "--name", &name, "printf 'from-send-%s\\n' two"], &shell);
    assert!(send.wait_for_exit(), "{}", send.screen());
    wait_and_read(&world, &proj, mux, &name, "from-send-two");
}

#[test]
fn zellij_run_send_and_history() {
    let Some(bin) = require_installed("zellij") else { return };
    drive("zellij", &bin);
}

#[test]
fn tmux_run_send_and_history() {
    let Some(bin) = require_installed("tmux") else { return };
    drive("tmux", &bin);
}

#[test]
fn zmx_run_send_and_history() {
    let Some(bin) = require_installed("zmx") else { return };
    drive("zmx", &bin);
}

/// tmux matches a bare target by prefix and reads `.` as a pane separator, so
/// `demo` would find `demo2` and `my.app` would never be found at all.
#[test]
fn tmux_matches_session_names_exactly() {
    let Some(bin) = require_installed("tmux") else { return };
    let world = World::new();
    let proj = project(&world);
    bless_empty(&world);
    let longer = format!("{}-longer", session_name(&world));
    let dotted = format!("{}.dotted", session_name(&world));
    let socket = world.socket_dir("tmux");
    let _longer = Stop { bin: bin.clone(), mux: "tmux", name: longer.clone(), socket: socket.clone() };
    let _dotted = Stop { bin: bin.clone(), mux: "tmux", name: dotted.clone(), socket };
    world.stubs.set("tmux", &format!("exec '{}' \"$@\"", bin.display()));
    let shell = [("SHELL", "/bin/sh")];

    for name in [&longer, &dotted] {
        let mut run = world.tui_with(&proj, &["--mux", "tmux", "run", "--name", name, "--config", "empty"], &shell);
        assert!(run.wait_for_exit(), "{}", run.screen());
    }
    let prefix = longer.trim_end_matches("-longer").to_string();
    world
        .workon(&proj)
        .args(["--mux", "tmux", "history", "--name", &prefix])
        .assert()
        .failure()
        .stderr(predicate::str::contains(format!("session '{prefix}' is not running")));

    let mut send = world.tui_with(&proj, &["--mux", "tmux", "send", "--name", &dotted, "printf 'dot-%s\\n' ok"], &shell);
    assert!(send.wait_for_exit(), "{}", send.screen());
    wait_and_read(&world, &proj, "tmux", &dotted, "dot-ok");
}
