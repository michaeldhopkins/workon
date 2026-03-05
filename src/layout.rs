use std::path::PathBuf;

use anyhow::Result;
use tempfile::NamedTempFile;

const EMBEDDED_LAYOUT: &str = include_str!("../layouts/workon.kdl");

pub enum LayoutSource {
    UserFile(PathBuf),
    TempFile(NamedTempFile),
}

impl LayoutSource {
    pub fn path(&self) -> &std::path::Path {
        match self {
            Self::UserFile(p) => p,
            Self::TempFile(f) => f.path(),
        }
    }
}

pub fn get_layout() -> Result<LayoutSource> {
    let user_layout = config_dir()?.join("workon").join("layout.kdl");
    get_layout_with_override(&user_layout)
}

fn get_layout_with_override(user_layout: &std::path::Path) -> Result<LayoutSource> {
    if user_layout.is_file() {
        return Ok(LayoutSource::UserFile(user_layout.to_path_buf()));
    }

    let tmp = NamedTempFile::with_suffix(".kdl")?;
    std::fs::write(tmp.path(), EMBEDDED_LAYOUT)?;
    Ok(LayoutSource::TempFile(tmp))
}

fn config_dir() -> Result<PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map_err(|_| anyhow::anyhow!("cannot determine config directory"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_layout_contains_kdl_content() {
        let nonexistent = std::path::Path::new("/tmp/workon_test_no_such_file.kdl");
        let source = get_layout_with_override(nonexistent).unwrap();
        let content = std::fs::read_to_string(source.path()).unwrap();
        assert!(content.contains("default_mode"));
        assert!(content.contains("branchdiff"));
        assert!(content.contains("claude"));
    }

    #[test]
    fn user_override_preferred() {
        let tmp = tempfile::tempdir().unwrap();
        let layout_path = tmp.path().join("layout.kdl");
        std::fs::write(&layout_path, "custom layout").unwrap();

        let source = get_layout_with_override(&layout_path).unwrap();
        let content = std::fs::read_to_string(source.path()).unwrap();
        assert_eq!(content, "custom layout");
    }
}
