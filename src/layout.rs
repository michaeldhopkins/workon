use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use tempfile::NamedTempFile;

const EMBEDDED_LAYOUT: &str = include_str!("../layouts/workon.kdl");

const CREATING_A_CONFIG_URL: &str =
    "https://github.com/michaeldhopkins/workon#creating-a-config";

pub struct ResolvedLayout {
    temp: NamedTempFile,
}

impl ResolvedLayout {
    pub fn path(&self) -> &Path {
        self.temp.path()
    }
}

/// Read raw layout content for the given config name (no injection).
///
/// `None` or `Some("default")` resolves to, in order:
///   1. `~/.config/workon/configs/default.kdl`
///   2. `~/.config/workon/layout.kdl` (legacy single-config path)
///   3. The embedded layout
///
/// Any other name resolves only to `~/.config/workon/configs/<name>.kdl`,
/// erroring if the file is absent.
pub fn read_config(config: Option<&str>) -> Result<String> {
    let workon_dir = config_dir()?.join("workon");
    read_config_from(&workon_dir, config)
}

pub fn resolve_layout(config: Option<&str>) -> Result<ResolvedLayout> {
    let workon_dir = config_dir()?.join("workon");
    resolve_layout_from(&workon_dir, config)
}

pub fn resolve_workspace_layout(config: Option<&str>, claude_session_id: &str) -> Result<ResolvedLayout> {
    let workon_dir = config_dir()?.join("workon");
    resolve_workspace_layout_from(&workon_dir, config, claude_session_id)
}

pub fn resolve_resume_layout(config: Option<&str>, claude_session_id: &str) -> Result<ResolvedLayout> {
    let workon_dir = config_dir()?.join("workon");
    resolve_resume_layout_from(&workon_dir, config, claude_session_id)
}

/// Return the `command="..."` value of the layout's focused pane (the one
/// marked `focus=true`). Falls back to the first commanded pane if nothing
/// is explicitly focused. Returns `None` when the layout has no commanded
/// panes at all.
///
/// Bails if **more than one** commanded pane is marked `focus=true` — that's
/// almost always a typo, and our mismatch guard would otherwise silently
/// pick whichever line we saw first.
///
/// Used to detect whether a running zellij session matches the requested
/// layout: if the focused command isn't somewhere in the session's process
/// tree, the user almost certainly launched the session with a different
/// config and attaching would silently apply that config's layout instead.
pub fn focused_command(layout: &str) -> Result<Option<String>> {
    let focused: Vec<&str> = layout
        .lines()
        .filter(|line| line.contains("focus=true"))
        .filter_map(command_in_line)
        .collect();

    if focused.len() > 1 {
        bail!(
            "your layout has {} panes marked focus=true ({}). Mark only one. workon uses the focused pane to tell which config a session was launched with.\n\n\
             See: {}",
            focused.len(),
            focused.join(", "),
            CREATING_A_CONFIG_URL,
        );
    }

    if let Some(cmd) = focused.first() {
        return Ok(Some((*cmd).to_string()));
    }

    Ok(layout.lines().filter_map(command_in_line).next().map(String::from))
}

/// Eager validation hook for layouts. Currently just probes `focused_command`
/// so the multi-focus error surfaces before any subprocesses run. Add other
/// checks here as needed.
pub fn validate_layout(layout: &str) -> Result<()> {
    let _ = focused_command(layout)?;
    Ok(())
}

fn command_in_line(line: &str) -> Option<&str> {
    let after = line.split("command=\"").nth(1)?;
    let end = after.find('"')?;
    Some(&after[..end])
}

/// Bail if `layout_content` references no `command="claude"` pane. Used to
/// reject `--resume` against configs that don't drive Claude.
pub fn ensure_resume_compatible(config_name: &str, layout_content: &str) -> Result<()> {
    if !layout_content.contains("command=\"claude\"") {
        bail!(
            "--resume only works with claude-based configs (active config: {config_name})"
        );
    }
    Ok(())
}

fn resolve_layout_from(workon_dir: &Path, config: Option<&str>) -> Result<ResolvedLayout> {
    build(read_config_from(workon_dir, config)?)
}

fn resolve_workspace_layout_from(
    workon_dir: &Path,
    config: Option<&str>,
    claude_session_id: &str,
) -> Result<ResolvedLayout> {
    let raw = read_config_from(workon_dir, config)?;
    let args = format!("\"--session-id\" \"{claude_session_id}\"");
    build(inject_claude_args(&raw, &args))
}

fn resolve_resume_layout_from(
    workon_dir: &Path,
    config: Option<&str>,
    claude_session_id: &str,
) -> Result<ResolvedLayout> {
    let raw = read_config_from(workon_dir, config)?;
    let args = format!("\"-r\" \"{claude_session_id}\"");
    build(inject_claude_args(&raw, &args))
}

fn build(content: String) -> Result<ResolvedLayout> {
    let tmp = NamedTempFile::with_suffix(".kdl")?;
    std::fs::write(tmp.path(), &content)?;
    Ok(ResolvedLayout { temp: tmp })
}

fn read_config_from(workon_dir: &Path, config: Option<&str>) -> Result<String> {
    let configs_dir = workon_dir.join("configs");
    match config {
        None | Some("default") => {
            let default_path = configs_dir.join("default.kdl");
            if default_path.is_file() {
                return Ok(std::fs::read_to_string(&default_path)?);
            }
            let legacy = workon_dir.join("layout.kdl");
            if legacy.is_file() {
                return Ok(std::fs::read_to_string(&legacy)?);
            }
            Ok(EMBEDDED_LAYOUT.to_string())
        }
        Some(name) => {
            if !is_valid_config_name(name) {
                bail!("invalid config name '{name}': use letters, digits, '-', or '_'");
            }
            let path = configs_dir.join(format!("{name}.kdl"));
            if !path.is_file() {
                bail!(
                    "workon config '{}' not found.\n\
                     Looked at: {}\n\
                     How to create one: {}",
                    name,
                    path.display(),
                    CREATING_A_CONFIG_URL,
                );
            }
            Ok(std::fs::read_to_string(&path)?)
        }
    }
}

fn is_valid_config_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
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
    fn default_uses_embedded_when_nothing_present() {
        let tmp = tempfile::tempdir().unwrap();
        let content = read_config_from(tmp.path(), None).unwrap();
        assert!(content.contains("default_mode"));
        assert!(content.contains("branchdiff"));
        assert!(content.contains("claude"));
    }

    #[test]
    fn default_uses_legacy_layout_kdl_when_no_configs_default() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("layout.kdl"), "LEGACY").unwrap();

        let content = read_config_from(tmp.path(), None).unwrap();
        assert_eq!(content, "LEGACY");
    }

    #[test]
    fn default_prefers_configs_default_over_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("layout.kdl"), "LEGACY").unwrap();
        std::fs::create_dir_all(tmp.path().join("configs")).unwrap();
        std::fs::write(tmp.path().join("configs/default.kdl"), "NEW_DEFAULT").unwrap();

        let content = read_config_from(tmp.path(), None).unwrap();
        assert_eq!(content, "NEW_DEFAULT");
    }

    #[test]
    fn explicit_default_name_uses_same_resolution_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("layout.kdl"), "LEGACY").unwrap();

        let content = read_config_from(tmp.path(), Some("default")).unwrap();
        assert_eq!(content, "LEGACY");
    }

    #[test]
    fn named_config_loads_from_configs_dir() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("configs")).unwrap();
        std::fs::write(tmp.path().join("configs/opencode.kdl"), "OPENCODE_LAYOUT").unwrap();

        let content = read_config_from(tmp.path(), Some("opencode")).unwrap();
        assert_eq!(content, "OPENCODE_LAYOUT");
    }

    #[test]
    fn named_config_missing_errors_with_helpful_message() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_config_from(tmp.path(), Some("missing")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'missing'"), "{msg}");
        assert!(msg.contains("configs/missing.kdl"), "{msg}");
        assert!(msg.contains("#creating-a-config"), "{msg}");
    }

    #[test]
    fn named_config_does_not_fall_back_to_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("layout.kdl"), "LEGACY").unwrap();

        let err = read_config_from(tmp.path(), Some("opencode")).unwrap_err();
        assert!(err.to_string().contains("opencode"));
    }

    #[test]
    fn build_writes_content_to_tempfile_at_path() {
        let resolved = build("HELLO".to_string()).unwrap();
        assert_eq!(std::fs::read_to_string(resolved.path()).unwrap(), "HELLO");
    }

    #[test]
    fn rejects_empty_config_name() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_config_from(tmp.path(), Some("")).unwrap_err();
        assert!(err.to_string().contains("invalid config name"), "{err}");
    }

    #[test]
    fn rejects_path_traversal_in_config_name() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_config_from(tmp.path(), Some("../etc/hosts")).unwrap_err();
        assert!(err.to_string().contains("invalid config name"), "{err}");
    }

    #[test]
    fn rejects_subdirectory_config_name() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_config_from(tmp.path(), Some("foo/bar")).unwrap_err();
        assert!(err.to_string().contains("invalid config name"), "{err}");
    }

    #[test]
    fn rejects_dotfile_config_name() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_config_from(tmp.path(), Some(".hidden")).unwrap_err();
        assert!(err.to_string().contains("invalid config name"), "{err}");
    }

    #[test]
    fn accepts_valid_config_name_with_dash_and_underscore() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("configs")).unwrap();
        std::fs::write(tmp.path().join("configs/my-cfg_2.kdl"), "OK").unwrap();
        let content = read_config_from(tmp.path(), Some("my-cfg_2")).unwrap();
        assert_eq!(content, "OK");
    }

    #[test]
    fn is_valid_config_name_rules() {
        assert!(is_valid_config_name("opencode"));
        assert!(is_valid_config_name("my-cfg_2"));
        assert!(is_valid_config_name("ABC123"));
        assert!(!is_valid_config_name(""));
        assert!(!is_valid_config_name("a/b"));
        assert!(!is_valid_config_name("a b"));
        assert!(!is_valid_config_name(".dot"));
        assert!(!is_valid_config_name(".."));
    }

    #[test]
    fn resolve_workspace_layout_from_reads_named_config_and_injects_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("configs")).unwrap();
        std::fs::write(
            tmp.path().join("configs/myclaude.kdl"),
            "pane command=\"claude\" size=\"80%\"\n",
        )
        .unwrap();

        let resolved = resolve_workspace_layout_from(tmp.path(), Some("myclaude"), "abc-123").unwrap();
        let content = std::fs::read_to_string(resolved.path()).unwrap();
        assert!(content.contains("command=\"claude\""));
        assert!(content.contains(r#"args "--session-id" "abc-123""#));
    }

    #[test]
    fn resolve_resume_layout_from_reads_named_config_and_injects_resume_args() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("configs")).unwrap();
        std::fs::write(
            tmp.path().join("configs/myclaude.kdl"),
            "pane command=\"claude\" size=\"80%\"\n",
        )
        .unwrap();

        let resolved = resolve_resume_layout_from(tmp.path(), Some("myclaude"), "uuid-xyz").unwrap();
        let content = std::fs::read_to_string(resolved.path()).unwrap();
        assert!(content.contains(r#"args "-r" "uuid-xyz""#));
    }

    #[test]
    fn resolve_layout_from_falls_back_to_embedded() {
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_layout_from(tmp.path(), None).unwrap();
        let content = std::fs::read_to_string(resolved.path()).unwrap();
        assert!(content.contains("command=\"claude\""));
        assert!(content.contains("command=\"branchdiff\""));
    }

    #[test]
    fn focused_command_returns_command_on_focus_true_line() {
        let layout = r#"layout {
    pane command="claude" size="80%" focus=true
    pane command="branchdiff" size="50%"
}"#;
        assert_eq!(focused_command(layout).unwrap(), Some("claude".to_string()));
    }

    #[test]
    fn focused_command_picks_focused_when_not_first() {
        let layout = r#"layout {
    pane command="branchdiff" size="50%"
    pane command="opencode" size="80%" focus=true
}"#;
        assert_eq!(focused_command(layout).unwrap(), Some("opencode".to_string()));
    }

    #[test]
    fn focused_command_falls_back_to_first_when_no_focus() {
        let layout = r#"pane command="branchdiff"
pane command="specdiff""#;
        assert_eq!(focused_command(layout).unwrap(), Some("branchdiff".to_string()));
    }

    #[test]
    fn focused_command_returns_none_when_no_commands() {
        let layout = r#"layout {
    pane size="20%"
    pane size="80%"
}"#;
        assert_eq!(focused_command(layout).unwrap(), None);
    }

    #[test]
    fn focused_command_finds_focus_in_embedded_layout() {
        assert_eq!(focused_command(EMBEDDED_LAYOUT).unwrap(), Some("claude".to_string()));
    }

    #[test]
    fn focused_command_errors_when_multiple_panes_are_focused() {
        let layout = r#"layout {
    pane command="claude" size="80%" focus=true
    pane command="branchdiff" size="50%" focus=true
}"#;
        let err = focused_command(layout).expect_err("should error on multi-focus");
        let msg = err.to_string();
        assert!(msg.contains("2 panes"), "{msg}");
        assert!(msg.contains("claude"), "{msg}");
        assert!(msg.contains("branchdiff"), "{msg}");
        assert!(msg.contains("Mark only one"), "{msg}");
        assert!(msg.contains("#creating-a-config"), "{msg}");
    }

    #[test]
    fn focused_command_ignores_focus_on_panes_without_command() {
        // A focused empty pane shouldn't count toward the multi-focus check —
        // it's not something the mismatch guard could match against anyway.
        let layout = r#"layout {
    pane command="claude" focus=true
    pane size="20%" focus=true
}"#;
        assert_eq!(focused_command(layout).unwrap(), Some("claude".to_string()));
    }

    #[test]
    fn validate_layout_passes_for_well_formed_layout() {
        assert!(validate_layout(EMBEDDED_LAYOUT).is_ok());
    }

    #[test]
    fn validate_layout_rejects_multi_focus() {
        let layout = r#"pane command="claude" focus=true
pane command="branchdiff" focus=true"#;
        assert!(validate_layout(layout).is_err());
    }

    #[test]
    fn ensure_resume_compatible_passes_when_layout_has_claude() {
        let layout = r#"pane command="claude" size="80%""#;
        assert!(ensure_resume_compatible("default", layout).is_ok());
    }

    #[test]
    fn ensure_resume_compatible_errors_when_layout_lacks_claude() {
        let layout = r#"pane command="opencode" size="80%""#;
        let err = ensure_resume_compatible("opencode", layout).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("--resume"), "{msg}");
        assert!(msg.contains("opencode"), "{msg}");
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

    #[test]
    fn inject_args_noop_when_no_claude_pane() {
        let layout = r#"pane command="opencode" size="80%""#;
        let result = inject_claude_args(layout, r#""-r" "some-uuid""#);
        assert_eq!(result, layout);
    }
}
