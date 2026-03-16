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

/// Return a layout with `--session-id` injected into the `claude` pane.
pub fn get_workspace_layout(claude_session_id: &str) -> Result<LayoutSource> {
    let args = format!("\"--session-id\" \"{claude_session_id}\"");
    layout_with_claude_args(&args)
}

/// Return a layout with `--resume` injected into the `claude` pane.
pub fn get_resume_layout(claude_session_id: &str) -> Result<LayoutSource> {
    let args = format!("\"-r\" \"{claude_session_id}\"");
    layout_with_claude_args(&args)
}

fn layout_with_claude_args(args: &str) -> Result<LayoutSource> {
    let user_layout = config_dir()?.join("workon").join("layout.kdl");
    let content = if user_layout.is_file() {
        std::fs::read_to_string(&user_layout)?
    } else {
        EMBEDDED_LAYOUT.to_string()
    };

    let modified = inject_claude_args(&content, args);
    let tmp = NamedTempFile::with_suffix(".kdl")?;
    std::fs::write(tmp.path(), &modified)?;
    Ok(LayoutSource::TempFile(tmp))
}

fn inject_claude_args(layout: &str, args: &str) -> String {
    let args_line = format!("args {args}");
    let lines: Vec<&str> = layout.lines().collect();
    let mut result = Vec::with_capacity(lines.len() + 2);

    for line in lines {
        if line.contains("command=\"claude\"") {
            let trimmed = line.trim_end();
            let indent = &line[..line.len() - line.trim_start().len()];
            if trimmed.ends_with('{') {
                result.push(line.to_string());
                result.push(format!("{indent}    {args_line}"));
            } else {
                result.push(format!("{trimmed} {{"));
                result.push(format!("{indent}    {args_line}"));
                result.push(format!("{indent}}}"));
            }
        } else {
            result.push(line.to_string());
        }
    }

    result.join("\n")
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

    #[test]
    fn inject_args_into_claude_pane() {
        let layout = r#"layout {
    pane command="claude" size="80%" focus=true
    pane command="branchdiff" size="50%"
}"#;
        let result = inject_claude_args(layout, r#""--session-id" "abc-123""#);
        assert!(result.contains(r#"args "--session-id" "abc-123""#));
        for line in result.lines() {
            if line.contains("branchdiff") {
                assert!(!line.contains("session-id"), "branchdiff pane should not get session-id");
            }
        }
    }

    #[test]
    fn inject_args_leaves_other_panes_alone() {
        let layout = r#"pane command="claude" size="80%"
pane command="branchdiff" size="50%""#;
        let result = inject_claude_args(layout, r#""-r" "test-uuid""#);
        for line in result.lines() {
            if line.contains("branchdiff") {
                assert!(!line.contains("test-uuid"));
            }
        }
    }

    #[test]
    fn inject_args_into_existing_args_block() {
        let layout = r#"    pane command="claude" size="80%" focus=true {
        args "--model" "opus"
    }"#;
        let result = inject_claude_args(layout, r#""--session-id" "abc-123""#);
        assert!(result.contains(r#"args "--session-id" "abc-123""#));
        assert!(result.contains(r#"args "--model" "opus""#));
        assert!(!result.contains("{ {"));
    }

    #[test]
    fn inject_args_into_embedded_layout() {
        let args = r#""--session-id" "550e8400-e29b-41d4-a716-446655440000""#;
        let result = inject_claude_args(EMBEDDED_LAYOUT, args);
        assert!(result.contains(r#"args "--session-id" "550e8400-e29b-41d4-a716-446655440000""#));
        assert!(result.contains("command=\"claude\""));
        assert!(result.contains("command=\"branchdiff\""));
        let opens: usize = result.chars().filter(|&c| c == '{').count();
        let closes: usize = result.chars().filter(|&c| c == '}').count();
        assert_eq!(opens, closes, "unbalanced braces in injected layout");
    }

    #[test]
    fn inject_resume_args() {
        let layout = r#"pane command="claude" size="80%" focus=true"#;
        let result = inject_claude_args(layout, r#""-r" "some-uuid""#);
        assert!(result.contains(r#"args "-r" "some-uuid""#));
    }
}
