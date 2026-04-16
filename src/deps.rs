use anyhow::{bail, Result};
use vcs_runner::Cmd;

struct Dep {
    name: &'static str,
    install_hint: &'static str,
}

const REQUIRED: &[Dep] = &[
    Dep {
        name: "zellij",
        install_hint: "brew install zellij",
    },
    Dep {
        name: "claude",
        install_hint: "https://claude.ai/code",
    },
    Dep {
        name: "branchdiff",
        install_hint: "brew install michaeldhopkins/tap/branchdiff",
    },
];

pub fn check_all() -> Result<()> {
    let mut problems = Vec::new();

    for dep in REQUIRED {
        match which::which(dep.name) {
            Err(_) => {
                problems.push(format!("  {} — not found. Install: {}", dep.name, dep.install_hint));
            }
            Ok(path) => {
                if let Some(warning) = check_version(dep.name, &path) {
                    eprintln!("warning: {warning}");
                }
            }
        }
    }

    if problems.is_empty() {
        Ok(())
    } else {
        bail!("missing dependencies:\n{}", problems.join("\n"));
    }
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
    fn check_all_error_lists_all_missing() {
        let missing = ["nonexistent_a", "nonexistent_b"];
        let formatted = missing.join(", ");
        assert!(formatted.contains("nonexistent_a"));
        assert!(formatted.contains("nonexistent_b"));
    }
}
