//! `workon exec [REF] -- COMMAND…`: one command run in a workspace, as a shell opened there would
//! run it (workon#7).

use std::ffi::OsString;
use std::io::ErrorKind;
use std::process::{Command, ExitCode};

use anyhow::{bail, Result};

/// Shell conventions for a command that never started, so a script can tell them from the
/// command's own failures.
const NOT_FOUND: u8 = 127;
const NOT_EXECUTABLE: u8 = 126;

/// Replaces this process with `command` in the workspace's directory, with the environment its
/// session gets (mise's, then the provisioners' session env), so the terminal, signals and exit
/// status are the command's own. Returns only when the command could not be started.
pub fn cmd_exec(reference: Option<&str>, command: &[OsString]) -> Result<ExitCode> {
    let Some((program, args)) = command.split_first() else {
        bail!("no command given: workon exec [REF] -- COMMAND [ARGS…]");
    };
    let ws_dir = crate::workspace::resolve_ws_dir(reference)?;
    if !ws_dir.is_dir() {
        bail!("workspace directory {} does not exist", ws_dir.display());
    }
    // With the env's PATH set on the Command, std looks the program up on that PATH, and a
    // relative program (`bin/rails`) resolves against the workspace, since exec changes
    // directory first.
    let mut cmd = Command::new(program);
    cmd.args(args).current_dir(&ws_dir).envs(crate::workspace::session_launch_env(&ws_dir));
    let error = match run(&mut cmd) {
        Ok(code) => return Ok(code),
        Err(error) => error,
    };
    let name = program.to_string_lossy();
    let (code, what) = match error.kind() {
        ErrorKind::NotFound => (NOT_FOUND, "command not found"),
        ErrorKind::PermissionDenied => (NOT_EXECUTABLE, "permission denied"),
        _ => (NOT_EXECUTABLE, "could not run it"),
    };
    eprintln!("workon exec: {name}: {what} ({error})");
    Ok(ExitCode::from(code))
}

/// Unix replaces this process, so it returns only on failure.
#[cfg(unix)]
fn run(cmd: &mut Command) -> std::io::Result<ExitCode> {
    use std::os::unix::process::CommandExt;
    Err(cmd.exec())
}

/// Windows has no exec: run the command as a child and pass its exit status on.
#[cfg(not(unix))]
fn run(cmd: &mut Command) -> std::io::Result<ExitCode> {
    let status = cmd.status()?;
    let code = status.code().unwrap_or(1);
    Ok(ExitCode::from(u8::try_from(code).unwrap_or(1)))
}
