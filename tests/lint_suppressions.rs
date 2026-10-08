//! The lint-suppression ratchet.
//!
//! Two things let a lint stop biting without anyone deciding it should: an `#[allow(…)]` or
//! `#[expect(…)]` on an item, and a `clippy.toml` threshold raised above clippy's default. Both
//! are pinned at their level when this went in (2026-10-08). Either may only move toward clean,
//! and the pin moves with it in the same change, so nothing regrows. When you touch an item
//! carrying a suppression, remove it (extract the options struct, split the function) rather
//! than work around it.
//!
//! Suppressions are counted on tokens, not text, so a string or comment mentioning one is not
//! a suppression, while one inside a macro invocation (a `proptest!` block) still is.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use proc_macro2::{Delimiter, TokenStream, TokenTree};

/// The trees scanned, relative to the workspace root.
const ROOTS: &[&str] = &["src", "tests", "fuzz/fuzz_targets"];

/// `#[allow]`/`#[expect]` attributes in the tree. May only fall. Today: `provision_in`'s
/// eight parameters (`src/workspace.rs`) and `SessionArgs`' four bools (`src/cli.rs`).
const SUPPRESSIONS: usize = 2;

/// Clippy's defaults for the thresholds a `clippy.toml` can raise. A key missing from here
/// fails the check, so a newly loosened threshold cannot slip past it unmeasured.
const CLIPPY_DEFAULTS: &[(&str, i64)] = &[
    ("too-many-arguments-threshold", 7),
    ("cognitive-complexity-threshold", 25),
    ("too-many-lines-threshold", 100),
    ("type-complexity-threshold", 250),
    ("max-struct-bools", 3),
    ("max-fn-params-bools", 3),
];

/// Thresholds above their default, pinned at today's value.
const PINNED_THRESHOLDS: &[(&str, i64)] =
    &[("cognitive-complexity-threshold", 30), ("too-many-arguments-threshold", 8)];

/// Settings in `clippy.toml` that are switches rather than thresholds.
const SWITCHES: &[&str] = &["allow-unwrap-in-tests"];

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            sources(&path, found);
        } else if path.extension().is_some_and(|e| e == "rs") {
            found.push(path);
        }
    }
}

/// Whether an attribute's bracketed body suppresses a lint: `allow(…)`, `expect(…)`, or a
/// `cfg_attr(…)` carrying either.
fn suppresses(body: &TokenStream) -> bool {
    let mut tokens = body.clone().into_iter();
    match tokens.next() {
        Some(TokenTree::Ident(name)) if name == "allow" || name == "expect" => true,
        Some(TokenTree::Ident(name)) if name == "cfg_attr" => tokens.any(|t| match t {
            TokenTree::Group(g) => g
                .stream()
                .into_iter()
                .any(|inner| matches!(&inner, TokenTree::Ident(i) if i == "allow" || i == "expect")),
            _ => false,
        }),
        _ => false,
    }
}

/// Suppression attributes in a token stream, descending into every group.
fn count_suppressions(stream: TokenStream) -> usize {
    let tokens: Vec<TokenTree> = stream.into_iter().collect();
    let mut count = 0;
    for (i, token) in tokens.iter().enumerate() {
        match token {
            TokenTree::Punct(p) if p.as_char() == '#' => {
                let body = match (tokens.get(i + 1), tokens.get(i + 2)) {
                    (Some(TokenTree::Group(g)), _) if g.delimiter() == Delimiter::Bracket => Some(g),
                    (Some(TokenTree::Punct(bang)), Some(TokenTree::Group(g)))
                        if bang.as_char() == '!' && g.delimiter() == Delimiter::Bracket =>
                    {
                        Some(g)
                    }
                    _ => None,
                };
                if body.is_some_and(|g| suppresses(&g.stream())) {
                    count += 1;
                }
            }
            TokenTree::Group(g) => count += count_suppressions(g.stream()),
            _ => {}
        }
    }
    count
}

fn suppressions_in(source: &str) -> usize {
    let stream: TokenStream = source.parse().unwrap_or_else(|e| panic!("does not lex: {e:?}"));
    count_suppressions(stream)
}

/// `key = integer` settings from a `clippy.toml`; switches and comments are skipped.
fn thresholds(clippy_toml: &str) -> BTreeMap<String, i64> {
    clippy_toml
        .lines()
        .map(|line| line.split('#').next().unwrap_or_default().trim())
        .filter_map(|line| line.split_once('='))
        .filter_map(|(key, value)| Some((key.trim().to_string(), value.trim().parse().ok()?)))
        .collect()
}

fn suppression_verdict(count: usize, pin: usize) -> Option<String> {
    if count > pin {
        Some(format!("{count} lint suppressions, up from {pin}. Fix the cause instead of silencing the lint."))
    } else if count < pin {
        Some(format!("{count} lint suppressions, down from {pin}. Lower SUPPRESSIONS to {count}."))
    } else {
        None
    }
}

/// Failures for one `clippy.toml`, given clippy's defaults and today's pins.
fn threshold_verdicts(set: &BTreeMap<String, i64>, defaults: &[(&str, i64)], pins: &[(&str, i64)]) -> Vec<String> {
    let mut failures = Vec::new();
    for (key, &value) in set {
        let Some(&(_, default)) = defaults.iter().find(|(k, _)| k == key) else {
            failures.push(format!(
                "clippy.toml sets `{key}`, which has no default in CLIPPY_DEFAULTS. Add clippy's \
                 default so the check can tell whether it is loosened."
            ));
            continue;
        };
        let pin = pins.iter().find(|(k, _)| k == key).map(|&(_, p)| p);
        match pin {
            _ if value <= default && pin.is_some() => failures
                .push(format!("`{key}` is back to {value}, within clippy's default of {default}; remove its pin.")),
            _ if value <= default => {}
            None => {
                failures.push(format!("`{key}` = {value} loosens clippy's default of {default}. Fix the code instead."))
            }
            Some(p) if value > p => failures
                .push(format!("`{key}` = {value}, up from its pinned {p}. A limit is never raised to fit the code.")),
            Some(p) if value < p => {
                failures.push(format!("`{key}` down to {value} from {p}; lower its pin to {value}."));
            }
            Some(_) => {}
        }
    }
    for (key, _) in pins {
        if !set.contains_key(*key) {
            failures.push(format!("`{key}` is pinned but no longer set; remove its pin."));
        }
    }
    failures
}

#[test]
fn lint_suppressions_only_fall() {
    let root = workspace_root();
    let mut files = Vec::new();
    for dir in ROOTS {
        let before = files.len();
        sources(&root.join(dir), &mut files);
        assert!(files.len() > before, "no `.rs` files under `{dir}`; the walk is broken");
    }
    let count: usize =
        files.iter().map(|path| suppressions_in(&std::fs::read_to_string(path).unwrap_or_default())).sum();
    let verdict = suppression_verdict(count, SUPPRESSIONS);
    assert!(verdict.is_none(), "{}", verdict.unwrap_or_default());
}

#[test]
fn clippy_thresholds_only_move_toward_the_default() {
    let toml = std::fs::read_to_string(workspace_root().join("clippy.toml"))
        .unwrap_or_else(|e| panic!("reading clippy.toml: {e}"));
    let mut set = thresholds(&toml);
    set.retain(|key, _| !SWITCHES.contains(&key.as_str()));
    assert!(!set.is_empty(), "no thresholds read from clippy.toml; the parser is broken");
    let failures = threshold_verdicts(&set, CLIPPY_DEFAULTS, PINNED_THRESHOLDS);
    assert!(failures.is_empty(), "\n{}\n", failures.join("\n"));
}

#[test]
fn suppressions_are_counted_on_tokens() {
    assert_eq!(suppressions_in("#[allow(dead_code)] fn a() {}"), 1);
    assert_eq!(suppressions_in("#![expect(clippy::too_many_lines)]"), 1);
    assert_eq!(suppressions_in("#[cfg_attr(test, allow(unused))] fn a() {}"), 1);
    assert_eq!(suppressions_in("mod m { fn a() { #[allow(unused)] let x = 1; } }"), 1);
    assert_eq!(suppressions_in("proptest! { #[allow(unused)] fn p() {} }"), 1, "inside a macro");
    assert_eq!(suppressions_in(r##"const S: &str = "#[allow(x)]"; // #[allow(y)]"##), 0);
    assert_eq!(suppressions_in("#[cfg(test)] #[derive(Debug)] /// doc\n struct S;"), 0);
}

#[test]
fn the_suppression_count_ratchets() {
    assert!(suppression_verdict(2, 2).is_none());
    assert!(suppression_verdict(3, 2).is_some_and(|m| m.contains("up from 2")));
    assert!(suppression_verdict(1, 2).is_some_and(|m| m.contains("Lower SUPPRESSIONS to 1")));
}

#[test]
fn thresholds_ratchet_toward_the_default() {
    let defaults = [("t", 10)];
    let pins = [("t", 20)];
    let with = |v: i64| BTreeMap::from([("t".to_string(), v)]);
    assert!(threshold_verdicts(&with(20), &defaults, &pins).is_empty(), "at its pin");
    assert!(threshold_verdicts(&with(21), &defaults, &pins)[0].contains("up from its pinned"));
    assert!(threshold_verdicts(&with(15), &defaults, &pins)[0].contains("lower its pin to 15"));
    assert!(threshold_verdicts(&with(10), &defaults, &pins)[0].contains("remove its pin"));
    assert!(threshold_verdicts(&with(11), &defaults, &[])[0].contains("loosens"));
    assert!(threshold_verdicts(&with(5), &defaults, &[]).is_empty(), "stricter is fine");
    assert!(threshold_verdicts(&BTreeMap::new(), &defaults, &pins)[0].contains("no longer set"));
    let unknown = BTreeMap::from([("mystery".to_string(), 1)]);
    assert!(threshold_verdicts(&unknown, &defaults, &[])[0].contains("no default"));
}

#[test]
fn thresholds_are_read_from_clippy_toml() {
    let read = thresholds("# note\nmax-struct-bools = 2 # why\nallow-unwrap-in-tests = true\n");
    assert_eq!(read, BTreeMap::from([("max-struct-bools".to_string(), 2)]));
}

/// The `[lints]` baseline, the house Rust lint table: each lint at no weaker than this level. Removing one, or easing it from deny to warn,
/// fails. `unsafe_code = "forbid"` is not here because the tests call `std::env::set_var`;
/// `undocumented_unsafe_blocks` stands in for it.
const BASELINE: &[(&str, &str, Level)] = &[
    ("clippy", "struct_excessive_bools", Level::Deny),
    ("clippy", "fn_params_excessive_bools", Level::Deny),
    ("clippy", "collapsible_if", Level::Deny),
    ("clippy", "too_many_arguments", Level::Deny),
    ("clippy", "needless_range_loop", Level::Deny),
    ("clippy", "extend_with_drain", Level::Deny),
    ("clippy", "unwrap_used", Level::Deny),
    ("clippy", "dbg_macro", Level::Deny),
    ("clippy", "undocumented_unsafe_blocks", Level::Deny),
    ("clippy", "match_bool", Level::Warn),
    ("clippy", "bool_to_int_with_if", Level::Warn),
    ("clippy", "cognitive_complexity", Level::Warn),
    ("clippy", "too_many_lines", Level::Warn),
    ("rust", "unexpected_cfgs", Level::Warn),
];

/// Lint levels in order of strength; `warn` is enough because CI runs clippy with `-D warnings`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Allow,
    Warn,
    Deny,
    Forbid,
}

fn level(name: &str) -> Option<Level> {
    match name {
        "allow" | "expect" => Some(Level::Allow),
        "warn" => Some(Level::Warn),
        "deny" => Some(Level::Deny),
        "forbid" => Some(Level::Forbid),
        _ => None,
    }
}

/// A lint's level and, for the table form, its `check-cfg` list. Keys are read with hyphens and
/// underscores alike, since cargo accepts both.
fn lint_entry(lints: &toml::Table, group: &str, lint: &str) -> Option<(Option<Level>, Vec<String>)> {
    let table = lints.get(group)?.as_table()?;
    let value = table.iter().find(|(k, _)| k.replace('-', "_") == lint).map(|(_, v)| v)?;
    match value {
        toml::Value::String(s) => Some((level(s), Vec::new())),
        toml::Value::Table(t) => {
            let level = t.get("level").and_then(toml::Value::as_str).and_then(level);
            let cfgs = t
                .get("check-cfg")
                .and_then(toml::Value::as_array)
                .map(|a| a.iter().filter_map(toml::Value::as_str).map(String::from).collect())
                .unwrap_or_default();
            Some((level, cfgs))
        }
        _ => Some((None, Vec::new())),
    }
}

/// Failures for a `Cargo.toml`'s `[lints]` against the baseline.
fn baseline_verdicts(cargo_toml: &str, baseline: &[(&str, &str, Level)]) -> Vec<String> {
    let doc: toml::Table = match cargo_toml.parse() {
        Ok(doc) => doc,
        Err(e) => return vec![format!("Cargo.toml does not parse: {e}")],
    };
    let empty = toml::Table::new();
    let lints = doc.get("lints").and_then(toml::Value::as_table).unwrap_or(&empty);
    let mut failures = Vec::new();
    for &(group, lint, want) in baseline {
        match lint_entry(lints, group, lint) {
            None => failures.push(format!("[lints.{group}] lacks `{lint}`; set it to {want:?} or stronger.")),
            Some((Some(have), _)) if have < want => {
                failures.push(format!("[lints.{group}] `{lint}` is {have:?}, weaker than the baseline {want:?}."));
            }
            Some((None, _)) => failures.push(format!("[lints.{group}] `{lint}` has no level workon can read.")),
            Some(_) => {}
        }
    }
    // A misspelt `#[cfg(fuzzng)]` would otherwise apply silently never.
    if !lint_entry(lints, "rust", "unexpected_cfgs").is_some_and(|(_, cfgs)| cfgs.iter().any(|c| c == "cfg(fuzzing)")) {
        failures.push("[lints.rust] `unexpected_cfgs` must list `cfg(fuzzing)` in check-cfg.".to_string());
    }
    failures
}

#[test]
fn the_lints_table_keeps_the_baseline() {
    let toml = std::fs::read_to_string(workspace_root().join("Cargo.toml"))
        .unwrap_or_else(|e| panic!("reading Cargo.toml: {e}"));
    let failures = baseline_verdicts(&toml, BASELINE);
    assert!(failures.is_empty(), "\n{}\n", failures.join("\n"));
}

#[test]
fn baseline_lints_may_not_be_dropped_or_weakened() {
    let baseline = [("clippy", "unwrap_used", Level::Deny), ("clippy", "too_many_lines", Level::Warn)];
    let cfg = "\n[lints.rust]\nunexpected_cfgs = { level = \"warn\", check-cfg = [\"cfg(fuzzing)\"] }\n";
    let with = |clippy: &str| format!("[lints.clippy]\n{clippy}{cfg}");

    assert!(baseline_verdicts(&with("unwrap_used = \"deny\"\ntoo-many-lines = \"warn\"\n"), &baseline).is_empty());
    assert!(baseline_verdicts(&with("unwrap_used = \"forbid\"\ntoo_many_lines = \"deny\"\n"), &baseline).is_empty());
    let weaker = baseline_verdicts(&with("unwrap_used = \"warn\"\ntoo_many_lines = \"warn\"\n"), &baseline);
    assert!(weaker.len() == 1 && weaker[0].contains("weaker"), "{weaker:?}");
    let missing = baseline_verdicts(&with("too_many_lines = \"warn\"\n"), &baseline);
    assert!(missing.len() == 1 && missing[0].contains("lacks `unwrap_used`"), "{missing:?}");
    let allowed = baseline_verdicts(&with("unwrap_used = \"allow\"\ntoo_many_lines = \"warn\"\n"), &baseline);
    assert!(allowed.len() == 1 && allowed[0].contains("weaker"), "{allowed:?}");
    let table = baseline_verdicts(
        &with("unwrap_used = { level = \"deny\", priority = 1 }\ntoo_many_lines = \"warn\"\n"),
        &baseline,
    );
    assert!(table.is_empty(), "{table:?}");

    let no_cfg = baseline_verdicts("[lints.clippy]\nunwrap_used = \"deny\"\ntoo_many_lines = \"warn\"\n", &baseline);
    assert!(no_cfg.len() == 1 && no_cfg[0].contains("cfg(fuzzing)"), "{no_cfg:?}");
}
