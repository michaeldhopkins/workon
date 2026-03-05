use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

#[derive(Debug)]
pub struct Project {
    pub dir: PathBuf,
    pub name: String,
}

pub fn resolve(arg: Option<&str>) -> Result<Project> {
    let dir = match arg {
        Some(input) => resolve_input(input)?,
        None => std::env::current_dir()?,
    };

    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workon".into());

    Ok(Project { dir, name })
}

fn resolve_input(input: &str) -> Result<PathBuf> {
    let path = Path::new(input);
    if path.is_dir() {
        return Ok(path.canonicalize()?);
    }

    let workspace_path = home_dir()?.join("workspace").join(input);
    if workspace_path.is_dir() {
        return Ok(workspace_path.canonicalize()?);
    }

    bail!("directory not found: {input} (also checked ~/workspace/{input})")
}

fn home_dir() -> Result<PathBuf> {
    crate::home::home_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_existing_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let result = resolve(Some(tmp.path().to_str().unwrap())).unwrap();
        assert_eq!(result.dir, tmp.path().canonicalize().unwrap());
        assert_eq!(result.name, tmp.path().file_name().unwrap().to_str().unwrap());
    }

    #[test]
    fn resolve_nonexistent_errors() {
        let result = resolve(Some("/tmp/workon_test_nonexistent_xyz_999"));
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("directory not found"));
    }

    #[test]
    fn resolve_none_uses_cwd() {
        let result = resolve(None).unwrap();
        assert_eq!(result.dir, std::env::current_dir().unwrap());
    }
}
