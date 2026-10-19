//! A workspace's identity is the name jj registered for the workspace whose root
//! is that directory. Parsing `<project>-<ws_id>` off the directory name is wrong
//! the moment jj's name and the directory disagree (workon issue #5), so a project
//! that has `.jj` never does it. A git-only worktree has no jj registration; the
//! directory spelling is the only name it has, and that fallback stays.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use vcs_runner::run_jj;

use crate::discover;

/// `jj` reads `\0` and `\n` as escapes. A real NUL in this string would split the
/// argument before jj's template parser saw it.
///
/// Each record is `name.escape_json()`, a NUL, then `stringify(root).escape_json()`
/// (or `null`), and a final NUL. A `RefSymbol` whose display form is quoted
/// (`my workspace`, `feat@v2`) comes back as the name jj registered. The root
/// is JSON, so a trailing space or an embedded newline stays in the string.
const LIST_TEMPLATE: &str =
    "name.escape_json() ++ \"\\0\" ++ if(root, stringify(root).escape_json(), \"null\") ++ \"\\0\"";

struct Registered {
    name: String,
    root: Option<PathBuf>,
    /// Both fields re-encode to the bytes jj emitted. A field that does not
    /// is never a directory we delete.
    exact: bool,
}

struct Known {
    name: String,
    root: PathBuf,
    nickname: Option<String>,
}

enum Asked<'a> {
    Token(&'a str),
    Path(&'a Path),
}

enum Found<'a> {
    One(&'a Known),
    None,
    Many(Vec<&'a Known>),
}

pub(crate) struct Census {
    pub(crate) names: BTreeMap<PathBuf, String>,
    pub(crate) unmanaged: Vec<Unmanaged>,
}

/// A jj registration workon will not offer as a workspace it can destroy.
/// Sorted by name, then by the project it belongs to.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Unmanaged {
    name: String,
    project: PathBuf,
    detail: String,
}

/// The project a worktree belongs to. Git's common dir wins when it resolves;
/// otherwise the `.jj/repo` pointer (a file in a secondary workspace, a directory
/// in the main repo). When both fail, the git error is the one returned.
pub(crate) fn project_of(ws_dir: &Path) -> Result<PathBuf> {
    match discover::project_dir_of(ws_dir) {
        Ok(dir) => Ok(dir),
        Err(err) => jj_project(ws_dir).ok_or(err),
    }
}

/// The jj-registered name of the workspace rooted at `ws_dir`, or the
/// `<project>-<ws_id>` spelling when the project is not a jj repo. `None` becomes
/// the caller's existing "is not named" error; a jj failure is not turned into that.
pub(crate) fn identity(project_dir: &Path, project_name: &str, ws_dir: &Path) -> Result<String> {
    let found = if project_dir.join(".jj").is_dir() {
        name_at_root(project_dir, ws_dir)?
    } else {
        discover::ws_id_of(ws_dir, project_name)
    };
    found.with_context(|| format!("{} is not named <project>-<ws_id> for project {project_name}", ws_dir.display()))
}

/// The single workspace `token` names: a jj registration, or a git-only worktree's
/// ws_id / nickname. Same miss and ambiguity errors as before.
pub(crate) fn root_for_token(token: &str) -> Result<PathBuf> {
    match find(&collect_known()?, &Asked::Token(token)) {
        Found::One(hit) => Ok(hit.root.clone()),
        Found::None => bail!("no workspace matches '{token}' (see: workon list)"),
        Found::Many(hits) => {
            let listed =
                hits.iter().map(|hit| hit.root.to_string_lossy().into_owned()).collect::<Vec<_>>().join("\n  ");
            bail!("'{token}' is ambiguous; matches:\n  {listed}")
        }
    }
}

/// Names of workspaces workon manages (root strictly inside `worktrees`), and the
/// registrations in scope of `cwd` that it does not. A jj failure propagates: list
/// must not fall back to directory names when it could not ask jj.
pub(crate) fn census(worktrees: &Path, cwd: &Path) -> Result<Census> {
    let cwd = canonical(cwd);
    let worktrees = canonical(worktrees);
    let mut names = BTreeMap::new();
    let mut unmanaged = Vec::new();
    for project in projects_for(&worktrees, &cwd) {
        classify(&project, &cwd, &worktrees, &mut names, &mut unmanaged)?;
    }
    unmanaged.sort();
    unmanaged.dedup();
    Ok(Census { names, unmanaged })
}

/// Every repo has a jj workspace named `default`, so that line would show on
/// every list. Any other name is still reported.
fn reports_unmanaged(name: &str) -> bool {
    name != "default"
}

pub(crate) fn report_unmanaged(rows: &[Unmanaged]) {
    for row in rows.iter().filter(|row| reports_unmanaged(&row.name)) {
        eprintln!("jj workspace {} is registered and workon does not manage it ({})", row.name, row.detail);
    }
}

/// What `list` and completion should call a worktree. Jj's registered name when
/// the project has `.jj` (never a parse of the directory); the directory spelling
/// otherwise. `None` when a jj project has no registration at this root — the
/// caller shows the directory's own name rather than a stripped one.
pub(crate) fn listed_name(
    names: &BTreeMap<PathBuf, String>,
    ws_dir: &Path,
    project_dir: &Path,
    project_name: Option<&str>,
) -> Option<String> {
    if project_dir.join(".jj").is_dir() {
        return names.get(&canonical(ws_dir)).cloned();
    }
    project_name.and_then(|project| discover::ws_id_of(ws_dir, project))
}

fn known_at(reg: Registered) -> Option<Known> {
    if !reg.exact {
        return None;
    }
    Some(Known { name: reg.name, root: canonical(&reg.root?), nickname: None })
}

fn list_registered(project: &Path) -> Result<Vec<Registered>> {
    if !project.join(".jj").is_dir() {
        return Ok(Vec::new());
    }
    // Not `run_jj_utf8`: that trims the whole stdout, which drops a trailing
    // space on the last field. The bytes jj printed are the record.
    let output = run_jj(project, &["--ignore-working-copy", "--color=never", "workspace", "list", "-T", LIST_TEMPLATE])
        .with_context(|| format!("jj workspace list in {}", project.display()))?;
    let stdout = String::from_utf8(output.stdout)
        .with_context(|| format!("jj workspace list in {} was not utf-8", project.display()))?;
    Ok(parse_list(&stdout))
}

fn parse_list(output: &str) -> Vec<Registered> {
    let mut fields = output.split('\0');
    let mut out = Vec::new();
    while let (Some(name_raw), Some(root_raw)) = (fields.next(), fields.next()) {
        let Some((name, name_exact)) = parse_string_field(name_raw) else { continue };
        if name.is_empty() {
            continue;
        }
        let Some((root, root_exact)) = parse_root_field(root_raw) else { continue };
        out.push(Registered { name, root, exact: name_exact && root_exact });
    }
    out
}

/// One JSON string field, untrimmed. `exact` when re-encoding equals `raw`.
fn parse_string_field(raw: &str) -> Option<(String, bool)> {
    let serde_json::Value::String(text) = serde_json::from_str(raw).ok()? else { return None };
    let encoded = serde_json::to_string(&text).ok()?;
    Some((text, encoded == raw))
}

/// `raw` is one JSON root field, untrimmed. `None` when it is not JSON.
/// `exact` is false when re-encoding the value is not `raw` (jj escaped it
/// differently than we would), so the path is not safe to delete.
fn parse_root_field(raw: &str) -> Option<(Option<PathBuf>, bool)> {
    let value: serde_json::Value = serde_json::from_str(raw).ok()?;
    match value {
        serde_json::Value::Null => Some((None, raw == "null")),
        serde_json::Value::String(text) => {
            let encoded = serde_json::to_string(&text).ok()?;
            let exact = encoded == raw && Path::new(&text).to_str() == Some(text.as_str());
            let root = (!text.is_empty()).then(|| PathBuf::from(text));
            Some((root, exact))
        }
        _ => None,
    }
}

fn name_at_root(project_dir: &Path, ws_dir: &Path) -> Result<Option<String>> {
    name_from(list_registered(project_dir)?, ws_dir)
}

/// The registered name whose root is exactly `ws_dir`. An inexact root that
/// canonicalizes here is a refusal: deleting would trust a path the parser
/// could not prove.
fn name_from(regs: Vec<Registered>, ws_dir: &Path) -> Result<Option<String>> {
    let wanted = canonical(ws_dir);
    let hits: Vec<&Registered> = regs
        .iter()
        .filter(|reg| reg.root.as_deref().is_some_and(|root| same_path(&canonical(root), &wanted)))
        .collect();
    if hits.iter().any(|reg| !reg.exact) {
        bail!("refusing to delete {}: workspace root does not round-trip", wanted.display());
    }
    // Two names for one directory are resolved in a later check. Neither is deleted.
    let known: Vec<Known> =
        hits.iter().map(|reg| Known { name: reg.name.clone(), root: wanted.clone(), nickname: None }).collect();
    Ok(match find(&known, &Asked::Path(&wanted)) {
        Found::One(hit) => Some(hit.name.clone()),
        Found::None | Found::Many(_) => None,
    })
}

fn collect_known() -> Result<Vec<Known>> {
    let cwd = std::env::current_dir()?;
    let worktrees = discover::worktrees_dir()?;
    let mut known = Vec::new();
    for project in projects_for(&worktrees, &cwd) {
        add_jj_project(&project, &mut known)?;
    }
    for dir in discover::list_dirs_in(&worktrees) {
        add_directory(&dir, &mut known);
    }
    Ok(known)
}

fn add_jj_project(project: &Path, known: &mut Vec<Known>) -> Result<()> {
    for reg in list_registered(project)? {
        let Some(mut hit) = known_at(reg) else { continue };
        if !hit.root.exists() || known.iter().any(|k| same_path(&k.root, &hit.root)) {
            continue;
        }
        hit.nickname = crate::workspace::recorded_name(&hit.root);
        known.push(hit);
    }
    Ok(())
}

fn add_directory(dir: &Path, known: &mut Vec<Known>) {
    let root = canonical(dir);
    if known.iter().any(|k| same_path(&k.root, &root)) {
        return;
    }
    let project = project_of(dir).ok();
    // A jj project's short directory parse is how `feature` used to select
    // `proj-feature`. The registration added above is the only name it has.
    if project.as_ref().is_some_and(|p| p.join(".jj").is_dir()) {
        return;
    }
    let name = project.as_ref().and_then(|p| git_ws_id(dir, p)).unwrap_or_default();
    let nickname = crate::workspace::recorded_name(dir);
    let named = !name.is_empty() || nickname.as_deref().is_some_and(|n| !crate::workspace::slugify(n).is_empty());
    if named {
        known.push(Known { name, root, nickname });
    }
}

fn git_ws_id(dir: &Path, project: &Path) -> Option<String> {
    let project_name = project.file_name()?.to_string_lossy();
    discover::ws_id_of(dir, &project_name)
}

fn projects_for(worktrees: &Path, cwd: &Path) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    if let Some(project) = jj_project(cwd) {
        remember(&mut seen, &mut out, project);
    }
    for dir in discover::list_dirs_in(worktrees) {
        if let Ok(project) = project_of(&dir) {
            remember(&mut seen, &mut out, project);
        }
    }
    out
}

fn remember(seen: &mut BTreeSet<PathBuf>, out: &mut Vec<PathBuf>, project: PathBuf) {
    if seen.insert(canonical(&project)) {
        out.push(project);
    }
}

fn classify(
    project: &Path,
    cwd: &Path,
    worktrees: &Path,
    names: &mut BTreeMap<PathBuf, String>,
    unmanaged: &mut Vec<Unmanaged>,
) -> Result<()> {
    let in_scope = canonical(project).starts_with(cwd);
    for reg in list_registered(project)? {
        if !reg.exact {
            continue;
        }
        if let Some(root) = managed_root(reg.root.as_deref(), worktrees) {
            names.insert(root, reg.name);
        } else if in_scope {
            unmanaged.push(Unmanaged {
                name: reg.name,
                project: canonical(project),
                detail: detail_of(reg.root.as_deref()),
            });
        }
    }
    Ok(())
}

fn managed_root(root: Option<&Path>, worktrees: &Path) -> Option<PathBuf> {
    let root = root.filter(|path| path.exists())?;
    let root = canonical(root);
    (root != worktrees && root.starts_with(worktrees)).then_some(root)
}

fn detail_of(root: Option<&Path>) -> String {
    root.map(|path| canonical(path).display().to_string()).unwrap_or_else(|| "no root".to_string())
}

/// The main repo that owns `start`. A secondary workspace's `.jj/repo` is a file
/// whose path is relative to the `.jj` directory; the store it names is
/// `<project>/.jj/repo`.
fn jj_project(start: &Path) -> Option<PathBuf> {
    for ancestor in start.ancestors() {
        let marker = ancestor.join(".jj").join("repo");
        if marker.is_dir() {
            return Some(canonical(ancestor));
        }
        if marker.is_file() {
            return project_from_repo_file(&marker);
        }
    }
    None
}

fn project_from_repo_file(marker: &Path) -> Option<PathBuf> {
    let pointer = std::fs::read_to_string(marker).ok()?;
    let base = marker.parent()?;
    let store = std::path::absolute(base.join(pointer.trim())).ok()?;
    let store = canonical(&store);
    store.parent()?.parent().map(Path::to_path_buf)
}

fn canonical(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn find<'a>(known: &'a [Known], asked: &Asked<'_>) -> Found<'a> {
    let hits: Vec<&Known> = known.iter().filter(|k| asked_matches(k, asked)).collect();
    match hits.len() {
        0 => Found::None,
        1 => Found::One(hits[0]),
        _ => Found::Many(hits),
    }
}

fn asked_matches(known: &Known, asked: &Asked<'_>) -> bool {
    match asked {
        Asked::Token(token) => crate::workspace::token_matches(token, &known.name, known.nickname.as_deref()),
        Asked::Path(path) => same_path(path, &known.root),
    }
}

/// Component equality, so a shared string prefix (`/wt` vs `/wt-evil`) is not a match.
fn same_path(left: &Path, right: &Path) -> bool {
    left.components().eq(right.components())
}

#[cfg(test)]
mod tests {
    use std::process::Command;

    use super::*;

    #[test]
    fn list_template_contains_the_two_character_nul_escape() {
        assert!(LIST_TEMPLATE.contains("name.escape_json()"), "{LIST_TEMPLATE:?}");
        assert!(LIST_TEMPLATE.contains("\\0"), "{LIST_TEMPLATE:?}");
        assert!(!LIST_TEMPLATE.contains('\0'), "{LIST_TEMPLATE:?}");
    }

    #[test]
    fn a_root_keeps_a_trailing_space_and_an_embedded_newline() {
        let spaced = "/work/trail ";
        let newline = "/work/foo\nbar";
        let text = format!(
            "{}\0{}\0{}\0{}\0",
            serde_json::to_string("kept").unwrap(),
            serde_json::to_string(spaced).unwrap(),
            serde_json::to_string("broken").unwrap(),
            serde_json::to_string(newline).unwrap(),
        );
        // `run_jj_utf8` trims the whole buffer. A NUL after the last field keeps
        // that trim from eating a trailing space; the space is inside the JSON.
        assert_eq!(text.trim(), text);
        let parsed = parse_list(text.trim());
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0].root.as_deref().and_then(|p| p.to_str()), Some(spaced));
        assert_eq!(parsed[1].root.as_deref().and_then(|p| p.to_str()), Some(newline));
    }

    #[test]
    fn a_split_record_does_not_yield_a_partial_root() {
        // A raw newline inside the root field, as `lines()` would see it.
        let text = "plain\0\"/tmp/foo\nbar\"\0";
        let parsed = parse_list(text);
        let roots: Vec<&str> = parsed.iter().filter_map(|reg| reg.root.as_deref().and_then(|p| p.to_str())).collect();
        assert!(!roots.contains(&"/tmp/foo"), "partial root: {roots:?}");
        assert!(!roots.contains(&"\"/tmp/foo"), "partial root: {roots:?}");
    }

    #[test]
    fn a_name_with_a_space_or_at_comes_back_unquoted() {
        for name in ["my workspace", "feat@v2"] {
            let text = format!(
                "{}\0{}\0",
                serde_json::to_string(name).unwrap(),
                serde_json::to_string("/work/place").unwrap(),
            );
            let parsed = parse_list(&text);
            assert_eq!(parsed.len(), 1, "{name}");
            assert_eq!(parsed[0].name, name, "{name}");
            assert!(parsed[0].exact, "{name}");
        }
        // `\u0040` decodes to `@` and re-encodes as `@`, so the name is not exact.
        let escaped = format!("\"feat\\u0040v2\"\0{}\0", serde_json::to_string("/work/place").unwrap());
        let parsed = parse_list(&escaped);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "feat@v2");
        assert!(!parsed[0].exact);
    }

    #[test]
    fn a_root_that_does_not_round_trip_is_not_exact() {
        let text = format!("{}\0\"\\u002fwork\\u002ffoo\"\0", serde_json::to_string("plain").unwrap());
        let parsed = parse_list(&text);
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].root.as_deref().and_then(|p| p.to_str()), Some("/work/foo"));
        assert!(!parsed[0].exact);
    }

    #[test]
    fn an_exact_root_resolves_and_an_inexact_one_refuses() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = std::fs::canonicalize(tmp.path()).unwrap().join("ws");
        std::fs::create_dir(&dir).unwrap();
        let exact = Registered { name: "kept".into(), root: Some(dir.clone()), exact: true };
        assert_eq!(name_from(vec![exact], &dir).unwrap().as_deref(), Some("kept"));
        let inexact = Registered { name: "bad".into(), root: Some(dir.clone()), exact: false };
        let err = name_from(vec![inexact], &dir).unwrap_err().to_string();
        assert_eq!(err, format!("refusing to delete {}: workspace root does not round-trip", dir.display()));
    }

    fn matched_name<'a>(found: Found<'a>) -> std::result::Result<&'a str, proptest::test_runner::TestCaseError> {
        match found {
            Found::One(hit) => Ok(hit.name.as_str()),
            Found::None => Err(proptest::test_runner::TestCaseError::fail("expected one match, found none")),
            Found::Many(_) => Err(proptest::test_runner::TestCaseError::fail("expected one match, found many")),
        }
    }

    proptest::proptest! {
        #[test]
        fn path_selects_the_registered_name_not_the_directory(
            dir_name in "[a-z]{1,8}",
            registered in "[a-z]{1,8}",
        ) {
            let root = PathBuf::from("/worktrees").join(&dir_name);
            let known = [Known { name: registered.clone(), root: root.clone(), nickname: None }];
            proptest::prop_assert_eq!(matched_name(find(&known, &Asked::Path(&root)))?, registered.as_str());
            let by_dir = find(&known, &Asked::Token(&dir_name));
            if dir_name == registered {
                proptest::prop_assert_eq!(matched_name(by_dir)?, registered.as_str());
            } else {
                proptest::prop_assert!(matches!(by_dir, Found::None));
            }
        }

        /// Directory `proj-feature` registered as `proj-feature`: the stripped
        /// suffix is not a name, and the path still selects the registration.
        #[test]
        fn a_prefixed_directory_does_not_match_its_stripped_suffix(
            project in "[a-z]{1,6}",
            short in "[a-z]{1,6}",
        ) {
            proptest::prop_assume!(project != short);
            let registered = format!("{project}-{short}");
            let root = PathBuf::from("/worktrees").join(&registered);
            let known = [Known { name: registered.clone(), root: root.clone(), nickname: None }];
            proptest::prop_assert!(matches!(find(&known, &Asked::Token(&short)), Found::None));
            proptest::prop_assert_eq!(matched_name(find(&known, &Asked::Path(&root)))?, registered.as_str());
        }

        #[test]
        fn a_list_line_round_trips_including_an_empty_root(
            name in "[a-z][a-z0-9-]{0,12}",
            root in "[a-z]{0,12}",
        ) {
            let name_json = serde_json::to_string(&name).unwrap();
            let (text, expected) = if root.is_empty() {
                (format!("{name_json}\0null\0"), None)
            } else {
                let path = format!("/{root}");
                (format!("{name_json}\0{}\0", serde_json::to_string(&path).unwrap()), Some(PathBuf::from(path)))
            };
            let parsed = parse_list(&text);
            proptest::prop_assert_eq!(parsed.len(), 1);
            proptest::prop_assert!(parsed[0].exact);
            proptest::prop_assert_eq!(&parsed[0].name, &name);
            proptest::prop_assert_eq!(&parsed[0].root, &expected);
            // A field that is not JSON is skipped, and does not swallow the next record.
            let noisy = format!("not a record\0not-json\0{text}");
            proptest::prop_assert_eq!(parse_list(&noisy).len(), 1);
            proptest::prop_assert_eq!(&parse_list(&noisy)[0].name, &name);
        }

        /// The bug: a trailing space or a newline in the root must come back as
        /// that exact path, and the record must still be exact enough to delete.
        #[test]
        fn a_root_with_space_or_newline_round_trips(
            name in "[a-z][a-z0-9-]{0,8}",
            body in "[a-z]{0,8}",
            spaces in 0usize..3,
            newline in proptest::bool::ANY,
        ) {
            let mut root = format!("/{body}{}", " ".repeat(spaces));
            if newline {
                root.push('\n');
                root.push_str("more");
            }
            let text = format!(
                "{}\0{}\0",
                serde_json::to_string(&name).unwrap(),
                serde_json::to_string(&root).unwrap()
            );
            let parsed = parse_list(&text);
            proptest::prop_assert_eq!(parsed.len(), 1);
            proptest::prop_assert!(parsed[0].exact);
            proptest::prop_assert_eq!(parsed[0].root.as_deref().and_then(|p| p.to_str()), Some(root.as_str()));
        }

        #[test]
        fn a_nickname_selects_the_registered_name(registered in "[a-z]{1,6}", nick in "[a-z]{1,6}") {
            proptest::prop_assume!(nick != registered);
            let known = [Known {
                name: registered.clone(),
                root: PathBuf::from("/worktrees/dir"),
                nickname: Some(nick.clone()),
            }];
            proptest::prop_assert_eq!(matched_name(find(&known, &Asked::Token(&nick)))?, registered.as_str());
        }

        /// A name jj would quote is still the name, once the field is JSON.
        #[test]
        fn a_json_name_round_trips(name in "[a-z @\"\\\\]{1,12}", root in "[a-z]{1,8}") {
            let path = format!("/{root}");
            let text = format!(
                "{}\0{}\0",
                serde_json::to_string(&name).unwrap(),
                serde_json::to_string(&path).unwrap(),
            );
            let parsed = parse_list(&text);
            proptest::prop_assert_eq!(parsed.len(), 1);
            proptest::prop_assert!(parsed[0].exact);
            proptest::prop_assert_eq!(&parsed[0].name, &name);
            proptest::prop_assert_eq!(parsed[0].root.as_deref().and_then(|p| p.to_str()), Some(path.as_str()));
        }

        /// `default` is the one name every repo already has. It is never reported,
        /// and dropping it leaves every other name.
        #[test]
        fn only_the_workspace_named_default_is_left_unreported(
            others in proptest::collection::vec("[a-z][a-z0-9-]{0,12}", 0..6),
        ) {
            let mut names = others;
            names.retain(|name| name != "default");
            names.push("default".to_string());
            let kept: Vec<&str> = names.iter().filter(|name| reports_unmanaged(name)).map(String::as_str).collect();
            proptest::prop_assert!(!kept.contains(&"default"));
            proptest::prop_assert_eq!(kept.len(), names.len() - 1);
        }
    }

    fn jj_fixture_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/jj")
    }

    /// Real `jj workspace list` stdout for one captured version. The bytes are
    /// untrimmed. `cases.txt` is what that jj was asked to register: a name, a
    /// tab, and the absolute root, or a tab and nothing when the directory was
    /// already gone.
    fn check_workspace_list(version: &str) {
        let dir = jj_fixture_root().join(version);
        let raw = std::fs::read(dir.join("workspace-list.bin")).unwrap_or_else(|err| {
            panic!("jj {version}: missing tests/fixtures/jj/{version}/workspace-list.bin ({err})")
        });
        let text = String::from_utf8(raw).unwrap_or_else(|_| panic!("jj {version}: list output was not utf-8"));
        let parsed = parse_list(&text);
        let cases = std::fs::read_to_string(dir.join("cases.txt"))
            .unwrap_or_else(|err| panic!("jj {version}: missing cases.txt ({err})"));
        let mut expected = Vec::new();
        for line in cases.lines() {
            let (name, root) = line.split_once('\t').unwrap_or_else(|| panic!("jj {version}: {line:?}"));
            expected.push((name.to_string(), (!root.is_empty()).then(|| root.to_string())));
        }
        let mut got: Vec<(String, Option<String>)> = parsed
            .iter()
            .map(|reg| {
                assert!(reg.exact, "jj {version}: {} did not round-trip", reg.name);
                (reg.name.clone(), reg.root.as_deref().and_then(|path| path.to_str()).map(str::to_string))
            })
            .collect();
        got.sort();
        expected.sort();
        assert_eq!(got, expected, "jj {version}");
        assert!(
            expected
                .iter()
                .any(|(name, root)| name.contains(' ') && root.as_deref().is_some_and(|p| Path::new(p).is_absolute())),
            "jj {version}: a name with a space and an absolute root"
        );
        assert!(expected.iter().any(|(name, root)| name == "ghost" && root.is_none()), "jj {version}: a missing root");
        assert!(
            expected
                .iter()
                .any(|(name, root)| name == "absroot" && root.as_deref().is_some_and(|p| Path::new(p).is_absolute())),
            "jj {version}: an absolute root"
        );
        assert!(
            expected
                .iter()
                .any(|(name, root)| name == "plain" && root.as_deref().is_some_and(|p| Path::new(p).is_absolute())),
            "jj {version}: an ordinary name"
        );
    }

    #[test]
    fn jj_0_45_1() {
        check_workspace_list("0.45.1");
    }

    /// Rewrites `tests/fixtures/jj/<version>/` from the jj on PATH.
    /// `scripts/capture-jj-workspace-list.sh` runs this. It does not install jj.
    #[test]
    #[ignore = "rewrites tests/fixtures/jj/<version> from the jj on PATH"]
    fn capture_jj_workspace_list() {
        let version = jj_version();
        let tmp = tempfile::tempdir().expect("tempdir");
        let parent = std::fs::canonicalize(tmp.path()).expect("canonical temp");
        let repo = parent.join("repo");
        std::fs::create_dir(&repo).expect("repo");
        let config = parent.join("jj-config.toml");
        std::fs::write(
            &config,
            "[user]\nname = \"Test User\"\nemail = \"test@example.com\"\n[ui]\ncolor = \"never\"\npaginate = \"never\"\n",
        )
        .expect("jj config");
        git(&repo, &["init", "--initial-branch=main"]);
        git(&repo, &["config", "user.email", "test@example.com"]);
        git(&repo, &["config", "user.name", "Test User"]);
        std::fs::write(repo.join("README"), "hi\n").expect("readme");
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "init"]);
        jj(&repo, &config, &["git", "init", "--colocate"]);

        let plain = parent.join("plain");
        let spaced = parent.join("spaced");
        let absolute = parent.join("absroot");
        let ghost = parent.join("ghost");
        jj(&repo, &config, &["workspace", "add", plain.to_str().unwrap(), "--name", "plain", "-r", "@"]);
        jj(&repo, &config, &["workspace", "add", spaced.to_str().unwrap(), "--name", "my workspace", "-r", "@"]);
        jj(&repo, &config, &["workspace", "add", absolute.to_str().unwrap(), "--name", "absroot", "-r", "@"]);
        jj(&repo, &config, &["workspace", "add", ghost.to_str().unwrap(), "--name", "ghost", "-r", "@"]);
        std::fs::remove_dir_all(&ghost).expect("remove ghost workspace dir");

        let output = Command::new("jj")
            .args(["--ignore-working-copy", "--color=never", "workspace", "list", "-T", LIST_TEMPLATE])
            .current_dir(&repo)
            .env("JJ_CONFIG", &config)
            .output()
            .expect("jj workspace list");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let dir = jj_fixture_root().join(&version);
        std::fs::create_dir_all(&dir).expect("fixture dir");
        std::fs::write(dir.join("workspace-list.bin"), &output.stdout).expect("write list");
        let cases = format!(
            "default\t{}\nplain\t{}\nmy workspace\t{}\nabsroot\t{}\nghost\t\n",
            repo.display(),
            plain.display(),
            spaced.display(),
            absolute.display(),
        );
        std::fs::write(dir.join("cases.txt"), cases).expect("write cases");
    }

    fn jj_version() -> String {
        let output = Command::new("jj").arg("--version").output().expect("jj --version");
        assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
        let text = String::from_utf8(output.stdout).expect("version is utf-8");
        let version = text.trim().strip_prefix("jj ").unwrap_or_else(|| panic!("jj --version: {text:?}"));
        assert!(!version.is_empty(), "{text:?}");
        version.to_string()
    }

    fn git(dir: &Path, args: &[&str]) {
        let output = Command::new("git").args(args).current_dir(dir).output().expect("git");
        assert!(output.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }

    fn jj(dir: &Path, config: &Path, args: &[&str]) {
        let output = Command::new("jj").args(args).current_dir(dir).env("JJ_CONFIG", config).output().expect("jj");
        assert!(output.status.success(), "jj {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }

    /// A fixture directory with no test is a version we parse and never check.
    #[test]
    fn every_jj_workspace_list_fixture_has_a_test() {
        const TESTED: &[&str] = &["0.45.1"];
        let mut captured: Vec<String> = std::fs::read_dir(jj_fixture_root())
            .expect("tests/fixtures/jj")
            .map(|entry| entry.expect("entry").file_name().to_string_lossy().into_owned())
            .filter(|name| !name.starts_with('.'))
            .collect();
        captured.sort();
        assert_eq!(captured, TESTED, "add a jj_<version> test for each directory under tests/fixtures/jj");
    }
}
