//! Work handed to a process that may outlive workon.
//!
//! These go through `std::process::Command`, not procpilot: since procpilot 0.9.0, dropping a
//! `SpawnedProcess` kills the child, which would stop a background deletion the moment its
//! handle went out of scope.

use std::io;
use std::path::Path;
use std::process::{Command, Stdio};
use std::thread;

/// Starts `rm -rf <dir>` and returns without waiting, so the user gets their shell back while
/// the OS finishes the deletion. If workon exits first, the deletion carries on.
pub fn remove_dir_in_background(dir: &Path) -> io::Result<()> {
    let mut child = Command::new("rm")
        .arg("-rf")
        .arg(dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;
    // Reaped on a side thread so a long-lived caller is not left with a zombie.
    thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    #[test]
    fn the_directory_is_gone_after_the_call_returns() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("ws");
        std::fs::create_dir_all(dir.join("nested/deeper")).unwrap();
        std::fs::write(dir.join("nested/deeper/file"), "x").unwrap();

        remove_dir_in_background(&dir).unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        while dir.exists() {
            assert!(Instant::now() < deadline, "{} was never removed", dir.display());
            thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let root = tempfile::tempdir().unwrap();
        remove_dir_in_background(&root.path().join("never-existed")).unwrap();
    }
}
