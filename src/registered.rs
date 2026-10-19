//! A workspace's identity is the name jj registered for the workspace whose root
//! is that directory. Parsing `<project>-<ws_id>` off the directory name is wrong
//! the moment jj's name and the directory disagree (workon issue #5), so a project
//! that has `.jj` never does it. A git-only worktree has no jj registration; the
//! directory spelling is the only name it has, and that fallback stays.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use vcs_runner::run_jj_utf8_ignore_wc;

use crate::discover;

/// `jj` reads `\0` and `\n` as escapes. A real NUL in this string would split the
/// argument before jj's template parser saw it.
const LIST_TEMPLATE: &str = "name ++ \"\\0\" ++ if(root, root, \"\") ++ \"\\n\"";

struct Registered {
    name: String,
    root: Option<PathBuf>,
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

fn name_at_root(project_dir: &Path, ws_dir: &Path) -> Result<Option<String>> {
    let wanted = canonical(ws_dir);
    let known: Vec<Known> = list_registered(project_dir)?.into_iter().filter_map(known_at).collect();
    Ok(match find(&known, &Asked::Path(&wanted)) {
        Found::One(hit) => Some(hit.name.clone()),
        Found::None | Found::Many(_) => None,
    })
}

fn known_at(reg: Registered) -> Option<Known> {
    Some(Known { name: reg.name, root: canonical(&reg.root?), nickname: None })
}

fn list_registered(project: &Path) -> Result<Vec<Registered>> {
    if !project.join(".jj").is_dir() {
        return Ok(Vec::new());
    }
    let output = run_jj_utf8_ignore_wc(project, &["--color=never", "workspace", "list", "-T", LIST_TEMPLATE])
        .with_context(|| format!("jj workspace list in {}", project.display()))?;
    Ok(parse_list(&output))
}

fn parse_list(output: &str) -> Vec<Registered> {
    output
        .lines()
        .filter_map(|line| {
            let (name, root) = line.split_once('\0')?;
            if name.is_empty() {
                return None;
            }
            let root = (!root.is_empty()).then(|| PathBuf::from(root));
            Some(Registered { name: name.to_string(), root })
        })
        .collect()
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
        let Some(root) = reg.root.as_deref().filter(|path| path.exists()) else {
            continue;
        };
        let root = canonical(root);
        if known.iter().any(|k| same_path(&k.root, &root)) {
            continue;
        }
        known.push(Known { name: reg.name, root: root.clone(), nickname: crate::workspace::recorded_name(&root) });
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
    use super::*;

    #[test]
    fn list_template_contains_the_two_character_nul_escape() {
        assert!(LIST_TEMPLATE.contains("\\0"), "{LIST_TEMPLATE:?}");
        assert!(!LIST_TEMPLATE.contains('\0'), "{LIST_TEMPLATE:?}");
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
            let (text, expected) = if root.is_empty() {
                (format!("{name}\0\n"), None)
            } else {
                (format!("{name}\0/{root}\n"), Some(PathBuf::from(format!("/{root}"))))
            };
            let parsed = parse_list(&text);
            proptest::prop_assert_eq!(parsed.len(), 1);
            proptest::prop_assert_eq!(&parsed[0].name, &name);
            proptest::prop_assert_eq!(&parsed[0].root, &expected);
            let noisy = format!("not a record\n{text}");
            proptest::prop_assert_eq!(parse_list(&noisy).len(), 1);
            proptest::prop_assert_eq!(&parse_list(&noisy)[0].name, &name);
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
}
