use std::path::PathBuf;

use anyhow::Result;

#[derive(Debug)]
pub struct Project {
    pub dir: PathBuf,
    pub name: String,
}

/// The project is always the current directory — workon operates on the repo
/// you're standing in. (Earlier versions accepted a path or `~/workspace/<name>`
/// argument; that was a single-machine convention carried over from the original
/// shell script and has been removed.)
pub fn resolve() -> Result<Project> {
    let dir = std::env::current_dir()?;
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workon".into());

    Ok(Project { dir, name })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_uses_cwd() {
        let result = resolve().unwrap();
        assert_eq!(result.dir, std::env::current_dir().unwrap());
        assert!(!result.name.is_empty());
    }
}
