use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result};
use tempfile::NamedTempFile;
use vcs_runner::{Cmd, RunError};

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
    match Cmd::new("zellij")
        .args(["list-sessions", "--no-formatting"])
        .timeout(ZELLIJ_TIMEOUT)
        .run()
    {
        Ok(output) => Ok(output.stdout_lossy().lines().any(|line| {
            line.split_whitespace()
                .next()
                .is_some_and(|first| first == name)
        })),
        Err(ref e) if e.is_timeout() => {
            // IPC is hung. Could be our session's server or an unrelated orphan
            // (zellij IPC blocks globally on a single bad socket). Surgically
            // recover only what's bound to OUR session, then return false so
            // the caller launches fresh.
            recover_session(name)?;
            Ok(false)
        }
        // Fresh machine / fully-reaped sessions: zellij exits 1 with this
        // stderr sentinel. Semantically equivalent to "our session does not
        // exist" — return false so the caller launches a new one.
        Err(ref e) if is_no_sessions_error(e) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn is_no_sessions_error(err: &RunError) -> bool {
    err.stderr()
        .is_some_and(|s| s.contains("No active zellij sessions"))
}

fn delete_session(name: &str) -> Result<()> {
    match Cmd::new("zellij")
        .args(["delete-session", name, "--force"])
        .timeout(ZELLIJ_TIMEOUT)
        .run()
    {
        Ok(_) => Ok(()),
        // delete-session itself wedged — fall through to surgical kill.
        Err(ref e) if e.is_timeout() => recover_session(name),
        // Non-zero typically means "no such session"; nothing to do.
        Err(_) => Ok(()),
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
fn anchored_server_pattern(name: &str) -> String {
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

    /// Build a real `RunError::NonZeroExit` by running `sh -c "...>&2; exit 1"`.
    /// `CmdDisplay::new` is crate-private upstream, so direct construction
    /// isn't an option — running a real subprocess is the supported path.
    fn non_zero_exit_with_stderr(stderr: &str) -> RunError {
        let script = format!("printf %s {} 1>&2; exit 1", shell_single_quote(stderr));
        Cmd::new("sh")
            .args(["-c", &script])
            .timeout(Duration::from_secs(5))
            .run()
            .expect_err("expected non-zero exit")
    }

    /// Single-quote a string for POSIX shell. Inputs in this file are static
    /// test fixtures, but using the right quoting keeps the helper reusable.
    fn shell_single_quote(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('\'');
        for ch in s.chars() {
            if ch == '\'' {
                out.push_str("'\\''");
            } else {
                out.push(ch);
            }
        }
        out.push('\'');
        out
    }

    #[test]
    fn no_sessions_error_recognized_on_fresh_machine() {
        // The exact stderr zellij emits when no sessions exist on the host.
        // Without this classifier, workon's first run on a clean machine
        // aborts. See specs/no-active-sessions-bug.md.
        let err = non_zero_exit_with_stderr("No active zellij sessions found.");
        assert!(is_no_sessions_error(&err));
    }

    #[test]
    fn no_sessions_error_tolerates_punctuation_drift() {
        let err = non_zero_exit_with_stderr("No active zellij sessions found");
        assert!(is_no_sessions_error(&err));
    }

    #[test]
    fn unrelated_non_zero_exit_is_not_no_sessions() {
        let err = non_zero_exit_with_stderr("some other zellij failure");
        assert!(!is_no_sessions_error(&err));
    }

    #[test]
    fn timeout_error_is_not_no_sessions() {
        let err = Cmd::new("sleep")
            .arg("60")
            .timeout(Duration::from_millis(100))
            .run()
            .expect_err("expected timeout");
        assert!(err.is_timeout());
        assert!(!is_no_sessions_error(&err));
    }

    #[test]
    fn spawn_error_is_not_no_sessions() {
        let err = Cmd::new("definitely-not-a-real-binary-zxqv-9001")
            .timeout(Duration::from_secs(5))
            .run()
            .expect_err("expected spawn failure");
        assert!(err.is_spawn_failure());
        assert!(!is_no_sessions_error(&err));
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
}
