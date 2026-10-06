//! CI's gates scan for advisories and check formatting.
//!
//! Until 2026-10-06 ci.yml and release.yml ran `cargo deny check licenses` only, so RUSTSEC was
//! never consulted and three advisories sat in Cargo.lock unnoticed. `cargo fmt --check` was in no
//! gate either, and the tree had drifted from rustfmt by about 1,100 lines.

use std::path::Path;

/// Every `cargo deny` call that leaves out a check the full run makes, as `name:line: text`.
/// A bare `cargo deny check` runs all of them; a call that names checks must name
/// advisories, bans and licenses.
fn narrowed_deny_calls(name: &str, text: &str) -> Vec<String> {
    let mut found = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let Some(at) = line.find("cargo deny").or_else(|| line.find("cargo-deny")) else { continue };
        let words: Vec<&str> = line[at..].split_whitespace().collect();
        let Some(check) = words.iter().position(|w| *w == "check") else { continue };
        let named: Vec<&str> = words[check + 1..].iter().copied().filter(|w| !w.starts_with('-')).collect();
        if !named.is_empty() && ["advisories", "bans", "licenses"].iter().any(|c| !named.contains(c)) {
            found.push(format!("{name}:{}: {} runs only {}", n + 1, line.trim(), named.join(" ")));
        }
    }
    found
}

fn workflows() -> Vec<(String, String)> {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows");
    let mut found: Vec<(String, String)> = std::fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|x| x == "yml" || x == "yaml"))
        .map(|p| (p.file_name().unwrap().to_string_lossy().into_owned(), std::fs::read_to_string(&p).unwrap()))
        .collect();
    found.sort();
    found
}

#[test]
fn no_cargo_deny_call_is_narrowed_past_advisories() {
    let all = workflows();
    assert!(all.iter().any(|(n, _)| n == "ci.yml"), "found no ci.yml among {:?}", all.iter().map(|w| &w.0));
    let found: Vec<String> = all.iter().flat_map(|(n, t)| narrowed_deny_calls(n, t)).collect();
    assert!(found.is_empty(), "run a bare `cargo deny check` instead:\n{}", found.join("\n"));
}

#[test]
fn ci_and_release_gate_on_cargo_deny_and_cargo_fmt() {
    let all = workflows();
    for name in ["ci.yml", "release.yml"] {
        let (_, text) = all.iter().find(|(n, _)| n == name).unwrap_or_else(|| panic!("no {name}"));
        assert!(text.contains("run: cargo deny check"), "{name} runs no cargo deny check");
        for gate in ["cargo fmt --all --check\n", "cargo fmt --all --check --manifest-path fuzz/Cargo.toml"] {
            assert!(text.contains(gate), "{name} does not run {}", gate.trim());
        }
    }
}

#[test]
fn a_licenses_only_call_is_flagged() {
    let found = narrowed_deny_calls("ci.yml", "steps:\n  - run: cargo deny check licenses\n");
    assert_eq!(found, vec!["ci.yml:2: - run: cargo deny check licenses runs only licenses"]);
}

#[test]
fn a_call_missing_advisories_is_flagged() {
    assert_eq!(narrowed_deny_calls("x", "run: cargo deny --locked check bans licenses").len(), 1);
}

#[test]
fn bare_and_complete_calls_pass() {
    assert!(narrowed_deny_calls("x", "run: cargo deny check").is_empty());
    assert!(narrowed_deny_calls("x", "run: cargo deny check advisories bans licenses").is_empty());
    assert!(narrowed_deny_calls("x", "run: cargo deny check --hide-inclusion-graph").is_empty());
    assert!(narrowed_deny_calls("x", "- uses: taiki-e/install-action@cargo-deny").is_empty());
}

#[test]
fn the_hyphenated_binary_is_checked_too() {
    assert_eq!(narrowed_deny_calls("x", "run: cargo-deny check licenses").len(), 1);
}
