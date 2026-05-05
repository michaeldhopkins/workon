use anyhow::{bail, Result};
use vcs_runner::Cmd;

const ZELLIJ_HINT: &str = "brew install zellij";

/// Pre-flight check that all binaries the chosen layout will spawn are on PATH.
/// `zellij` is always required; everything else is derived from `command="..."`
/// occurrences in the layout content.
pub fn check_all(layout: &str) -> Result<()> {
    let mut problems = Vec::new();
    check_dep("zellij", ZELLIJ_HINT, &mut problems);

    for cmd in extract_commands(layout) {
        let hint = install_hint(&cmd);
        check_dep(&cmd, hint, &mut problems);
    }

    if problems.is_empty() {
        Ok(())
    } else {
        bail!("missing dependencies:\n{}", problems.join("\n"));
    }
}

fn check_dep(name: &str, hint: &str, problems: &mut Vec<String>) {
    match which::which(name) {
        Err(_) => {
            if hint.is_empty() {
                problems.push(format!("  {name} — not found on PATH"));
            } else {
                problems.push(format!("  {name} — not found. Install: {hint}"));
            }
        }
        Ok(path) => {
            if let Some(warning) = check_version(name, &path) {
                eprintln!("warning: {warning}");
            }
        }
    }
}

fn install_hint(name: &str) -> &'static str {
    match name {
        "claude" => "https://claude.ai/code",
        "branchdiff" => "brew install michaeldhopkins/tap/branchdiff",
        _ => "",
    }
}

/// Extract unique `command="X"` values from a layout, preserving first-seen order.
///
/// This is a naive line scanner, not a KDL parser: a commented-out line like
/// `// pane command="foo"` will still surface `foo` as a required dependency.
/// Acceptable trade-off — a real KDL parser would be overkill for the dep
/// pre-check, and users who want to disable a pane should remove the line.
fn extract_commands(layout: &str) -> Vec<String> {
    let mut found: Vec<String> = Vec::new();
    for line in layout.lines() {
        let mut rest = line;
        while let Some(idx) = rest.find("command=\"") {
            let after = &rest[idx + "command=\"".len()..];
            if let Some(end) = after.find('"') {
                let cmd = after[..end].to_string();
                if !cmd.is_empty() && !found.contains(&cmd) {
                    found.push(cmd);
                }
                rest = &after[end + 1..];
            } else {
                break;
            }
        }
    }
    found
}

fn check_version(name: &str, path: &std::path::Path) -> Option<String> {
    let output = Cmd::new(path.to_string_lossy().as_ref()).arg("--version").run().ok()?;
    let version_str = output.stdout_lossy();
    let version_str = version_str.trim();

    match name {
        "zellij" => {
            let version = version_str.strip_prefix("zellij ")?;
            let major_minor = parse_major_minor(version)?;
            if major_minor < (0, 40) {
                return Some(format!(
                    "zellij {version} is installed but 0.40+ is recommended. Upgrade: brew upgrade zellij"
                ));
            }
        }
        "branchdiff" => {
            let version = version_str.strip_prefix("branchdiff ")?;
            let major_minor = parse_major_minor(version)?;
            if major_minor < (0, 50) {
                return Some(format!(
                    "branchdiff {version} is installed but 0.50+ is recommended. Upgrade: brew upgrade branchdiff"
                ));
            }
        }
        _ => {}
    }

    None
}

fn parse_major_minor(version: &str) -> Option<(u32, u32)> {
    let mut parts = version.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    Some((major, minor))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_missing_dependency() {
        assert!(which::which("workon_fake_dep_abc123").is_err());
    }

    #[test]
    fn detects_present_dependency() {
        assert!(which::which("ls").is_ok());
    }

    #[test]
    fn parse_major_minor_works() {
        assert_eq!(parse_major_minor("0.43.1"), Some((0, 43)));
        assert_eq!(parse_major_minor("1.2.3"), Some((1, 2)));
        assert_eq!(parse_major_minor("bad"), None);
    }

    #[test]
    fn extract_commands_finds_pane_commands() {
        let layout = r#"layout {
    pane command="claude" size="80%"
    pane command="branchdiff" size="50%"
}"#;
        let cmds = extract_commands(layout);
        assert_eq!(cmds, vec!["claude".to_string(), "branchdiff".to_string()]);
    }

    #[test]
    fn extract_commands_dedupes() {
        let layout = r#"pane command="claude"
pane command="claude""#;
        let cmds = extract_commands(layout);
        assert_eq!(cmds, vec!["claude".to_string()]);
    }

    #[test]
    fn extract_commands_ignores_directives_and_strings() {
        let layout = r#"default_mode "locked"
session_serialization false
on_force_close "quit"
pane command="opencode""#;
        let cmds = extract_commands(layout);
        assert_eq!(cmds, vec!["opencode".to_string()]);
    }

    #[test]
    fn extract_commands_handles_multiple_per_line() {
        let layout = r#"foo command="a" bar command="b""#;
        let cmds = extract_commands(layout);
        assert_eq!(cmds, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn extract_commands_handles_empty_layout() {
        assert!(extract_commands("").is_empty());
        assert!(extract_commands("layout { }").is_empty());
    }

    #[test]
    fn install_hint_known_binaries() {
        assert!(install_hint("claude").contains("claude.ai"));
        assert!(install_hint("branchdiff").contains("brew install"));
        assert_eq!(install_hint("opencode"), "");
    }
}
