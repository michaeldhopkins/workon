//! The environment `mise env` computes for a directory, handed to the session and
//! to provisioners.

use std::collections::HashMap;
use std::path::Path;

use vcs_runner::Cmd;

/// `mise env --json` for `dir`. Empty when mise is absent or fails, since a
/// project without mise is normal; a warning when mise ran but printed something
/// that is not the JSON object it documents, so a lost env is never silent.
///
/// `--json` exists in every release named `mise` (2024.1.0 onward; checked
/// against `src/cli/env.rs` at that tag), so no version fallback is needed.
pub fn mise_env(dir: &Path) -> HashMap<String, String> {
    let Ok(output) = Cmd::new("mise").args(["env", "--json"]).in_dir(dir).run() else {
        return HashMap::new();
    };
    parse(&output.stdout_lossy()).unwrap_or_else(|e| {
        eprintln!("Warning: could not read `mise env --json` output ({e}); the session starts without mise's env");
        HashMap::new()
    })
}

/// JSON, not the shell form: that quotes each value for a shell (`'it'\''s'`) and
/// prints a multi-line value over several lines, so reading it back needs a shell.
/// Non-string values are skipped; mise only emits strings.
fn parse(output: &str) -> Result<HashMap<String, String>, serde_json::Error> {
    let map: HashMap<String, serde_json::Value> = serde_json::from_str(output)?;
    Ok(map.into_iter().filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string()))).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_values_verbatim() {
        // Captured from `mise env --json` (mise 2026.2.21). The shell form of the same
        // env printed `'it'\''s`, `'say "hi"'` and a value split over two lines,
        // which the old line parser read back as `it'\''s`, `say "hi` and `line1`.
        let output = r#"{
  "PATH": "/usr/local/bin:/usr/bin",
  "W_DQUOTE": "say \"hi\"",
  "W_EMPTY": "",
  "W_NL": "line1\nline2",
  "W_SQUOTE": "it's",
  "W_TRAILQ": "x'"
}"#;
        let vars = parse(output).unwrap();
        assert_eq!(vars.get("PATH").unwrap(), "/usr/local/bin:/usr/bin");
        assert_eq!(vars.get("W_DQUOTE").unwrap(), "say \"hi\"");
        assert_eq!(vars.get("W_EMPTY").unwrap(), "");
        assert_eq!(vars.get("W_NL").unwrap(), "line1\nline2");
        assert_eq!(vars.get("W_SQUOTE").unwrap(), "it's");
        assert_eq!(vars.get("W_TRAILQ").unwrap(), "x'");
        assert_eq!(vars.len(), 6);
    }

    #[test]
    fn skips_non_strings() {
        let vars = parse(r#"{"A": "a", "B": 1, "C": null}"#).unwrap();
        assert_eq!(vars, HashMap::from([("A".to_string(), "a".to_string())]));
    }

    #[test]
    fn output_that_is_not_a_json_object_is_an_error() {
        assert!(parse("export A=a\n").is_err(), "shell-form output is not JSON");
        assert!(parse("").is_err());
        assert!(parse("[1]").is_err());
    }
}
