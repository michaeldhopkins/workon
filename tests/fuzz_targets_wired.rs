//! Fuzz-target wiring ratchet.
//! Per-repo configuration lives in `tests/ratchets.toml`.

use std::fs;
use std::path::Path;

#[rustfmt::skip]
struct Config {
    fuzz_min_targets: usize,
}

#[rustfmt::skip]
fn load_config(root: &Path) -> Config {
    let mut cfg = Config { fuzz_min_targets: 2 };
    let Ok(raw) = fs::read_to_string(root.join("tests/ratchets.toml")) else {
        return cfg;
    };
    let mut section = String::new();
    for raw_line in raw.lines() {
        let line = raw_line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            continue;
        }
        if !section.is_empty() {
            continue;
        }
        let Some((k, v)) = line.split_once('=') else {
            continue;
        };
        if k.trim() == "fuzz_min_targets" {
            cfg.fuzz_min_targets = v.trim().parse().expect("valid fuzz_min_targets");
        }
    }
    cfg
}

#[rustfmt::skip]
fn declared_targets() -> Vec<String> {
    let src = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("fuzz/Cargo.toml"))
        .expect("read fuzz/Cargo.toml");
    let mut out = Vec::new();
    let mut in_bin = false;
    for line in src.lines() {
        let t = line.trim();
        if t == "[[bin]]" {
            in_bin = true;
        } else if t.starts_with('[') {
            in_bin = false;
        } else if in_bin && let Some(rest) = t.strip_prefix("name = ") {
            out.push(rest.trim().trim_matches('"').to_string());
            in_bin = false;
        }
    }
    out.sort();
    out
}

#[rustfmt::skip]
fn workflow(name: &str) -> String {
    fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows").join(name))
        .unwrap_or_else(|e| panic!("read {name}: {e}"))
}

#[rustfmt::skip]
fn matrix_targets(name: &str) -> Vec<String> {
    let src = workflow(name);
    let line = src
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("target: ["))
        .unwrap_or_else(|| panic!("{name} declares no `target: [...]` matrix"));
    let inner = line.trim_start_matches("target: [").trim_end_matches(']');
    let mut out: Vec<String> =
        inner.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    out.sort();
    out
}

#[rustfmt::skip]
#[test]
fn fuzzing_runs_on_no_schedule() {
    let name = "fuzz-replay.yml";
    assert!(
        !workflow(name).lines().any(|l| l.trim() == "schedule:"),
        "{name} has a `schedule:` trigger; it is an ordinary per-push replay test."
    );
}

#[rustfmt::skip]
#[test]
fn every_fuzz_target_is_wired_into_the_per_push_replay() {
    let cfg = load_config(Path::new(env!("CARGO_MANIFEST_DIR")));
    let declared = declared_targets();
    assert!(
        declared.len() >= cfg.fuzz_min_targets,
        "only {} targets found — the Cargo.toml parse is wrong",
        declared.len()
    );
    assert_eq!(
        declared,
        matrix_targets("fuzz-replay.yml"),
        "fuzz/Cargo.toml and the per-push replay disagree."
    );
}

#[rustfmt::skip]
#[test]
fn every_fuzz_target_has_a_source_file_and_committed_seeds() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    for target in declared_targets() {
        let src = root.join("fuzz/fuzz_targets").join(format!("{target}.rs"));
        assert!(src.is_file(), "missing fuzz target source {}", src.display());
        let corpus = root.join("fuzz/corpus").join(&target);
        let seeds = fs::read_dir(&corpus)
            .unwrap_or_else(|e| panic!("reading {}: {e}", corpus.display()))
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().starts_with("seed-"))
            .count();
        assert!(seeds > 0, "fuzz/corpus/{target} has no committed seed-* inputs for replay");
    }
}
