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

// CI is bounded (agents/dependencies-and-ci.md): every workflow sets a concurrency group and a
// top-level `permissions`, and every job that runs steps sets `timeout-minutes`. Until
// 2026-10-07 ci.yml had none of the three, so a hung job ran for GitHub's six-hour default with
// a write token. The workflows are read by indentation, not by a YAML parser: they are ours,
// written two spaces a level, and the unit tests below pin how the reader sees them.

/// The lines under the top-level `key:`, up to the next top-level key. `None` when absent.
fn top_level_block<'a>(text: &'a str, key: &str) -> Option<Vec<&'a str>> {
    let mut lines = text.lines();
    lines.by_ref().find(|l| l.trim_end() == format!("{key}:") || l.starts_with(&format!("{key}: ")))?;
    Some(lines.take_while(|l| l.is_empty() || l.starts_with(' ') || l.starts_with('#')).collect())
}

/// Each job under `jobs:`, with the keys written directly on it (four spaces in).
fn jobs(text: &str) -> Vec<(String, Vec<String>)> {
    let mut found: Vec<(String, Vec<String>)> = Vec::new();
    for line in top_level_block(text, "jobs").unwrap_or_default() {
        let indent = line.len() - line.trim_start().len();
        let Some((key, _)) = line.trim().split_once(':') else { continue };
        if line.trim_start().starts_with('#') || key.starts_with('-') {
            continue;
        }
        match indent {
            2 => found.push((key.to_string(), Vec::new())),
            4 => {
                if let Some(job) = found.last_mut() {
                    job.1.push(key.to_string());
                }
            }
            _ => {}
        }
    }
    found
}

/// What a workflow leaves unbounded, as `name: problem` lines.
fn unbounded(name: &str, text: &str) -> Vec<String> {
    let mut found = Vec::new();
    if !top_level_block(text, "concurrency").is_some_and(|b| b.iter().any(|l| l.trim().starts_with("group:"))) {
        found.push(format!("{name}: no top-level concurrency group"));
    }
    if top_level_block(text, "permissions").is_none() {
        found.push(format!("{name}: no top-level permissions"));
    }
    let all = jobs(text);
    if all.is_empty() {
        found.push(format!("{name}: no jobs found"));
    }
    for (job, keys) in all {
        // A job that calls a reusable workflow (`uses:`) takes its timeouts from that workflow.
        if !keys.iter().any(|k| k == "timeout-minutes" || k == "uses") {
            found.push(format!("{name}: job {job} sets no timeout-minutes"));
        }
    }
    found
}

#[test]
fn every_workflow_is_bounded() {
    let all = workflows();
    assert!(all.len() >= 5, "found only {:?}", all.iter().map(|w| &w.0).collect::<Vec<_>>());
    let found: Vec<String> = all.iter().flat_map(|(n, t)| unbounded(n, t)).collect();
    assert!(
        found.is_empty(),
        "set these (agents/dependencies-and-ci.md, \"CI jobs are bounded\"):\n{}",
        found.join("\n")
    );
}

#[test]
fn a_bounded_workflow_passes() {
    let text = concat!(
        "name: X\non:\n  push:\nconcurrency:\n  group: ${{ github.workflow }}-${{ github.ref }}\n",
        "permissions:\n  contents: read\njobs:\n  a:\n    runs-on: ubuntu-latest\n    timeout-minutes: 5\n",
        "    steps:\n      - run: echo timeout-minutes: 1\n  b:\n    uses: ./.github/workflows/y.yml\n",
    );
    assert_eq!(unbounded("x.yml", text), Vec::<String>::new());
    assert_eq!(
        jobs(text),
        vec![
            ("a".into(), vec!["runs-on".into(), "timeout-minutes".into(), "steps".into()]),
            ("b".into(), vec!["uses".into()])
        ]
    );
}

#[test]
fn an_unbounded_workflow_is_caught() {
    // The timeout on a step (six spaces in) does not bound the job.
    let text = "name: X\nconcurrency: fuzz\njobs:\n  a:\n    runs-on: ubuntu-latest\n    steps:\n      - run: x\n        timeout-minutes: 5\n";
    assert_eq!(
        unbounded("x.yml", text),
        vec![
            "x.yml: no top-level concurrency group",
            "x.yml: no top-level permissions",
            "x.yml: job a sets no timeout-minutes"
        ]
    );
    assert_eq!(
        unbounded("y.yml", "name: Y\n"),
        vec!["y.yml: no top-level concurrency group", "y.yml: no top-level permissions", "y.yml: no jobs found"]
    );
}

/// The toolchain is pinned to an exact stable, and no workflow installs a moving one over it.
/// A new stable can add a lint or change formatting and turn CI red on a change that touched
/// neither. The fuzz workflows name `+nightly` on every cargo call, which outranks the pin.
#[test]
fn the_toolchain_is_pinned_and_every_workflow_installs_the_pin() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let pin = std::fs::read_to_string(root.join("rust-toolchain.toml")).expect("rust-toolchain.toml");
    let table: toml::Table = pin.parse().expect("rust-toolchain.toml parses");
    let channel = table["toolchain"]["channel"].as_str().expect("a channel");
    let parts: Vec<&str> = channel.split('.').collect();
    assert!(
        parts.len() == 3 && parts.iter().all(|p| p.parse::<u32>().is_ok()),
        "channel {channel:?} is not an exact x.y.z"
    );
    let components = table["toolchain"]["components"].as_array().expect("components");
    for wanted in ["clippy", "rustfmt"] {
        assert!(components.iter().any(|c| c.as_str() == Some(wanted)), "rust-toolchain.toml lacks {wanted}");
    }
    for (name, text) in workflows() {
        for moving in ["dtolnay/rust-toolchain@stable", "dtolnay/rust-toolchain@beta"] {
            assert!(!text.contains(moving), "{name} installs {moving} over the pin; run `rustup toolchain install`");
        }
    }
}

/// Dependencies move through the workon-deps upkeep job, never Dependabot: the owner has no
/// pull-request workflow to feed (agents/dependencies-and-ci.md).
#[test]
fn dependencies_are_kept_current_by_the_upkeep_job() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for name in ["dependabot.yml", "dependabot.yaml"] {
        assert!(!root.join(".github").join(name).exists(), ".github/{name}: the workon-deps upkeep job does this");
    }
    let agents = std::fs::read_to_string(root.join("AGENTS.md")).expect("AGENTS.md");
    assert!(agents.contains("`workon-deps`"), "AGENTS.md does not name the workon-deps upkeep job");
}

/// The docs build with warnings denied (a broken intra-doc link fails), an unused dependency
/// fails, and the declared rust-version is built on its own toolchain.
#[test]
fn ci_builds_docs_checks_unused_dependencies_and_the_msrv() {
    let all = workflows();
    let (_, ci) = all.iter().find(|(n, _)| n == "ci.yml").expect("ci.yml");
    assert!(
        ci.contains("run: cargo doc --locked --no-deps") && ci.contains("RUSTDOCFLAGS: -D warnings"),
        "ci.yml builds no docs with warnings denied"
    );
    assert!(ci.contains("run: cargo machete"), "ci.yml runs no cargo machete");
    assert!(ci.contains("check --locked") && ci.contains("rust-version"), "ci.yml checks no MSRV build");
}
