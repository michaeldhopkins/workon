//! The "Terminal UIs" rule, checked. `tests/tui.toml` names the test covering each place
//! workon hands the terminal to zellij or reads an answer from it. This fails when:
//! - a named test does not exist;
//! - a test spawns the binary without `env!("CARGO_BIN_EXE_…")` and `env_clear()`
//!   (`cargo_bin`, `cargo_bin_cmd!`), or sets PATH to anything but its stub directory;
//! - a test runs the binary on a pty without setting PATH there;
//! - `src/` runs a program by absolute path, one `programs` does not list (the harness stubs
//!   exactly that list), or one chosen at run time that `[[runtime]]` does not explain;
//! - a listed program or `[[runtime]]` call is no longer run;
//! - a test sleeps outside a deadline-bounded poll.
//!
//! Adapted from specdiff's `tests/tui_rules.rs`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use quote::ToTokens;
use syn::visit::Visit;

fn rs_files(dir: &Path, skip: Option<&Path>, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for entry in entries.flatten() {
        let path = entry.path();
        if skip.is_some_and(|s| path.starts_with(s)) {
            continue;
        }
        if path.is_dir() {
            rs_files(&path, skip, found);
        } else if path.extension().is_some_and(|e| e == "rs") {
            found.push(path);
        }
    }
}

fn parse(path: &Path) -> syn::File {
    let source = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    syn::parse_file(&source).unwrap_or_else(|e| panic!("{} does not parse: {e}", path.display()))
}

fn is_test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("test")
            || (a.path().is_ident("cfg") && a.parse_args::<syn::Meta>().is_ok_and(|m| m.path().is_ident("test")))
    })
}

fn path_string(expr: &syn::Expr) -> String {
    match expr {
        syn::Expr::Path(p) => p.path.segments.iter().map(|s| s.ident.to_string()).collect::<Vec<_>>().join("::"),
        _ => String::new(),
    }
}

fn last_segment(expr: &syn::Expr) -> Option<String> {
    match expr {
        syn::Expr::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        _ => None,
    }
}

fn str_lit(expr: &syn::Expr) -> Option<String> {
    match expr {
        syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) => Some(s.value()),
        _ => None,
    }
}

fn tokens(node: &impl ToTokens) -> String {
    node.to_token_stream().to_string().replace(' ', "")
}

#[derive(Default)]
struct TestFns(BTreeSet<String>);

impl<'a> Visit<'a> for TestFns {
    fn visit_item_fn(&mut self, f: &'a syn::ItemFn) {
        if f.attrs.iter().any(|a| a.path().is_ident("test")) {
            self.0.insert(f.sig.ident.to_string());
        }
        syn::visit::visit_item_fn(self, f);
    }
}

/// What a call in `src/` runs: `Some(Ok(name))` for a program named in the code,
/// `Some(Err(()))` for one chosen at run time, `None` for a call that runs nothing.
fn program_run_by(call: &syn::ExprCall) -> Option<Result<String, ()>> {
    let callee = path_string(&call.func);
    let first = call.args.first();
    if ["Command::new", "Cmd::new", "CommandBuilder::new"]
        .iter()
        .any(|c| callee == *c || callee.ends_with(&format!("::{c}")))
        || callee == "binary_available"
        || callee.ends_with("::binary_available")
    {
        return Some(first.and_then(str_lit).ok_or(()));
    }
    let name = last_segment(&call.func)?;
    if name.starts_with("run_jj") || name.starts_with("jj_") {
        Some(Ok("jj".into()))
    } else if name.starts_with("run_git") || name == "git_available" {
        Some(Ok("git".into()))
    } else {
        None
    }
}

struct Programs<'m> {
    file: String,
    allowed: &'m [String],
    runtime: &'m [String],
    ran: &'m mut BTreeSet<String>,
    out: Vec<String>,
}

impl<'a> Visit<'a> for Programs<'_> {
    fn visit_item_mod(&mut self, m: &'a syn::ItemMod) {
        if !is_test_only(&m.attrs) {
            syn::visit::visit_item_mod(self, m);
        }
    }

    fn visit_item_fn(&mut self, f: &'a syn::ItemFn) {
        if !is_test_only(&f.attrs) {
            syn::visit::visit_item_fn(self, f);
        }
    }

    fn visit_expr_call(&mut self, call: &'a syn::ExprCall) {
        match program_run_by(call) {
            Some(Ok(program)) if program.starts_with('/') => {
                self.out.push(format!("{}: runs {program} by absolute path, past every stub", self.file));
            }
            Some(Ok(program)) => {
                if !self.allowed.contains(&program) {
                    self.out
                        .push(format!("{}: runs {program}, which tests/tui.toml `programs` does not list", self.file));
                }
                self.ran.insert(program);
            }
            Some(Err(())) => {
                let text = format!("{}: {}", self.file, tokens(call));
                if !self.runtime.contains(&text) {
                    self.out.push(format!("{}: runs a program chosen at run time: {}", self.file, tokens(call)));
                }
                self.ran.insert(text);
            }
            None => {}
        }
        syn::visit::visit_expr_call(self, call);
    }
}

struct Harness {
    file: String,
    deadline_loops: Vec<bool>,
    out: Vec<String>,
}

impl Harness {
    fn check_fn(&mut self, name: &syn::Ident, body: &syn::Block) {
        let text = tokens(body);
        if text.contains("env!(\"CARGO_BIN_EXE_") && !text.contains(".env_clear()") {
            self.out.push(format!("{}: {name} spawns the binary without env_clear()", self.file));
        }
    }

    fn enter_loop(&mut self, body: &syn::Block) {
        self.deadline_loops.push(tokens(body).contains(".elapsed()"));
    }
}

impl<'a> Visit<'a> for Harness {
    fn visit_item_fn(&mut self, f: &'a syn::ItemFn) {
        self.check_fn(&f.sig.ident, &f.block);
        syn::visit::visit_item_fn(self, f);
    }

    fn visit_impl_item_fn(&mut self, f: &'a syn::ImplItemFn) {
        self.check_fn(&f.sig.ident, &f.block);
        syn::visit::visit_impl_item_fn(self, f);
    }

    fn visit_expr_loop(&mut self, l: &'a syn::ExprLoop) {
        self.enter_loop(&l.body);
        syn::visit::visit_expr_loop(self, l);
        self.deadline_loops.pop();
    }

    fn visit_expr_while(&mut self, l: &'a syn::ExprWhile) {
        self.enter_loop(&l.body);
        syn::visit::visit_expr_while(self, l);
        self.deadline_loops.pop();
    }

    fn visit_expr_for_loop(&mut self, l: &'a syn::ExprForLoop) {
        self.enter_loop(&l.body);
        syn::visit::visit_expr_for_loop(self, l);
        self.deadline_loops.pop();
    }

    fn visit_expr_call(&mut self, call: &'a syn::ExprCall) {
        match last_segment(&call.func).as_deref() {
            Some("sleep") if !self.deadline_loops.iter().any(|d| *d) => {
                self.out.push(format!("{}: sleeps outside a deadline-bounded poll: {}", self.file, tokens(call)));
            }
            Some("cargo_bin") => {
                self.out.push(format!("{}: spawns through cargo_bin, not env!(\"CARGO_BIN_EXE_…\")", self.file));
            }
            _ => {}
        }
        syn::visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'a syn::ExprMethodCall) {
        if call.method == "cargo_bin" {
            self.out.push(format!("{}: spawns through cargo_bin, not env!(\"CARGO_BIN_EXE_…\")", self.file));
        }
        syn::visit::visit_expr_method_call(self, call);
    }
}

const PATH_KEYS: [&str; 4] = ["\"PATH\",", "\"PATH\".into(),", "\"PATH\".to_string(),", "\"PATH\".to_owned(),"];

/// Checks on a test file's token text, which (unlike the syn walk) sees inside macros.
fn text_violations(file: &str, text: &str) -> Vec<String> {
    let mut out = Vec::new();
    if text.contains("cargo_bin_cmd!") {
        out.push(format!("{file}: spawns through cargo_bin_cmd!, not env!(\"CARGO_BIN_EXE_…\") with env_clear()"));
    }
    let on_a_pty = text.contains("portable_pty") && text.contains("env!(\"CARGO_BIN_EXE_");
    if on_a_pty && !PATH_KEYS.iter().any(|key| text.contains(key)) {
        out.push(format!("{file}: runs the binary on a pty without setting PATH to the stub directory"));
    }
    if text.contains("var(\"PATH\")") || text.contains("var_os(\"PATH\")") {
        out.push(format!("{file}: reads the inherited PATH"));
    }
    for key in PATH_KEYS {
        for (at, _) in text.match_indices(key) {
            let value = expression_at(&text[at + key.len()..]);
            if !value.contains("stubs.path()") {
                out.push(format!("{file}: sets PATH to {value}, not the stub directory's stubs.path()"));
            }
        }
    }
    out
}

fn expression_at(text: &str) -> &str {
    let mut depth = 0usize;
    for (i, c) in text.char_indices() {
        match c {
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' | ',' | ';' if depth == 0 => return &text[..i],
            ')' | ']' | '}' => depth -= 1,
            _ => {}
        }
    }
    text
}

fn strings(manifest: &toml::Value, key: &str) -> Vec<String> {
    manifest
        .get(key)
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(String::from))
        .collect()
}

fn table_field(manifest: &toml::Value, table: &str, field: &str) -> Vec<String> {
    manifest
        .get(table)
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| entry.get(field).and_then(toml::Value::as_str).map(String::from))
        .collect()
}

fn violations(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string(root.join("tests/tui.toml")).expect("tests/tui.toml"))
            .expect("tests/tui.toml parses");
    let programs = strings(&manifest, "programs");
    let runtime = table_field(&manifest, "runtime", "call");

    for table in ["pty", "key"] {
        for listed in table_field(&manifest, table, "test") {
            let Some((file, name)) = listed.split_once("::") else {
                out.push(format!("tests/tui.toml: {listed} is not <file>::<test>"));
                continue;
            };
            let path = root.join(file);
            if !path.exists() {
                out.push(format!("tests/tui.toml: {listed}: no file {file}"));
                continue;
            }
            let mut fns = TestFns::default();
            fns.visit_file(&parse(&path));
            if !fns.0.contains(name) {
                out.push(format!("tests/tui.toml: {listed}: no #[test] fn {name} in {file}"));
            }
        }
    }

    let mut ran = BTreeSet::new();
    let mut sources = Vec::new();
    rs_files(&root.join("src"), None, &mut sources);
    for path in sources {
        let mut visitor =
            Programs { file: rel(root, &path), allowed: &programs, runtime: &runtime, ran: &mut ran, out: Vec::new() };
        visitor.visit_file(&parse(&path));
        out.extend(visitor.out);
    }
    for listed in programs.iter().chain(&runtime) {
        if !ran.contains(listed) {
            out.push(format!("tests/tui.toml: {listed} is listed but src/ no longer runs it"));
        }
    }

    let mut tests = Vec::new();
    rs_files(&root.join("tests"), Some(&root.join("tests/fixtures")), &mut tests);
    // This file spawns nothing; its string literals are the patterns it looks for.
    tests.retain(|path| !path.ends_with("tests/tui_rules.rs"));
    for path in tests {
        let mut visitor = Harness { file: rel(root, &path), deadline_loops: Vec::new(), out: Vec::new() };
        let file = parse(&path);
        visitor.visit_file(&file);
        out.extend(visitor.out);
        out.extend(text_violations(&rel(root, &path), &tokens(&file)));
    }

    out.sort();
    out
}

fn rel(root: &Path, path: &Path) -> String {
    path.strip_prefix(root).unwrap_or(path).display().to_string()
}

#[test]
fn workon_meets_the_terminal_ui_rule() {
    let found = violations(Path::new(env!("CARGO_MANIFEST_DIR")));
    assert!(found.is_empty(), "Terminal UI rule violations:\n{}", found.join("\n"));
}

#[test]
fn the_check_finds_every_kind_of_violation_in_its_fixture() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tui_rules/bad");
    let found = violations(&fixture);
    let expected = [
        "src/main.rs: runs /usr/bin/security by absolute path, past every stub",
        "src/main.rs: runs a program chosen at run time: Cmd::new(editor)",
        "src/main.rs: runs a program chosen at run time: vcs_runner::binary_available(tool)",
        "src/main.rs: runs open, which tests/tui.toml `programs` does not list",
        "tests/pty_without_path.rs: runs the binary on a pty without setting PATH to the stub directory",
        "tests/tui.rs: quits spawns the binary without env_clear()",
        "tests/tui.rs: reads the inherited PATH",
        "tests/tui.rs: sets PATH to \"/usr/bin\".to_string(), not the stub directory's stubs.path()",
        "tests/tui.rs: sets PATH to std::env::var(\"PATH\").unwrap(), not the stub directory's stubs.path()",
        "tests/tui.rs: sleeps outside a deadline-bounded poll: std::thread::sleep(Duration::from_millis(50))",
        "tests/tui.rs: spawns through cargo_bin, not env!(\"CARGO_BIN_EXE_…\")",
        "tests/tui.rs: spawns through cargo_bin_cmd!, not env!(\"CARGO_BIN_EXE_…\") with env_clear()",
        "tests/tui.toml: src/main.rs: Cmd::new(gone) is listed but src/ no longer runs it",
        "tests/tui.toml: tests/missing.rs::gone: no file tests/missing.rs",
        "tests/tui.toml: tests/tui.rs::resize_repaints: no #[test] fn resize_repaints in tests/tui.rs",
        "tests/tui.toml: zellij is listed but src/ no longer runs it",
    ];
    assert_eq!(found, expected.map(String::from).to_vec());
}

#[test]
fn a_clean_fixture_has_no_violations() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tui_rules/good");
    assert_eq!(violations(&fixture), Vec::<String>::new());
}
