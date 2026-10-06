//! workon's file-length gate.
//!
//! Its `production_lines` rule is the subtle part; see the comment on it. Function-level lints
//! (`clippy.toml`) never see a file growing one function at a time, and `src/workspace.rs` had
//! reached 1,178 production lines by the time this went in (2026-09-26).
//!
//! Two decisions make it useful rather than annoying:
//!
//! 1. **Tests don't count.** Rust convention keeps `#[cfg(test)] mod tests` in the same file, so
//!    counting whole files would mean "adding tests can break the build", the opposite of what we
//!    want. Only production lines are measured, and a file that is itself a test-only module
//!    (`#[cfg(test)] mod tests;` pointing at `tests.rs`) is not measured at all.
//!
//! 2. **It ratchets.** A file already over the limit is pinned at the size it was when the gate
//!    went in: allowed to shrink, never to grow, and a shrink must lower its pin in the same change
//!    (the test says to), so the file cannot grow back.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use syn::spanned::Spanned;
use syn::visit::Visit;

/// The ceiling for a file under `src`, in production lines.
const LIMIT: usize = 400;

/// Files over the limit, each pinned at its production size when the gate went in: it may
/// shrink, never grow. New code goes in a new module, never into a pinned file.
fn pinned() -> HashMap<&'static str, usize> {
    HashMap::from([("src/workspace.rs", 1115)])
}

/// Is this item compiled only for tests (`#[test]`, `#[tokio::test]`, `#[cfg(test)]`)?
fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().segments.last().is_some_and(|s| s.ident == "test")
            || (a.path().is_ident("cfg") && a.parse_args::<syn::Meta>().is_ok_and(|m| m.path().is_ident("test")))
    })
}

/// The 1-based line ranges of every test-only item, its attributes and doc comments included,
/// and the out-of-line test-only modules (`#[cfg(test)] mod tests;`) the file declares.
#[derive(Default)]
struct TestItems {
    lines: Vec<(usize, usize)>,
    out_of_line: Vec<OutOfLine>,
}

/// A test-only module whose body lives in another file.
struct OutOfLine {
    name: String,
    /// The `#[path = "…"]` it names, if any.
    path: Option<String>,
}

impl TestItems {
    fn add(&mut self, attrs: &[syn::Attribute], item: &impl Spanned) -> bool {
        if !test_only(attrs) {
            return false;
        }
        let span = item.span();
        self.lines.push((span.start().line, span.end().line));
        true
    }
}

fn path_attr(attrs: &[syn::Attribute]) -> Option<String> {
    attrs.iter().find(|a| a.path().is_ident("path")).and_then(|a| match &a.meta {
        syn::Meta::NameValue(nv) => match &nv.value {
            syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) => Some(s.value()),
            _ => None,
        },
        _ => None,
    })
}

impl<'a> Visit<'a> for TestItems {
    fn visit_item(&mut self, i: &'a syn::Item) {
        if let syn::Item::Mod(m) = i
            && m.content.is_none()
            && test_only(&m.attrs)
        {
            self.out_of_line.push(OutOfLine { name: m.ident.to_string(), path: path_attr(&m.attrs) });
        }
        let attrs = match i {
            syn::Item::Const(x) => &x.attrs,
            syn::Item::Enum(x) => &x.attrs,
            syn::Item::Fn(x) => &x.attrs,
            syn::Item::Impl(x) => &x.attrs,
            syn::Item::Macro(x) => &x.attrs,
            syn::Item::Mod(x) => &x.attrs,
            syn::Item::Static(x) => &x.attrs,
            syn::Item::Struct(x) => &x.attrs,
            syn::Item::Trait(x) => &x.attrs,
            syn::Item::Type(x) => &x.attrs,
            syn::Item::Use(x) => &x.attrs,
            _ => return syn::visit::visit_item(self, i),
        };
        if !self.add(attrs, i) {
            syn::visit::visit_item(self, i);
        }
    }
    fn visit_impl_item_fn(&mut self, f: &'a syn::ImplItemFn) {
        if !self.add(&f.attrs, f) {
            syn::visit::visit_impl_item_fn(self, f);
        }
    }
}

fn test_items(source: &str) -> syn::Result<TestItems> {
    let file = syn::parse_file(source)?;
    let mut tests = TestItems::default();
    tests.visit_file(&file);
    Ok(tests)
}

/// A source file's text. The gate fails on a file it cannot read rather than count it as empty.
fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// The file's lines outside its test-only items (`#[cfg(test)]` modules, helpers and impls,
/// `#[test]` functions), wherever they sit.
///
/// Read from the parsed file, never the text. Every text rule this gate had in its earlier
/// projects was fooled by a shape of ordinary code: taking any `#[cfg(test)]` as the start of the
/// tests waved through arbitrarily large files, stopping at the first test module measured a
/// 7,993-line file at 74, and ending a module at the first `}` in column 0 was fooled by fixture
/// strings.
fn production_lines(source: &str) -> syn::Result<usize> {
    let tests = test_items(source)?;
    let total = source.lines().count();
    Ok((1..=total).filter(|line| !tests.lines.iter().any(|(a, b)| (a..=b).contains(&line))).count())
}

/// Where the body of an out-of-line module declared in `file` lives, per the reference's module
/// rules: beside a `mod.rs`/`lib.rs`/`main.rs`, in a directory named after any other file, or at
/// its `#[path]`, relative to the declaring file's directory.
fn module_files(file: &Path, m: &OutOfLine) -> Vec<PathBuf> {
    let dir = file.parent().unwrap_or(Path::new(""));
    if let Some(p) = &m.path {
        return vec![dir.join(p)];
    }
    let owns_dir = matches!(file.file_name().and_then(|n| n.to_str()), Some("mod.rs" | "lib.rs" | "main.rs"));
    let base = if owns_dir { dir.to_path_buf() } else { dir.join(file.file_stem().unwrap_or_default()) };
    vec![base.join(format!("{}.rs", m.name)), base.join(&m.name).join("mod.rs")]
}

/// Every file under `src` that belongs to a test-only module: the file a `#[cfg(test)] mod x;`
/// points at, and everything beneath its directory.
fn test_module_files(files: &[PathBuf]) -> HashSet<PathBuf> {
    let mut roots = Vec::new();
    for file in files {
        let items = test_items(&read(file)).unwrap_or_else(|e| panic!("{} does not parse: {e}", file.display()));
        for m in items.out_of_line {
            roots.extend(module_files(file, &m));
        }
    }
    files.iter().filter(|f| roots.iter().any(|r| *f == r || f.starts_with(children_dir(r)))).cloned().collect()
}

/// Where a module file's own submodules live: beside a `mod.rs`, else in a directory named after
/// the file.
fn children_dir(module_file: &Path) -> PathBuf {
    if module_file.file_name().is_some_and(|n| n == "mod.rs") {
        module_file.parent().unwrap_or(Path::new("")).to_path_buf()
    } else {
        module_file.with_extension("")
    }
}

fn crate_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).to_path_buf()
}

/// Every `.rs` file under `dir`. Symlinks are not followed, so a link cycle cannot loop the walk,
/// and a directory it cannot read fails the gate rather than go unmeasured.
fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    let entries = std::fs::read_dir(dir).unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("cannot list {}: {e}", dir.display()));
        let path = entry.path();
        let kind = entry.file_type().unwrap_or_else(|e| panic!("cannot stat {}: {e}", path.display()));
        if kind.is_dir() {
            sources(&path, found);
        } else if kind.is_file() && path.extension().is_some_and(|e| e == "rs") {
            found.push(path);
        }
    }
}

/// The verdict for one file, or `None` when it is within its allowance. Split out from the
/// walk so the ratchet stays testable: with a tree that satisfies every pin, these branches
/// would go unexercised and rot.
fn verdict(relative: &str, lines: usize, limit: usize, pinned: &HashMap<&'static str, usize>) -> Option<String> {
    match pinned.get(relative) {
        Some(&ceiling) if lines > ceiling => Some(format!(
            "{relative}: {lines} lines, up from its pinned {ceiling}. It is already over the {limit}-line limit; split it rather than growing it further."
        )),
        Some(_) if lines <= limit => Some(format!(
            "{relative}: down to {lines} lines, under the {limit} limit, so remove its entry from `pinned()` and let the real limit hold it there."
        )),
        // The ratchet clicks: a pin left above the file's size would let it grow back.
        Some(&ceiling) if lines < ceiling => Some(format!(
            "{relative}: down to {lines} lines from its pinned {ceiling}. Lower its pin to {lines} so it cannot grow back."
        )),
        Some(_) => None,
        None if lines > limit => Some(format!(
            "{relative}: {lines} lines, over the {limit} limit. Split it into pieces that each do one thing (tests are not counted, so they are not the cause)."
        )),
        None => None,
    }
}

#[test]
fn no_file_outgrows_its_limit() {
    let root = crate_root();
    let pinned = pinned();
    let mut files = Vec::new();
    sources(&root.join("src"), &mut files);
    assert!(!files.is_empty(), "no `.rs` files under `src`: the walk is broken, not the tree");
    // workon keeps every test module inline today, so there is no real file to assert the
    // resolver finds; `a_test_only_module_file_is_found_from_its_declaration` covers it.
    let test_files = test_module_files(&files);
    let mut failures = Vec::new();
    for path in files.iter().filter(|f| !test_files.contains(*f)) {
        let relative = path.strip_prefix(&root).unwrap_or(path).to_string_lossy().replace('\\', "/");
        let lines = production_lines(&read(path)).unwrap_or_else(|e| panic!("{relative} does not parse: {e}"));
        if let Some(f) = verdict(&relative, lines, LIMIT, &pinned) {
            failures.push(f);
        }
    }
    failures.sort();
    assert!(failures.is_empty(), "\n{}\n", failures.join("\n"));
}

#[test]
fn the_ratchet_holds_a_pinned_file_to_its_size() {
    let pinned = HashMap::from([("a.rs", 500)]);
    assert!(verdict("a.rs", 500, 400, &pinned).is_none(), "at its ceiling is fine");
    assert!(
        verdict("a.rs", 450, 400, &pinned).is_some_and(|m| m.contains("Lower its pin to 450")),
        "a shrink must lower the pin, or the file could grow back"
    );
    assert!(
        verdict("a.rs", 501, 400, &pinned).is_some_and(|m| m.contains("up from its pinned")),
        "a pinned file may not grow"
    );
    assert!(
        verdict("a.rs", 400, 400, &pinned).is_some_and(|m| m.contains("remove its entry")),
        "once under the limit the pin must go"
    );
    assert!(verdict("b.rs", 400, 400, &pinned).is_none(), "unpinned, at the limit");
    assert!(verdict("b.rs", 401, 400, &pinned).is_some_and(|m| m.contains("over the")), "unpinned, over the limit");
}

#[test]
fn every_pinned_file_still_exists() {
    // A rename leaving a stale entry would exempt nothing, and the gate would quietly stop
    // protecting the file it names.
    let root = crate_root();
    let missing: Vec<&str> = pinned().keys().copied().filter(|r| !root.join(r).exists()).collect();
    assert!(missing.is_empty(), "pinned files no longer at these paths: {missing:?}");
}

#[test]
fn only_test_items_are_left_out_of_the_count() {
    let count = |source: &str| production_lines(source).expect("parses");
    assert_eq!(count("fn a() {}\nfn b() {}\n#[cfg(test)]\nmod tests {\n // lots\n}\n"), 2);
    assert_eq!(count("fn a() {}\n"), 1, "a file with no tests counts whole");
    assert_eq!(count(""), 0);
    let cases: &[(&str, &str, usize)] = &[
        (
            "a test-only mod declaration is its own line, not the rest of the file",
            "mod real;\n#[cfg(test)]\nmod test_support;\n\nfn a() {}\nfn b() {}\n",
            4,
        ),
        (
            "a test-only helper is the helper, not the rest of the file",
            "fn a() {}\n#[cfg(test)]\nfn helper() {}\nfn b() {}\nfn c() {}\n",
            3,
        ),
        (
            "an attribute between the guard and the item goes with the item",
            "#[cfg(test)]\n#[path = \"t.rs\"]\nmod tests;\n\nfn a() {}\nfn b() {}\n",
            3,
        ),
        ("a same-line test module body is still skipped", "fn a() {}\n#[cfg(test)] mod tests {\n    fn t() {}\n}\n", 1),
        ("a one-line test module", "fn a() {}\n#[cfg(test)] mod tests { fn t() {} }\nfn b() {}\n", 2),
        (
            "production code after a test module still counts",
            "fn a() {}\n#[cfg(test)]\nmod a_tests {\n    #[test]\n    fn t() {\n    }\n}\n\nfn b() {}\nfn c() {}\n",
            4,
        ),
        (
            "a `}` in column 0 inside a test's string does not end the module",
            "#[cfg(test)]\nmod tests {\n    const FIX: &str = \"\n}\n\";\n    fn t() {}\n}\nfn b() {}\n",
            1,
        ),
        (
            "`#[cfg(test)]` in a string or comment is not an attribute",
            "// #[cfg(test)]\nconst A: &str = \"\n#[cfg(test)]\nmod x {\";\nfn b() {}\n",
            5,
        ),
        ("a test-only method in a production impl", "impl A {\n    fn a() {}\n    #[cfg(test)]\n    fn t() {}\n}\n", 3),
        ("doc comments go with their item", "/// tests\n#[cfg(test)]\nmod t {}\nfn b() {}\n", 1),
        ("an async test under a runtime's attribute is a test", "#[tokio::test]\nasync fn t() {\n}\nfn b() {}\n", 1),
    ];
    for (why, source, expected) in cases {
        assert_eq!(count(source), *expected, "{why}");
    }
    assert!(production_lines("fn a( {").is_err(), "a file that does not parse is an error, not a count");
}

#[test]
#[cfg(unix)]
fn the_walk_does_not_follow_symlinks() {
    let root = std::env::temp_dir().join(format!("workon-walk-{}", std::process::id()));
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.rs"), "fn a() {}\n").unwrap();
    std::os::unix::fs::symlink(&root, root.join("src/loop")).unwrap();
    let mut found = Vec::new();
    sources(&root.join("src"), &mut found);
    std::fs::remove_dir_all(&root).unwrap();
    assert_eq!(found, vec![root.join("src/a.rs")], "a link back to an ancestor is not walked into");
}

#[test]
fn a_test_only_module_file_is_found_from_its_declaration() {
    let files: Vec<PathBuf> = [
        "src/vcs/git/mod.rs",
        "src/vcs/git/tests.rs",
        "src/vcs/git/tests/helpers.rs",
        "src/vcs/git/commands.rs",
        "src/app.rs",
        "src/app/fixtures/mod.rs",
        "src/app/fixtures/data.rs",
        "src/app/view.rs",
    ]
    .iter()
    .map(PathBuf::from)
    .collect();
    let beside_mod_rs = OutOfLine { name: "tests".into(), path: None };
    assert!(
        module_files(Path::new("src/vcs/git/mod.rs"), &beside_mod_rs).contains(&PathBuf::from("src/vcs/git/tests.rs"))
    );
    let under_named_dir = OutOfLine { name: "fixtures".into(), path: None };
    let roots = module_files(Path::new("src/app.rs"), &under_named_dir);
    assert!(
        roots.contains(&PathBuf::from("src/app/fixtures/mod.rs")),
        "a non-mod.rs file's children live in a directory named after it"
    );
    let with_path = OutOfLine { name: "t".into(), path: Some("elsewhere/t.rs".into()) };
    assert_eq!(
        module_files(Path::new("src/vcs/git/mod.rs"), &with_path),
        vec![PathBuf::from("src/vcs/git/elsewhere/t.rs")]
    );

    let tests_dir = std::env::temp_dir().join(format!("workon-file-length-{}", std::process::id()));
    for f in &files {
        let p = tests_dir.join(f);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        let body = match f.to_str().unwrap() {
            "src/vcs/git/mod.rs" => "mod commands;\n#[cfg(test)]\nmod tests;\n",
            "src/app.rs" => "mod view;\n#[cfg(test)]\nmod fixtures;\n",
            _ => "fn f() {}\n",
        };
        std::fs::write(&p, body).unwrap();
    }
    let on_disk: Vec<PathBuf> = files.iter().map(|f| tests_dir.join(f)).collect();
    let found = test_module_files(&on_disk);
    std::fs::remove_dir_all(&tests_dir).unwrap();
    let expected: HashSet<PathBuf> =
        ["src/vcs/git/tests.rs", "src/vcs/git/tests/helpers.rs", "src/app/fixtures/mod.rs", "src/app/fixtures/data.rs"]
            .iter()
            .map(|f| tests_dir.join(f))
            .collect();
    assert_eq!(
        found, expected,
        "the test module files and everything under a test module's directory, and nothing else"
    );
}
