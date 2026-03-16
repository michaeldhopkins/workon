mod git;
mod jj;

use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{bail, Result};

pub use self::git::GitBackend;
pub use self::jj::JjBackend;

pub trait Vcs: Send + Sync {
    fn detect_trunk(&self, project_dir: &Path) -> Result<String>;
    fn create_workspace(&self, project_dir: &Path, ws_dir: &Path, ws_id: &str, trunk: &str) -> Result<()>;
    fn pre_copy_sync(&self, project_dir: &Path);
    fn changed_files(&self, ws_id: &str, project_dir: &Path, ws_dir: &Path) -> Vec<String>;
    fn save_work(&self, ws_id: &str, project_dir: &Path, ws_dir: &Path) -> Result<()>;
    fn forget_workspace(&self, ws_id: &str, project_dir: &Path, ws_dir: &Path);
}

/// Detect VCS backend. jj preferred; git fallback when jj unavailable.
pub fn detect(project_dir: &Path) -> Result<Box<dyn Vcs>> {
    // Already has .jj → use jj
    if project_dir.join(".jj").is_dir() {
        return Ok(Box::new(JjBackend));
    }

    let has_git = project_dir.join(".git").is_dir();
    if !has_git {
        bail!("not a git or jj repository");
    }

    // Has .git, no .jj → try to init jj if binary available
    if jj_available() {
        jj::init_jj(project_dir)?;
        return Ok(Box::new(JjBackend));
    }

    // jj not installed → git fallback
    Ok(Box::new(GitBackend))
}

fn jj_available() -> bool {
    Command::new("jj")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

pub(crate) fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

pub(crate) fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {program}"))?;
    if !status.success() {
        bail!("{program} exited with status {status}");
    }
    Ok(())
}

use anyhow::Context;
