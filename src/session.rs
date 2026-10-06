use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use tempfile::NamedTempFile;
use vcs_runner::{Cmd, RunError};

use crate::layout;
use crate::zellij_reply::{delete_hung, read_listing, Listing};

const ZELLIJ_TIMEOUT: Duration = Duration::from_secs(5);

pub fn run(
    name: &str,
    layout: &Path,
    working_dir: &Path,
    force_new: bool,
    layout_content: &str,
    config_name: &str,
) -> Result<()> {
    let empty = std::collections::HashMap::new();
    if session_exists("zellij", name)? {
        if force_new {
            delete_session("zellij", name)?;
            launch(name, layout, working_dir, &empty)
        } else {
            ensure_layout_compatible(name, layout_content, config_name)?;
            attach(name, working_dir)
        }
    } else {
        launch(name, layout, working_dir, &empty)
    }
}

/// Refuse to attach to a running zellij session whose layout doesn't match
/// the requested config. We can't ask zellij which layout it loaded, so we
/// approximate: the focused command in the requested layout must be present
/// in the session's process tree. If it isn't, attaching would silently give
/// the user the *running* config rather than the requested one.
fn ensure_layout_compatible(name: &str, layout_content: &str, config_name: &str) -> Result<()> {
    let running = match running_descendant_commands(name) {
        Ok(set) => set,
        // Best-effort: a `ps` failure or missing server PID shouldn't block
        // attach, since the user might have been about to fix things anyway.
        Err(_) => return Ok(()),
    };
    ensure_layout_compatible_inner(name, layout_content, config_name, &running)
}

/// Pure decision logic for `ensure_layout_compatible`. Split out so the
/// branching (focused-command lookup, empty-running shortcut, bail message)
/// is testable without spawning `ps` or `pgrep`.
fn ensure_layout_compatible_inner(
    name: &str,
    layout_content: &str,
    config_name: &str,
    running: &HashSet<String>,
) -> Result<()> {
    let Some(focused) = layout::focused_command(layout_content)? else {
        return Ok(());
    };
    if running.is_empty() || running.contains(&focused) {
        return Ok(());
    }
    bail!(
        "zellij session '{name}' is already running in the main worktree with a different layout. \
         Zellij keeps the original layout when reattaching, so '{config_name}' would be ignored.\n\n\
         To open it as a separate workspace, run: workon -w -c {config_name}\n\
         To replace the running session instead:  workon --new-session -c {config_name}"
    );
}

/// Set of basename commands running anywhere under the zellij server for
/// session `name`. Empty if no server is found.
fn running_descendant_commands(name: &str) -> Result<HashSet<String>> {
    let Some(server_pid) = server_pid_for(name)? else {
        return Ok(HashSet::new());
    };
    let out = Cmd::new("ps")
        .args(["-A", "-o", "pid=,ppid=,comm="])
        .timeout(ZELLIJ_TIMEOUT)
        .run()?;
    Ok(parse_descendants(&out.stdout_lossy(), server_pid))
}

/// Pure parser for `ps -A -o pid=,ppid=,comm=` output. Walks the PPID graph
/// from `root_pid` and returns the set of basenames of all descendants.
/// Cross-platform: macOS prints full paths in `comm`; Linux prints basenames.
/// We strip leading directories defensively.
pub(crate) fn parse_descendants(ps_stdout: &str, root_pid: u32) -> HashSet<String> {
    let mut by_ppid: HashMap<u32, Vec<(u32, String)>> = HashMap::new();
    for line in ps_stdout.lines() {
        let parts: Vec<&str> = line.split_whitespace().collect();
        if parts.len() < 3 {
            continue;
        }
        let pid: u32 = match parts[0].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let ppid: u32 = match parts[1].parse() {
            Ok(p) => p,
            Err(_) => continue,
        };
        let comm = parts[2..].join(" ");
        let basename = comm.rsplit('/').next().unwrap_or(&comm);
        // Login shells appear as "-zsh"; trim the leading dash.
        let basename = basename.trim_start_matches('-').to_string();
        by_ppid.entry(ppid).or_default().push((pid, basename));
    }

    let mut found: HashSet<String> = HashSet::new();
    let mut visited: HashSet<u32> = HashSet::new();
    visited.insert(root_pid);
    let mut frontier: Vec<u32> = vec![root_pid];
    while let Some(p) = frontier.pop() {
        if let Some(children) = by_ppid.get(&p) {
            for (child_pid, child_comm) in children {
                // Guard against cycles in the input. Real process trees can't
                // cycle (kernel-enforced), but a malformed/racy `ps` snapshot
                // could produce one and we'd otherwise spin forever.
                if visited.insert(*child_pid) {
                    found.insert(child_comm.clone());
                    frontier.push(*child_pid);
                }
            }
        }
    }
    found
}

fn session_exists(zellij: &str, name: &str) -> Result<bool> {
    let listing = Cmd::new(zellij)
        .args(["list-sessions", "--no-formatting"])
        .timeout(ZELLIJ_TIMEOUT)
        .run()
        .map(|output| output.stdout_lossy().into_owned());
    match read_listing(name, listing)? {
        Listing::Found(found) => Ok(found),
        Listing::Hung => {
            recover_session(name)?;
            Ok(false)
        }
    }
}

fn delete_session(zellij: &str, name: &str) -> Result<()> {
    let result = Cmd::new(zellij)
        .args(["delete-session", name, "--force"])
        .timeout(ZELLIJ_TIMEOUT)
        .run();
    if delete_hung(&result) {
        recover_session(name)
    } else {
        Ok(())
    }
}

/// SIGKILL the zellij server bound to `name`'s socket and remove the socket
/// file. Targets a single session; leaves other sessions alone.
fn recover_session(name: &str) -> Result<()> {
    eprintln!("Warning: zellij session '{name}' appears hung, recovering...");

    let pattern = anchored_server_pattern(name);
    let _ = Cmd::new("pkill")
        .args(["-9", "-f", &pattern])
        .timeout(ZELLIJ_TIMEOUT)
        .run();

    if let Ok(socket) = session_socket(name) {
        let _ = std::fs::remove_file(&socket);
    }

    // Give the kernel a beat to release the socket inode before relaunching.
    std::thread::sleep(Duration::from_millis(200));
    Ok(())
}

/// Per-session socket path. Mirrors zellij's own derivation:
/// `$TMPDIR / zellij-<uid> / <version> / <session>`, honoring
/// `ZELLIJ_SOCKET_DIR` if set.
fn session_socket(name: &str) -> Result<PathBuf> {
    Ok(socket_dir()?.join(name))
}

fn socket_dir() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("ZELLIJ_SOCKET_DIR") {
        return Ok(PathBuf::from(dir));
    }
    let mut p = std::env::temp_dir();
    p.push(format!("zellij-{}", current_uid()?));
    p.push(zellij_version()?);
    Ok(p)
}

fn current_uid() -> Result<String> {
    let out = Cmd::new("id")
        .arg("-u")
        .timeout(ZELLIJ_TIMEOUT)
        .run()
        .context("failed to read current uid")?;
    Ok(out.stdout_lossy().trim().to_string())
}

fn zellij_version() -> Result<String> {
    // `zellij --version` prints from the binary; does not touch IPC, so it's
    // safe even when a server is hung.
    let out = Cmd::new("zellij")
        .arg("--version")
        .timeout(ZELLIJ_TIMEOUT)
        .run()
        .context("failed to read zellij version")?;
    let stdout = out.stdout_lossy();
    stdout
        .split_whitespace()
        .nth(1)
        .map(str::to_owned)
        .with_context(|| format!("unexpected `zellij --version` output: {stdout:?}"))
}

/// Build a POSIX ERE pattern that matches `zellij --server <socket_dir>/<name>`
/// only when the trailing path element is exactly `name`. The `([[:space:]]|$)`
/// tail covers both real zellij (no trailing argv) and our tests' decoys
/// (extra argv after the path); the leading `/` keeps `foo` from matching
/// `foo-bar` substrings.
pub(crate) fn anchored_server_pattern(name: &str) -> String {
    format!("zellij --server .*/{}([[:space:]]|$)", regex_escape(name))
}

fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if matches!(
            c,
            '.' | '+'
                | '*'
                | '?'
                | '('
                | ')'
                | '|'
                | '['
                | ']'
                | '{'
                | '}'
                | '^'
                | '$'
                | '\\'
        ) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn launch(
    name: &str,
    layout: &Path,
    working_dir: &Path,
    extra_env: &std::collections::HashMap<String, String>,
) -> Result<()> {
    // A stale socket left by a crashed server can wedge the new server's
    // startup. If our socket exists with no live server, remove it.
    preflight_socket(name);

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
    // `zellij attach` has no timeout knob and inherits the TTY, so if IPC is
    // hung it blocks forever. Probe responsiveness first; recover surgically
    // if the IPC layer is wedged.
    preflight_responsive(name);

    Command::new("zellij")
        .args(["attach", name])
        .current_dir(working_dir)
        .status()
        .context("failed to attach to zellij session")?;
    Ok(())
}

/// If our session's socket file exists but no server is listening on it,
/// remove the orphan. Best-effort; never blocks launch on this.
fn preflight_socket(name: &str) {
    let Ok(socket) = session_socket(name) else { return };
    if !socket.exists() {
        return;
    }
    if matches!(server_pid_for(name), Ok(Some(_))) {
        return;
    }
    let _ = std::fs::remove_file(&socket);
}

/// Probe IPC with a short `list-sessions`. If it times out, recover this
/// session's server before handing the TTY to a no-timeout `zellij attach`.
fn preflight_responsive(name: &str) {
    let result = Cmd::new("zellij")
        .args(["list-sessions", "--no-formatting"])
        .timeout(ZELLIJ_TIMEOUT)
        .run();
    if let Err(e) = result
        && e.is_timeout()
    {
        let _ = recover_session(name);
    }
}

fn server_pid_for(name: &str) -> Result<Option<u32>> {
    let pattern = anchored_server_pattern(name);
    match Cmd::new("pgrep")
        .args(["-f", &pattern])
        .timeout(ZELLIJ_TIMEOUT)
        .run()
    {
        Ok(out) => Ok(out
            .stdout_lossy()
            .lines()
            .next()
            .and_then(|s| s.trim().parse().ok())),
        Err(RunError::NonZeroExit { .. }) => Ok(None),
        Err(e) => Err(e.into()),
    }
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
    fn parse_descendants_finds_direct_children_linux_format() {
        // Linux-style: comm column is the basename only.
        let stdout = "\
   100     1 systemd
   200   100 zellij
   300   200 claude
   400   200 branchdiff
   500     1 unrelated
";
        let descendants = parse_descendants(stdout, 200);
        assert!(descendants.contains("claude"), "got {descendants:?}");
        assert!(descendants.contains("branchdiff"), "got {descendants:?}");
        assert!(!descendants.contains("unrelated"), "got {descendants:?}");
        assert!(!descendants.contains("systemd"), "got {descendants:?}");
    }

    #[test]
    fn parse_descendants_strips_paths_macos_format() {
        // macOS-style: comm column is the full executable path.
        let stdout = "\
  100     1 /sbin/launchd
  200   100 /usr/local/bin/zellij
  300   200 /Users/me/.opencode/bin/opencode
  400   200 /usr/local/bin/branchdiff
";
        let descendants = parse_descendants(stdout, 200);
        assert!(descendants.contains("opencode"), "got {descendants:?}");
        assert!(descendants.contains("branchdiff"), "got {descendants:?}");
    }

    #[test]
    fn parse_descendants_recurses_through_grandchildren() {
        let stdout = "\
   100     1 zellij
   200   100 zsh
   300   200 ruby
   400   300 bundle
";
        let descendants = parse_descendants(stdout, 100);
        assert!(descendants.contains("zsh"));
        assert!(descendants.contains("ruby"));
        assert!(descendants.contains("bundle"));
    }

    #[test]
    fn parse_descendants_strips_login_shell_dash() {
        let stdout = "\
   100     1 zellij
   200   100 -zsh
";
        let descendants = parse_descendants(stdout, 100);
        assert!(descendants.contains("zsh"), "got {descendants:?}");
    }

    #[test]
    fn parse_descendants_returns_empty_for_unknown_root() {
        let stdout = "\
   100     1 systemd
   200   100 zellij
";
        let descendants = parse_descendants(stdout, 9999);
        assert!(descendants.is_empty());
    }

    #[test]
    fn parse_descendants_skips_malformed_lines() {
        let stdout = "header line\n   100     1 zellij\nincomplete\n   200   100 claude\n";
        let descendants = parse_descendants(stdout, 100);
        assert!(descendants.contains("claude"));
    }

    // macOS prints the full executable path in `comm`, spaces included.
    #[test]
    fn parse_descendants_keeps_a_command_whose_path_has_spaces() {
        let stdout = "\
   100     1 zellij
   200   100 /Applications/Visual Studio Code.app/Contents/MacOS/Electron
";
        let descendants = parse_descendants(stdout, 100);
        assert!(descendants.contains("Electron"), "got {descendants:?}");
    }

    #[test]
    fn parse_descendants_terminates_on_pid_cycle() {
        // Pathological input: 200's parent is 100, but 100's parent is 200.
        // Without a visited-set guard the BFS would loop forever.
        let stdout = "\
   100   200 zellij
   200   100 child
";
        // Run with a hard wall-clock budget so a regression hangs the test
        // run rather than the whole CI job.
        let start = std::time::Instant::now();
        let descendants = parse_descendants(stdout, 100);
        assert!(start.elapsed() < Duration::from_secs(1));
        assert!(descendants.contains("child"), "got {descendants:?}");
    }

    #[test]
    fn ensure_layout_compatible_inner_ok_when_focused_command_present() {
        let layout = r#"pane command="claude" focus=true"#;
        let mut running = HashSet::new();
        running.insert("claude".to_string());
        running.insert("branchdiff".to_string());
        assert!(ensure_layout_compatible_inner("proj", layout, "default", &running).is_ok());
    }

    #[test]
    fn ensure_layout_compatible_inner_bails_when_focused_command_absent() {
        let layout = r#"pane command="opencode" focus=true"#;
        let mut running = HashSet::new();
        running.insert("claude".to_string());
        running.insert("branchdiff".to_string());

        let err = ensure_layout_compatible_inner("proj", layout, "opencode", &running)
            .expect_err("should refuse on mismatch");
        let msg = err.to_string();

        assert!(msg.contains("'proj'"), "{msg}");
        assert!(msg.contains("'opencode'"), "{msg}");
        assert!(msg.contains("main worktree"), "{msg}");
        assert!(msg.contains("workon -w -c opencode"), "{msg}");
        assert!(msg.contains("--new-session"), "{msg}");
    }

    #[test]
    fn ensure_layout_compatible_inner_ok_when_running_set_is_empty() {
        // Empty running set means we couldn't determine anything (e.g. server
        // PID not found yet). Don't block attach — current behavior wins.
        let layout = r#"pane command="opencode" focus=true"#;
        let running = HashSet::new();
        assert!(ensure_layout_compatible_inner("proj", layout, "opencode", &running).is_ok());
    }

    #[test]
    fn ensure_layout_compatible_inner_ok_when_layout_has_no_commands() {
        // Layouts with only empty terminal panes have nothing to match against.
        let layout = r#"layout {
    pane size="50%"
    pane size="50%"
}"#;
        let mut running = HashSet::new();
        running.insert("anything".to_string());
        assert!(ensure_layout_compatible_inner("proj", layout, "blank", &running).is_ok());
    }

    #[test]
    fn ensure_layout_compatible_inner_bail_message_uses_config_name() {
        let layout = r#"pane command="myco" focus=true"#;
        let running = {
            let mut s = HashSet::new();
            s.insert("claude".to_string());
            s
        };
        let err = ensure_layout_compatible_inner("foo", layout, "my-config", &running)
            .expect_err("should refuse");
        let msg = err.to_string();
        // The recovery hint must use the requested config name verbatim so the
        // user can copy-paste it.
        assert!(msg.contains("workon -w -c my-config"), "{msg}");
    }

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

    #[test]
    fn regex_escape_handles_metacharacters() {
        assert_eq!(regex_escape("simple"), "simple");
        assert_eq!(regex_escape("foo.com"), "foo\\.com");
        assert_eq!(regex_escape("a+b*c?"), "a\\+b\\*c\\?");
        assert_eq!(regex_escape("with$dollar"), "with\\$dollar");
        assert_eq!(regex_escape("ws-a1b2c3"), "ws-a1b2c3");
    }

    /// Both ZELLIJ_SOCKET_DIR tests mutate process-global env. Serialize them
    /// against each other so cargo's parallel test runner can't interleave a
    /// snapshot/restore from one test with a set from another.
    static ENV_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn socket_dir_honors_explicit_override() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os("ZELLIJ_SOCKET_DIR");
        // SAFETY: env mutation is serialized with sibling test via ENV_MUTEX.
        unsafe { std::env::set_var("ZELLIJ_SOCKET_DIR", "/custom/zellij/sock") };

        let dir = socket_dir().expect("socket_dir should respect override");
        assert_eq!(dir, PathBuf::from("/custom/zellij/sock"));

        unsafe {
            match prior {
                Some(v) => std::env::set_var("ZELLIJ_SOCKET_DIR", v),
                None => std::env::remove_var("ZELLIJ_SOCKET_DIR"),
            }
        }
    }

    #[test]
    fn session_socket_appends_session_name() {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os("ZELLIJ_SOCKET_DIR");
        // SAFETY: env mutation is serialized with sibling test via ENV_MUTEX.
        unsafe { std::env::set_var("ZELLIJ_SOCKET_DIR", "/custom/zellij/sock") };

        let socket = session_socket("my-project").expect("session_socket");
        assert_eq!(socket, PathBuf::from("/custom/zellij/sock/my-project"));

        unsafe {
            match prior {
                Some(v) => std::env::set_var("ZELLIJ_SOCKET_DIR", v),
                None => std::env::remove_var("ZELLIJ_SOCKET_DIR"),
            }
        }
    }

    /// Spawn a process that masquerades as a zellij server: argv[0] becomes
    /// `zellij --server <socket_path>` so pgrep -f sees the same surface a
    /// real zellij server would. Uses bash explicitly because `exec -a` is a
    /// bash extension — Ubuntu's /bin/sh (dash) doesn't support it, which
    /// would break Linux CI.
    fn spawn_fake_server(socket_path: &str) -> std::process::Child {
        use std::process::Stdio;
        Command::new("bash")
            .args([
                "-c",
                "exec -a \"zellij --server $1\" sleep 60",
                "decoy",
                socket_path,
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn decoy")
    }

    #[test]
    fn server_pid_anchors_on_session_name() {
        // Spawn two decoys whose argv looks like a zellij server. If our
        // anchor is correct, server_pid_for("foo") matches the foo decoy but
        // NOT the foo-bar decoy.
        let unique = format!("workon-test-{}", std::process::id());
        let foo_session = format!("{unique}-foo");
        let foo_bar_session = format!("{unique}-foo-bar");

        let foo_path = format!("/tmp/zellij-test/{foo_session}");
        let foo_bar_path = format!("/tmp/zellij-test/{foo_bar_session}");

        let mut foo = spawn_fake_server(&foo_path);
        let mut foo_bar = spawn_fake_server(&foo_bar_path);

        // pgrep can race the spawn — give the kernel a moment to publish argv.
        std::thread::sleep(Duration::from_millis(150));

        let foo_pid = server_pid_for(&foo_session).expect("pgrep ok");
        let foo_bar_pid = server_pid_for(&foo_bar_session).expect("pgrep ok");

        let _ = foo.kill();
        let _ = foo_bar.kill();
        let _ = foo.wait();
        let _ = foo_bar.wait();

        assert!(foo_pid.is_some(), "expected to find server for {foo_session}");
        assert!(
            foo_bar_pid.is_some(),
            "expected to find server for {foo_bar_session}"
        );
        // Critically: the foo lookup must NOT have matched the foo-bar decoy.
        assert_ne!(
            foo_pid, foo_bar_pid,
            "anchor failed: lookup for {foo_session} matched the same PID as {foo_bar_session}"
        );
    }

    #[test]
    fn recover_session_kills_target_and_spares_others() {
        // Stand up two fake servers; recover one and confirm the other survives.
        let unique = format!("workon-recover-{}", std::process::id());
        let target = format!("{unique}-target");
        let bystander = format!("{unique}-bystander");

        let mut target_proc = spawn_fake_server(&format!("/tmp/zellij-test/{target}"));
        let mut bystander_proc = spawn_fake_server(&format!("/tmp/zellij-test/{bystander}"));

        std::thread::sleep(Duration::from_millis(150));

        let target_pid_before = server_pid_for(&target).expect("pgrep ok");
        let bystander_pid_before = server_pid_for(&bystander).expect("pgrep ok");
        assert!(target_pid_before.is_some());
        assert!(bystander_pid_before.is_some());

        recover_session(&target).expect("recover_session");

        // pkill is async; give the kernel a moment to reap.
        std::thread::sleep(Duration::from_millis(200));

        let target_pid_after = server_pid_for(&target).expect("pgrep ok");
        let bystander_pid_after = server_pid_for(&bystander).expect("pgrep ok");

        // Cleanup before asserting so a panic doesn't leak processes.
        let _ = target_proc.kill();
        let _ = bystander_proc.kill();
        let _ = target_proc.wait();
        let _ = bystander_proc.wait();

        assert!(
            target_pid_after.is_none(),
            "recover_session left target alive (pid {target_pid_after:?})"
        );
        assert_eq!(
            bystander_pid_before, bystander_pid_after,
            "recover_session killed unrelated session — bystander pid changed"
        );
    }

    /// A stand-in for `zellij` that appends its arguments to `log` and runs `body`.
    fn stand_in(dir: &Path, log: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("zellij");
        let script = format!("#!/bin/sh\necho \"$*\" >> '{}'\n{body}\n", log.display());
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn session_exists_reads_the_listing_for_this_session() {
        let bin = tempfile::tempdir().unwrap();
        let log = bin.path().join("log");
        let zellij = stand_in(bin.path(), &log, "printf 'other [Created 1m ago]\\nmine [Created 2m ago]\\n'");
        let zellij = zellij.to_str().unwrap();

        assert!(session_exists(zellij, "mine").unwrap());
        assert!(!session_exists(zellij, "absent").unwrap());
        let logged = std::fs::read_to_string(&log).unwrap();
        assert_eq!(logged, "list-sessions --no-formatting\nlist-sessions --no-formatting\n");
    }

    #[test]
    fn delete_session_asks_zellij_to_force_delete_it() {
        let bin = tempfile::tempdir().unwrap();
        let log = bin.path().join("log");
        let zellij = stand_in(bin.path(), &log, "exit 0");

        delete_session(zellij.to_str().unwrap(), "mine").unwrap();

        assert_eq!(std::fs::read_to_string(&log).unwrap(), "delete-session mine --force\n");
    }

    /// Point `var` at `value` for the length of `body`, under ENV_MUTEX.
    fn with_env<T>(var: &str, value: &Path, body: impl FnOnce() -> T) -> T {
        let _guard = ENV_MUTEX.lock().unwrap_or_else(|e| e.into_inner());
        let prior = std::env::var_os(var);
        // SAFETY: env mutation is serialized with sibling tests via ENV_MUTEX.
        unsafe { std::env::set_var(var, value) };
        let out = body();
        unsafe {
            match prior {
                Some(v) => std::env::set_var(var, v),
                None => std::env::remove_var(var),
            }
        }
        out
    }

    #[test]
    fn preflight_socket_removes_a_socket_with_no_server() {
        let dir = tempfile::tempdir().unwrap();
        let name = format!("workon-orphan-{}", std::process::id());
        let socket = dir.path().join(&name);
        std::fs::write(&socket, "").unwrap();

        with_env("ZELLIJ_SOCKET_DIR", dir.path(), || preflight_socket(&name));

        assert!(!socket.exists(), "an orphaned socket should be removed before launch");
    }

    #[test]
    fn preflight_socket_keeps_a_socket_whose_server_is_alive() {
        let dir = tempfile::tempdir().unwrap();
        let name = format!("workon-live-{}", std::process::id());
        let socket = dir.path().join(&name);
        std::fs::write(&socket, "").unwrap();
        let mut server = spawn_fake_server(&socket.to_string_lossy());
        std::thread::sleep(Duration::from_millis(150));

        with_env("ZELLIJ_SOCKET_DIR", dir.path(), || preflight_socket(&name));
        let _ = server.kill();
        let _ = server.wait();

        assert!(socket.exists(), "a live server's socket must be left alone");
    }

    /// The user's own config survives, and any `default_mode` line, commented
    /// out or not, becomes `locked` rather than a second, conflicting setting.
    #[test]
    fn locked_config_layers_locked_mode_over_the_users_config() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("config.kdl");
        std::fs::write(&user, "theme \"nord\"\n// default_mode \"normal\"\n").unwrap();

        let tmp = with_env("ZELLIJ_CONFIG_FILE", &user, || locked_config().unwrap());

        let written = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(written.lines().collect::<Vec<_>>(), ["theme \"nord\"", "default_mode \"locked\""]);
    }

    #[test]
    fn locked_config_prepends_locked_mode_when_the_user_sets_none() {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("config.kdl");
        std::fs::write(&user, "theme \"nord\"\n").unwrap();

        let tmp = with_env("ZELLIJ_CONFIG_FILE", &user, || locked_config().unwrap());

        let written = std::fs::read_to_string(tmp.path()).unwrap();
        assert_eq!(written.lines().collect::<Vec<_>>(), ["default_mode \"locked\"", "theme \"nord\""]);
    }
}
