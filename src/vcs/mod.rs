mod git;
mod jj;

use std::path::Path;

use anyhow::{bail, Result};
use vcs_runner::{jj_available, run_git_utf8};

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
    if project_dir.join(".jj").is_dir() {
        return Ok(Box::new(JjBackend));
    }

    let has_git = project_dir.join(".git").is_dir();
    if !has_git {
        bail!("not a git or jj repository");
    }

    if jj_available() {
        jj::init_jj(project_dir)?;
        return Ok(Box::new(JjBackend));
    }

    Ok(Box::new(GitBackend))
}

pub(crate) fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

/// Returns the name of the first git remote (usually "origin", but could be anything).
pub(crate) fn detect_git_remote(project_dir: &Path) -> String {
    run_git_utf8(project_dir, &["remote"])
        .ok()
        .and_then(|s| s.lines().next().map(|l| l.to_string()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "origin".into())
}
