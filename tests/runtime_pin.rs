//! The fixtures' runtimes are named once, in mise.toml, and every workflow reads them from there.
//!
//! Before this test ci.yml wrote each version itself (Ruby 3.4, Node 22, PHP 8.4, OTP 27,
//! Elixir 1.18), so a move to a new major meant finding every copy, and nothing noticed when one
//! fell behind.

use std::path::Path;

/// The setup input that names each mise.toml tool's version.
const INPUTS: [(&str, &str); 6] = [
    ("ruby", "ruby-version"),
    ("node", "node-version"),
    ("php", "php-version"),
    ("dotnet", "dotnet-version"),
    ("erlang", "otp-version"),
    ("elixir", "elixir-version"),
];

/// Every way the workflows can drift from the pin, as messages naming the file and line.
fn violations(mise: &str, workflows: &[(String, String)]) -> Vec<String> {
    let mut found = Vec::new();
    let tools: toml::Table = match mise.parse::<toml::Table>() {
        Ok(t) => t.get("tools").and_then(|t| t.as_table()).cloned().unwrap_or_default(),
        Err(e) => return vec![format!("mise.toml does not parse: {e}")],
    };
    for (tool, input) in INPUTS {
        if !tools.contains_key(tool) {
            found.push(format!("mise.toml pins no {tool}"));
        }
        let wanted = format!("{input}: ${{{{ steps.pin.outputs.{tool} }}}}");
        for (name, text) in workflows {
            for (n, line) in text.lines().enumerate() {
                let line = line.trim();
                if line.starts_with(&format!("{input}:")) && !line.starts_with(&wanted) {
                    found.push(format!("{name}:{}: {line} does not read the pin", n + 1));
                }
            }
        }
    }
    for (name, text) in workflows {
        if text.contains("steps.pin.outputs.") && !text.contains("mise.toml") {
            found.push(format!("{name} reads steps.pin but no step reads mise.toml"));
        }
    }
    found
}

#[test]
fn every_workflow_reads_the_fixture_runtimes_from_mise_toml() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mise = std::fs::read_to_string(root.join("mise.toml")).expect("mise.toml");
    let mut workflows = Vec::new();
    for entry in std::fs::read_dir(root.join(".github/workflows")).expect("workflows") {
        let path = entry.expect("entry").path();
        let name = path.file_name().expect("name").to_string_lossy().into_owned();
        workflows.push((name, std::fs::read_to_string(&path).expect("workflow")));
    }
    assert!(!workflows.is_empty(), "no workflows found");
    assert_eq!(violations(&mise, &workflows), Vec::<String>::new());
    for stray in [".tool-versions", ".nvmrc", ".node-version", ".ruby-version", ".python-version"] {
        assert!(!root.join(stray).exists(), "{stray} would be a second pin");
    }
}

#[test]
fn a_hard_coded_or_missing_version_is_caught() {
    let mise = "[tools]\nruby = \"4.0\"\n";
    let workflow = "steps:\n  - run: sed mise.toml\n  - with:\n      ruby-version: \"3.4\"\n      node-version: ${{ steps.pin.outputs.node }}\n";
    let found = violations(mise, &[("ci.yml".into(), workflow.into())]);
    assert!(found.iter().any(|f| f.contains("ci.yml:4: ruby-version: \"3.4\"")), "{found:?}");
    assert!(found.iter().any(|f| f == "mise.toml pins no node"), "{found:?}");
    assert!(!found.iter().any(|f| f.contains("node-version")), "{found:?}");
}
