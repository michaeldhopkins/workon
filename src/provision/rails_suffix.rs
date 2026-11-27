//! The Rails database-name suffix: a script embedded in the binary, written to
//! workon's data directory, and loaded with `RUBYOPT=-r`. Nothing in here is
//! per workspace. The workspace id is exported beside the script, and the script
//! reads it from the environment.

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use vcs_runner::Cmd;

use super::{DbEngine, Resource};

pub(crate) const SCRIPT_NAME: &str = "rails_db_suffix.rb";
pub(crate) const SUFFIX_VAR: &str = "WORKON_RAILS_DB_SUFFIX";

/// Set to `server` in every workspace created from this version on, and stored in
/// `.workon.json` beside the suffix. It makes the hook name a per-checkout name
/// that passes the limit the way Postgres cuts it. A workspace without it keeps
/// the tagged names its databases already have.
pub(crate) const NAMES_VAR: &str = "WORKON_RAILS_DB_NAMES";
pub(crate) const NAMES_SERVER: &str = "server";

const SCRIPT: &str = include_str!("rails_db_suffix.rb");

/// `rails runner` prints one row per development and test database. Boot noise
/// is anything that does not start with this marker.
pub(crate) const LIST_DATABASES: &str = r#"rows = WorkonRailsDbSuffix.owned_rows(ActiveRecord::Base.configurations); rows.each { |env, adapter, database| $stdout.puts ["WORKON_DB", env, adapter, database].join("\t") }; $stdout.puts ["WORKON_DB_SERVER", rows.size].join("\t")"#;

/// Postgres identifiers are 63 bytes. Eight of those are reserved for a hash
/// tag so two long names cannot collapse into one suffix.
const SUFFIX_LIMIT: usize = 55;

/// `WORKON_DATA_DIR`, otherwise `$XDG_DATA_HOME/workon`, otherwise
/// `~/.local/share/workon`. The last avoids `~/Library`, whose space would
/// break `RUBYOPT`'s whitespace splitting.
pub(crate) fn data_dir() -> Result<PathBuf> {
    if let Some(dir) = env_nonempty("WORKON_DATA_DIR") {
        return Ok(PathBuf::from(dir));
    }
    if let Some(xdg) = env_nonempty("XDG_DATA_HOME") {
        return Ok(PathBuf::from(xdg).join("workon"));
    }
    Ok(crate::home::home_dir()?.join(".local/share/workon"))
}

/// Write the embedded script under `dir` and return its path. A byte-identical
/// file is left untouched. A stale file is replaced. A path inside a git or jj
/// repository is refused unless git's ignore rules cover it (see
/// [`refuse_repository`]). The first check runs before any directory is created.
/// Directories this call created are removed again when it refuses.
pub(crate) fn install_at(dir: &Path) -> Result<PathBuf> {
    install_checking(dir, refuse_repository)
}

/// [`install_at`] with the repository check supplied, so a test can make the
/// check after creation disagree with the check before it.
fn install_checking(dir: &Path, check: impl Fn(&Path) -> Result<()>) -> Result<PathBuf> {
    let dir = absolute_dir(dir)?;
    let tmp_name = format!(".{SCRIPT_NAME}.{}.tmp", std::process::id());
    for name in [SCRIPT_NAME, tmp_name.as_str()] {
        check(&dir.join(name))?;
    }
    let created = missing_dirs(&dir);
    let dir = match create_and_check(&dir, &tmp_name, &check) {
        Ok(canonical) => canonical,
        Err(err) => {
            remove_created(&created);
            return Err(err);
        }
    };
    let path = dir.join(SCRIPT_NAME);
    if path.is_file() && std::fs::read(&path).ok().as_deref() == Some(SCRIPT.as_bytes()) {
        return Ok(path);
    }
    let tmp = dir.join(&tmp_name);
    if let Err(err) = std::fs::write(&tmp, SCRIPT).and_then(|()| std::fs::rename(&tmp, &path)) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("replacing {}", path.display()));
    }
    Ok(path)
}

/// `RUBYOPT=-r` is resolved against the later working directory, so a relative
/// data dir has to become an absolute path before it is exported.
fn absolute_dir(dir: &Path) -> Result<PathBuf> {
    if dir.is_absolute() {
        Ok(dir.to_path_buf())
    } else {
        Ok(std::env::current_dir()?.join(dir))
    }
}

/// Create `dir`, resolve it, and check the resolved script and temp paths again:
/// the answer for a directory that now exists can differ from the one for a path
/// that did not.
fn create_and_check(dir: &Path, tmp_name: &str, check: &impl Fn(&Path) -> Result<()>) -> Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let canonical = dir.canonicalize().with_context(|| format!("resolving {}", dir.display()))?;
    for name in [SCRIPT_NAME, tmp_name] {
        check(&canonical.join(name))?;
    }
    Ok(canonical)
}

/// The directories `create_dir_all(dir)` would create, deepest first. Empty
/// when `dir` has a `..`, whose creations cannot be told apart lexically.
fn missing_dirs(dir: &Path) -> Vec<PathBuf> {
    if dir.components().any(|part| part == Component::ParentDir) {
        return Vec::new();
    }
    dir.ancestors().take_while(|ancestor| !ancestor.exists()).map(Path::to_path_buf).collect()
}

/// `remove_dir` only removes an empty directory, so nothing that was already
/// there, or that anything has since been written into, is lost.
fn remove_created(created: &[PathBuf]) {
    for dir in created {
        if std::fs::remove_dir(dir).is_err() {
            break;
        }
    }
}

/// Install into the data directory, guarded like any other path: a data
/// directory that resolves into a repository (a stow-style link into a
/// dotfiles checkout, say) is refused unless that repository ignores it.
pub(crate) fn install() -> Result<PathBuf> {
    install_at(&data_dir()?)
}

/// `_` plus the workspace id, with every other character turned into `_`.
pub(crate) fn db_suffix(ws_id: &str) -> String {
    let mut suffix = String::with_capacity(ws_id.len() + 1);
    suffix.push('_');
    for ch in ws_id.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            suffix.push(ch);
        } else {
            suffix.push('_');
        }
    }
    if suffix.len() <= SUFFIX_LIMIT {
        return suffix;
    }
    let tag = hex8(&Sha256::digest(suffix.as_bytes()));
    let keep = SUFFIX_LIMIT - tag.len();
    let mut out = String::new();
    for ch in suffix.chars() {
        if out.len() + ch.len_utf8() > keep {
            break;
        }
        out.push(ch);
    }
    out.push_str(&tag);
    out
}

fn hex8(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(8);
    for byte in bytes.iter().take(4) {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0xf) as usize] as char);
    }
    out
}

/// `RUBYOPT` that loads `script` once. An existing value keeps its other flags.
/// `RUBYOPT` splits on whitespace, so a path that contains whitespace is refused.
#[cfg(test)]
pub(crate) fn rubyopt_loading(existing: Option<&str>, script: &Path) -> Result<String> {
    match existing {
        Some(value) => compose_rubyopt([value], script),
        None => compose_rubyopt(std::iter::empty(), script),
    }
}

/// One `-r` of `script`, then every other flag from `sources`, in order.
/// An older `-r` of `rails_db_suffix.rb` is dropped. A path with whitespace
/// cannot be loaded: `RUBYOPT` splits on whitespace.
pub(crate) fn compose_rubyopt<'a>(sources: impl IntoIterator<Item = &'a str>, script: &Path) -> Result<String> {
    let path = script.to_str().context("rails db suffix path is not Unicode")?;
    if path.chars().any(char::is_whitespace) {
        bail!("rails db suffix path contains whitespace, so RUBYOPT cannot load it: {path}");
    }
    let flag = format!("-r{path}");
    let mut tokens = Vec::new();
    for source in sources {
        for token in source.split_whitespace() {
            if token == flag || hook_flag(token) || tokens.iter().any(|kept: &String| kept == token) {
                continue;
            }
            tokens.push(token.to_string());
        }
    }
    tokens.insert(0, flag);
    Ok(tokens.join(" "))
}

fn hook_flag(token: &str) -> bool {
    let Some(path) = token.strip_prefix("-r") else {
        return false;
    };
    Path::new(path).file_name().and_then(|name| name.to_str()) == Some(SCRIPT_NAME)
}

/// `rubyopt` loads `rails_db_suffix.rb`.
pub(crate) fn loads_hook(rubyopt: &str) -> bool {
    rubyopt.split_whitespace().any(hook_flag)
}

/// `rubyopt` with every `-r` of `rails_db_suffix.rb` removed.
pub(crate) fn without_hook(rubyopt: &str) -> String {
    rubyopt.split_whitespace().filter(|token| !hook_flag(token)).collect::<Vec<_>>().join(" ")
}

/// What a `rails runner` listing proved.
pub(crate) enum Listing {
    /// Every owned development and test server database ends with the suffix.
    Isolated(Vec<Resource>),
    /// The runner booted and reported no server database. SQLite only.
    NoServer,
    /// Prepare must not run. The string is the warning detail.
    Unproven(&'static str),
}

pub(crate) fn judge_listing(stdout: &str, suffix: &str) -> Listing {
    let listed = parse_db_listing(stdout);
    match server_count(stdout) {
        Some(0) if listed.is_empty() => return Listing::NoServer,
        Some(count) if count != listed.len() => {
            return Listing::Unproven("a server database was not reported as a database workon can isolate");
        }
        _ => {}
    }
    if listed.is_empty() || suffix.is_empty() {
        return Listing::Unproven("rails reported no development or test database");
    }
    let (isolated, all) = isolated_resources(listed, suffix);
    if all {
        Listing::Isolated(isolated)
    } else {
        Listing::Unproven("a database name did not end with the workspace suffix")
    }
}

fn server_count(stdout: &str) -> Option<usize> {
    stdout.lines().rev().find_map(|line| line.strip_prefix("WORKON_DB_SERVER\t")?.trim().parse().ok())
}

/// Rows from [`LIST_DATABASES`]. SQLite and unknown adapters are not server
/// databases workon can drop. A row needs the marker and exactly four fields.
pub(crate) fn parse_db_listing(stdout: &str) -> Vec<Resource> {
    let mut resources = Vec::new();
    for line in stdout.lines() {
        let mut fields = line.splitn(4, '\t');
        let Some("WORKON_DB") = fields.next() else { continue };
        let (Some(env), Some(adapter), Some(database)) = (fields.next(), fields.next(), fields.next()) else {
            continue;
        };
        if env != "development" && env != "test" || database.is_empty() {
            continue;
        }
        let Some(engine) = engine_for(adapter) else { continue };
        let resource = engine.resource(database);
        if !resources.contains(&resource) {
            resources.push(resource);
        }
    }
    resources
}

/// Bytes of the suffix that name the workspace (`_ws_` and six hex digits). The
/// hook cuts a long per-checkout name like Postgres and the cut keeps them.
const IDENTITY_BYTES: usize = 10;

/// A workspace database ends with the suffix, or is a per-checkout name the hook
/// cut to the server's limit, which still carries the workspace's identity. A
/// suffix too short to hold an identity matches by its end alone.
fn is_workspace_name(name: &str, suffix: &str) -> bool {
    name.ends_with(suffix) || suffix.get(..IDENTITY_BYTES).is_some_and(|identity| name.contains(identity))
}

/// Names that do not end with `suffix` are the primary checkout's databases.
/// They are dropped from the list, and `false` says the hook missed a row.
pub(crate) fn isolated_resources(listed: Vec<Resource>, suffix: &str) -> (Vec<Resource>, bool) {
    if listed.is_empty() || suffix.is_empty() {
        return (Vec::new(), false);
    }
    let isolated: Vec<Resource> =
        listed.iter().filter(|resource| is_workspace_name(resource.db_name(), suffix)).cloned().collect();
    let all_isolated = isolated.len() == listed.len();
    (isolated, all_isolated)
}

fn engine_for(adapter: &str) -> Option<DbEngine> {
    match adapter {
        "postgresql" | "postgres" | "postgis" => Some(DbEngine::Postgres),
        "mysql2" | "mysql" | "trilogy" => Some(DbEngine::Mysql),
        _ => None,
    }
}

fn env_nonempty(var: &str) -> Option<String> {
    std::env::var(var).ok().filter(|value| !value.is_empty())
}

const GIT_ENV_VARS: [&str; 5] = ["GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_COMMON_DIR", "GIT_NAMESPACE"];

/// Refuse when `path`, or a directory it would be created under, is inside a
/// git or jj repository that git would not ignore it in. "Ignore" is whatever
/// `git check-ignore` reports: `.gitignore` files, `.git/info/exclude` and
/// `core.excludesFile`, and never for a path in the git index. Because only the
/// git index is checked for tracked files, a file that jj tracks in its working
/// copy while `.gitignore` matches it still counts as ignored. A jj repository
/// without a `.git` is asked through its backing git store. When git (or jj)
/// cannot be run, or says nothing, the path counts as not ignored. The first
/// existing ancestor is canonicalized, so a symlink into a repository is the
/// repository.
fn refuse_repository(path: &Path) -> Result<()> {
    refuse_repository_on(path, None)
}

/// [`refuse_repository`] with the `PATH` git is looked up on, for a test.
fn refuse_repository_on(path: &Path, search_path: Option<&str>) -> Result<()> {
    let Some(anchor) = first_existing(path) else {
        return Ok(());
    };
    let canon = anchor.canonicalize().unwrap_or_else(|_| anchor.clone());
    let mut cursor = Some(canon.as_path());
    while let Some(dir) = cursor {
        if (dir.join(".git").exists() || dir.join(".jj").exists())
            && !repo_relative(dir, &anchor, &canon, path).is_some_and(|rel| repo_ignores(dir, &rel, search_path))
        {
            bail!("refusing to write {} inside a repository ({})", path.display(), dir.display());
        }
        cursor = dir.parent();
    }
    Ok(())
}

/// The path of `path` inside `repo`, keeping the uncreated tail below `anchor`
/// after `anchor` is canonicalized to `canon_anchor`.
fn repo_relative(repo: &Path, anchor: &Path, canon_anchor: &Path, path: &Path) -> Option<PathBuf> {
    let tail = path.strip_prefix(anchor).ok()?;
    canon_anchor.join(tail).strip_prefix(repo).ok().map(Path::to_path_buf)
}

/// Whether git ignores `rel` in the repository at `dir`. Any failure is `false`.
fn repo_ignores(dir: &Path, rel: &Path, search_path: Option<&str>) -> bool {
    let Some((dir_str, rel_str)) = dir.to_str().zip(rel.to_str()) else {
        return false;
    };
    let git = || {
        let cmd = GIT_ENV_VARS.into_iter().fold(Cmd::new("git"), Cmd::env_remove);
        match search_path {
            Some(path) => cmd.env("PATH", path),
            None => cmd,
        }
    };
    if dir.join(".git").exists() {
        return git()
            .in_dir(dir)
            .env("GIT_CEILING_DIRECTORIES", dir_str)
            .args(["check-ignore", "-q", "--", rel_str])
            .run()
            .is_ok();
    }
    let Ok(git_dir) = vcs_runner::run_jj_utf8_ignore_wc(dir, &["-R", dir_str, "git", "root"]) else {
        return false;
    };
    git()
        .in_dir(dir)
        .args([
            "-c",
            "core.bare=false",
            "--git-dir",
            &git_dir,
            "--work-tree",
            dir_str,
            "check-ignore",
            "-q",
            "--",
            rel_str,
        ])
        .run()
        .is_ok()
}

fn first_existing(path: &Path) -> Option<PathBuf> {
    let mut cursor = path.to_path_buf();
    loop {
        if cursor.exists() {
            return Some(cursor);
        }
        if !cursor.pop() {
            return None;
        }
    }
}

#[cfg(test)]
pub(crate) static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
pub(crate) fn with_env_var<T>(var: &str, value: &Path, body: impl FnOnce() -> T) -> T {
    let _guard = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    let prior = std::env::var_os(var);
    // SAFETY: env mutation is serialized with `ENV_LOCK`, held across this call.
    unsafe { std::env::set_var(var, value) };
    let out = body();
    // SAFETY: still under `ENV_LOCK`, held by `_guard`.
    unsafe {
        match prior {
            Some(previous) => std::env::set_var(var, previous),
            None => std::env::remove_var(var),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use std::fs::FileTimes;
    use std::time::{Duration, SystemTime};

    use super::*;

    #[test]
    fn the_listing_asks_the_hook_which_databases_it_owns() {
        assert!(LIST_DATABASES.contains("WorkonRailsDbSuffix.owned_rows"));
        assert!(!LIST_DATABASES.contains("configs_for"));
    }

    fn write_script(dir: &Path, bytes: &[u8]) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(SCRIPT_NAME);
        std::fs::write(&path, bytes).unwrap();
        path
    }

    fn set_modified(path: &Path, when: SystemTime) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        file.set_times(FileTimes::new().set_modified(when)).unwrap();
    }

    #[test]
    fn the_installed_script_path_has_no_dotdot() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("sub").join("data");
        std::fs::create_dir_all(&dir).unwrap();
        let via = tmp.path().join("sub").join("other").join("..").join("data");

        let path = install_at(&via).unwrap();

        assert!(path.is_absolute());
        assert!(path.components().all(|component| component.as_os_str() != ".."));
        assert_eq!(path, dir.canonicalize().unwrap().join(SCRIPT_NAME));
    }

    #[test]
    fn an_identical_script_is_not_rewritten() {
        let tmp = tempfile::tempdir().unwrap();
        let path = install_at(tmp.path()).unwrap();
        let stamp = SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        set_modified(&path, stamp);

        let again = install_at(tmp.path()).unwrap();

        assert_eq!(again, path);
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), stamp);
        assert_eq!(std::fs::read(&path).unwrap(), SCRIPT.as_bytes());
    }

    #[test]
    fn a_stale_script_is_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let path = write_script(tmp.path(), b"# stale\n");

        install_at(tmp.path()).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), SCRIPT.as_bytes());
        assert!(!tmp.path().join(format!(".{SCRIPT_NAME}.{}.tmp", std::process::id())).exists());
    }

    #[test]
    fn the_script_is_not_written_inside_a_git_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let nested = repo.join("nested").join("deeper");

        let err = install_at(&nested).unwrap_err();

        assert!(err.to_string().contains("repository"), "{err}");
        assert!(!nested.exists(), "no directory is created inside the repository");
        assert!(!repo.join(SCRIPT_NAME).exists());
    }

    #[test]
    fn the_script_is_not_written_beside_a_git_file_or_jj_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let worktree = tmp.path().join("worktree");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(worktree.join(".git"), "gitdir: /elsewhere/.git\n").unwrap();
        assert!(install_at(&worktree).is_err());
        assert!(!worktree.join(SCRIPT_NAME).exists());

        let jj_repo = tmp.path().join("jj");
        std::fs::create_dir_all(jj_repo.join(".jj")).unwrap();
        assert!(install_at(&jj_repo).is_err());
        assert!(!jj_repo.join(SCRIPT_NAME).exists());
    }

    #[test]
    fn a_symlink_into_a_repository_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(repo.join(".git")).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&repo, &link).unwrap();

        assert!(install_at(&link).is_err());
        assert!(!repo.join(SCRIPT_NAME).exists());
        assert!(!link.join(SCRIPT_NAME).exists());
    }

    #[test]
    fn the_script_is_written_when_a_git_or_jj_repository_ignores_the_path() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("git-home");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        std::fs::write(repo.join(".gitignore"), "/*\n!/tracked/\n!/tracked/**\n").unwrap();

        let unignored = repo.join("tracked").join("workon");
        let err = install_at(&unignored).unwrap_err();
        assert!(err.to_string().contains("repository"), "{err}");
        assert!(!unignored.exists());

        let ignored = repo.join(".local/share/workon");
        let written = install_at(&ignored).unwrap();
        assert!(written.is_file());
        assert_eq!(std::fs::read(&written).unwrap(), SCRIPT.as_bytes());

        if !vcs_runner::jj_available() {
            return;
        }
        let jj_repo = tmp.path().join("jj-home");
        std::fs::create_dir_all(&jj_repo).unwrap();
        vcs_runner::run_jj(&jj_repo, &["git", "init", "--no-colocate"]).unwrap();
        std::fs::write(jj_repo.join(".gitignore"), "/*\n!/tracked/\n!/tracked/**\n").unwrap();

        let jj_unignored = jj_repo.join("tracked").join("workon");
        let err = install_at(&jj_unignored).unwrap_err();
        assert!(err.to_string().contains("repository"), "{err}");
        assert!(!jj_unignored.exists());

        let jj_ignored = jj_repo.join(".local/share/workon");
        let written = install_at(&jj_ignored).unwrap();
        assert!(written.is_file());
        assert_eq!(std::fs::read(&written).unwrap(), SCRIPT.as_bytes());
    }

    fn run_git(dir: &Path, args: &[&str]) {
        GIT_ENV_VARS.into_iter().fold(Cmd::new("git"), Cmd::env_remove).in_dir(dir).args(args).run().unwrap();
    }

    fn git_repo(root: &Path, gitignore: &str) -> PathBuf {
        let repo = root.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        run_git(&repo, &["init", "-q"]);
        std::fs::write(repo.join(".gitignore"), gitignore).unwrap();
        repo
    }

    /// `git check-ignore` reports nothing for a tracked path, so a file git
    /// tracks is never "ignored", even under an ignored directory: the
    /// stale-file replace must not overwrite it.
    #[test]
    fn a_tracked_file_inside_an_ignored_directory_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git_repo(tmp.path(), "/*\n");
        let dir = repo.join("data/workon");
        let tracked = write_script(&dir, b"# tracked\n");
        run_git(&repo, &["add", "-f", "--", "data/workon/rails_db_suffix.rb"]);

        let err = install_at(&dir).unwrap_err();

        assert!(err.to_string().contains("repository"), "{err}");
        assert_eq!(std::fs::read(&tracked).unwrap(), b"# tracked\n", "the tracked file is not replaced");
        install_at(&repo.join("data/untracked")).unwrap();
    }

    #[test]
    fn an_inherited_git_env_pointing_at_a_decoy_repository_is_ignored() {
        const CHILD_VAR: &str = "WORKON_TEST_ISOLATED_GIT_ENV";
        if std::env::var_os(CHILD_VAR).is_none() {
            let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
                .args([
                    "--exact",
                    "provision::rails_suffix::tests::an_inherited_git_env_pointing_at_a_decoy_repository_is_ignored",
                ])
                .env(CHILD_VAR, "1")
                .output()
                .expect("re-run the test binary");
            let stdout = String::from_utf8_lossy(&out.stdout);
            let stderr = String::from_utf8_lossy(&out.stderr);
            assert!(out.status.success(), "{stdout}{stderr}");
            assert!(stdout.contains("1 passed"), "the child test did not run: {stdout}");
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let decoy = tmp.path().join("decoy");
        std::fs::create_dir_all(&decoy).unwrap();
        run_git(&decoy, &["init", "-q"]);
        std::fs::write(decoy.join(".gitignore"), "/*\n").unwrap();
        std::fs::write(decoy.join(".git/info/exclude"), "/*\n").unwrap();

        let repo = git_repo(tmp.path(), "");
        let dir = repo.join("data/workon");

        for (var, val) in [
            ("GIT_DIR", decoy.join(".git")),
            ("GIT_WORK_TREE", decoy.clone()),
            ("GIT_COMMON_DIR", decoy.join(".git")),
            ("GIT_NAMESPACE", PathBuf::from("decoy")),
        ] {
            let err = with_env_var(var, &val, || install_at(&dir)).unwrap_err();
            assert!(err.to_string().contains("repository"), "{var}: {err}");
            assert!(!repo.join("data").exists(), "{var} must not let a directory be created");
        }

        let tracked_root = tmp.path().join("tracked");
        let tracked_repo = git_repo(&tracked_root, "/*\n");
        let tracked_dir = tracked_repo.join("data/workon");
        let tracked = write_script(&tracked_dir, b"# tracked\n");
        run_git(&tracked_repo, &["add", "-f", "--", "data/workon/rails_db_suffix.rb"]);
        let err = with_env_var("GIT_INDEX_FILE", &decoy.join(".git/index"), || install_at(&tracked_dir)).unwrap_err();
        assert!(err.to_string().contains("repository"), "{err}");
        assert_eq!(std::fs::read(&tracked).unwrap(), b"# tracked\n");

        if vcs_runner::jj_available() {
            let jj_repo = tmp.path().join("jj");
            std::fs::create_dir_all(&jj_repo).unwrap();
            vcs_runner::run_jj(&jj_repo, &["git", "init", "--no-colocate"]).unwrap();
            let jj_dir = jj_repo.join("data/workon");
            let err = with_env_var("GIT_COMMON_DIR", &decoy.join(".git"), || install_at(&jj_dir)).unwrap_err();
            assert!(err.to_string().contains("repository"), "{err}");
            assert!(!jj_repo.join("data").exists());
        }
    }

    #[test]
    fn the_temporary_file_must_also_be_ignored_by_the_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git_repo(tmp.path(), "rails_db_suffix.rb\n");
        let dir = repo.join("data/workon");
        let tmp_file = dir.join(format!(".{SCRIPT_NAME}.{}.tmp", std::process::id()));

        let err = install_at(&dir).unwrap_err();
        assert!(err.to_string().contains("repository"), "{err}");
        assert!(!repo.join("data").exists(), "no directory is created when the temporary file is not ignored");

        std::fs::create_dir_all(&dir).unwrap();
        let err = install_at(&dir).unwrap_err();
        assert!(err.to_string().contains("repository"), "{err}");
        assert!(!dir.join(SCRIPT_NAME).exists());
        assert!(!tmp_file.exists());

        std::fs::write(repo.join(".gitignore"), "rails_db_suffix.rb\n.rails_db_suffix.rb.*.tmp\n").unwrap();
        let written = install_at(&dir).unwrap();
        assert_eq!(std::fs::read(&written).unwrap(), SCRIPT.as_bytes());
        assert!(!tmp_file.exists());

        std::fs::write(&written, b"# stale\n").unwrap();
        std::fs::write(&tmp_file, b"# tracked tmp\n").unwrap();
        let rel_tmp = format!("data/workon/.{SCRIPT_NAME}.{}.tmp", std::process::id());
        run_git(&repo, &["add", "-f", "--", &rel_tmp]);
        let err = install_at(&dir).unwrap_err();
        assert!(err.to_string().contains("repository"), "{err}");
        assert_eq!(std::fs::read(&tmp_file).unwrap(), b"# tracked tmp\n");
        assert_eq!(std::fs::read(&written).unwrap(), b"# stale\n");
    }

    #[test]
    fn a_missing_git_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git_repo(tmp.path(), "/*\n");
        let dir = repo.join("data/workon");
        let script = dir.join(SCRIPT_NAME);
        refuse_repository_on(&script, None).expect("the repository ignores the path while git runs");
        let no_programs = tmp.path().join("no-programs");
        std::fs::create_dir_all(&no_programs).unwrap();
        let stand_in = no_programs.to_str().unwrap();

        let err = refuse_repository_on(&script, Some(stand_in)).unwrap_err();
        assert!(err.to_string().contains("refusing to write"), "{err}");

        let err = install_checking(&dir, |path| refuse_repository_on(path, Some(stand_in))).unwrap_err();
        assert!(err.to_string().contains("refusing to write"), "{err}");
        assert!(!repo.join("data").exists(), "nothing is created when git cannot answer");
    }

    #[test]
    fn a_colocated_jj_repository_is_judged_by_its_git_ignore_rules() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("colocated");
        std::fs::create_dir_all(&repo).unwrap();
        vcs_runner::run_jj(&repo, &["git", "init", "--colocate"]).unwrap();
        assert!(repo.join(".git").exists() && repo.join(".jj").exists());
        std::fs::write(repo.join(".gitignore"), "/*\n!/tracked/\n!/tracked/**\n").unwrap();

        let unignored = repo.join("tracked").join("workon");
        let err = install_at(&unignored).unwrap_err();
        assert!(err.to_string().contains("repository"), "{err}");
        assert!(!unignored.exists());

        let written = install_at(&repo.join(".local/share/workon")).unwrap();
        assert_eq!(std::fs::read(&written).unwrap(), SCRIPT.as_bytes());
    }

    #[test]
    fn an_ignore_everything_home_with_reincludes_covers_a_default_data_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git_repo(tmp.path(), "*.log\n/*\n!/.gitignore\n!/.config/\n!/bin/\n");
        std::fs::create_dir_all(repo.join(".local/share")).unwrap();
        let dir = repo.join(".local/share/workon");

        let written = install_at(&dir).unwrap();
        assert_eq!(std::fs::read(&written).unwrap(), SCRIPT.as_bytes());
    }

    #[test]
    fn an_ignored_parent_directory_covers_a_data_dir_below_it() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git_repo(tmp.path(), ".local/\n");
        std::fs::create_dir_all(repo.join(".local/share")).unwrap();
        let dir = repo.join(".local/share/workon");

        let written = install_at(&dir).unwrap();
        assert_eq!(std::fs::read(&written).unwrap(), SCRIPT.as_bytes());
    }

    #[test]
    fn a_jj_repository_with_git_backing_and_no_dot_git_covers_a_default_data_dir() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("jj-home");
        std::fs::create_dir_all(&repo).unwrap();
        vcs_runner::run_jj(&repo, &["git", "init", "--no-colocate"]).unwrap();
        assert!(!repo.join(".git").exists() && repo.join(".jj").exists());
        std::fs::write(repo.join(".gitignore"), "/*\n!/.gitignore\n").unwrap();
        std::fs::create_dir_all(repo.join(".local/share")).unwrap();
        let dir = repo.join(".local/share/workon");

        let written = install_at(&dir).unwrap();
        assert_eq!(std::fs::read(&written).unwrap(), SCRIPT.as_bytes());
    }

    /// `git check-ignore` also reads `.git/info/exclude` and `core.excludesFile`,
    /// so a path they cover is ignored with no `.gitignore` entry.
    #[test]
    fn the_exclude_files_count_as_ignoring() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git_repo(tmp.path(), "");
        let dir = repo.join(".local/share/workon");
        assert!(install_at(&dir).is_err());
        assert!(!dir.exists());

        std::fs::write(repo.join(".git/info/exclude"), "/.local/\n").unwrap();

        let written = install_at(&dir).unwrap();
        assert_eq!(std::fs::read(&written).unwrap(), SCRIPT.as_bytes());

        let other = tempfile::tempdir().unwrap();
        let repo = git_repo(other.path(), "");
        let excludes = other.path().join("global-excludes");
        std::fs::write(&excludes, "/.local/\n").unwrap();
        let dir = repo.join(".local/share/workon");
        assert!(install_at(&dir).is_err());
        run_git(&repo, &["config", "core.excludesFile", excludes.to_str().unwrap()]);

        let written = install_at(&dir).unwrap();
        assert_eq!(std::fs::read(&written).unwrap(), SCRIPT.as_bytes());
    }

    /// When the check after creation disagrees with the one before it, the
    /// directories this call made are removed and the ones already there stay.
    #[test]
    fn a_refusal_after_creation_removes_the_directories_it_made() {
        let tmp = tempfile::tempdir().unwrap();
        let existing = tmp.path().join("existing");
        std::fs::create_dir_all(existing.join("kept")).unwrap();
        let dir = existing.join("kept").join("new").join("deeper");
        let calls = std::cell::Cell::new(0);
        let refuses_after_creation = |_: &Path| {
            calls.set(calls.get() + 1);
            if !dir.exists() {
                Ok(())
            } else {
                bail!("refusing to write: the repository does not ignore it")
            }
        };

        let err = install_checking(&dir, refuses_after_creation).unwrap_err();

        assert!(err.to_string().contains("refusing to write"), "{err}");
        assert_eq!(calls.get(), 3);
        assert!(existing.join("kept").is_dir(), "a directory that was already there stays");
        assert!(!existing.join("kept/new").exists(), "no stray directory is left behind");
    }

    #[test]
    fn a_refusal_does_not_remove_a_directory_that_holds_something() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("new").join("deeper");
        let refuses_after_a_file_appears = |_: &Path| {
            if dir.exists() {
                std::fs::write(tmp.path().join("new").join("note"), "x").unwrap();
                bail!("refusing to write")
            }
            Ok(())
        };

        assert!(install_checking(&dir, refuses_after_a_file_appears).is_err());

        assert!(tmp.path().join("new/note").is_file(), "a directory with a file in it is kept");
        assert!(!dir.exists(), "the empty one below it is still removed");
    }

    #[test]
    fn the_directories_to_create_are_found_without_following_dotdot() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        std::fs::create_dir_all(&base).unwrap();

        assert_eq!(
            missing_dirs(&base.join("a").join("b")),
            vec![base.join("a").join("b"), base.join("a")],
            "deepest first, stopping at the first directory that exists"
        );
        assert!(missing_dirs(&base).is_empty());
        assert!(missing_dirs(&base.join("a").join("..").join("b")).is_empty(), "a `..` path is left alone");
    }

    #[test]
    fn data_dir_prefers_workon_then_xdg_then_home() {
        let tmp = tempfile::tempdir().unwrap();
        let workon = tmp.path().join("explicit");
        let xdg = tmp.path().join("xdg");
        let home = tmp.path().join("home");
        assert_eq!(with_env_var("WORKON_DATA_DIR", &workon, || data_dir().unwrap()), workon);

        let _guard = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let prior_workon = std::env::var_os("WORKON_DATA_DIR");
        let prior_xdg = std::env::var_os("XDG_DATA_HOME");
        let prior_home = std::env::var_os("HOME");
        // SAFETY: env mutation is serialized with `ENV_LOCK`, held by `_guard`.
        unsafe {
            std::env::remove_var("WORKON_DATA_DIR");
            std::env::set_var("XDG_DATA_HOME", &xdg);
        }
        assert_eq!(data_dir().unwrap(), xdg.join("workon"));
        // SAFETY: still under `ENV_LOCK`. An empty WORKON_DATA_DIR does not count.
        unsafe { std::env::set_var("WORKON_DATA_DIR", "") };
        assert_eq!(data_dir().unwrap(), xdg.join("workon"));
        // SAFETY: still under `ENV_LOCK`.
        unsafe {
            std::env::remove_var("WORKON_DATA_DIR");
            std::env::remove_var("XDG_DATA_HOME");
            std::env::set_var("XDG_DATA_HOME", "");
            std::env::set_var("HOME", &home);
        }
        assert_eq!(data_dir().unwrap(), home.join(".local/share/workon"));
        // SAFETY: still under `ENV_LOCK`, restoring the process environment.
        unsafe {
            restore("WORKON_DATA_DIR", prior_workon);
            restore("XDG_DATA_HOME", prior_xdg);
            restore("HOME", prior_home);
        }
    }

    /// Restore one variable saved before this test changed the environment.
    ///
    /// # Safety
    ///
    /// The caller holds [`ENV_LOCK`] for the whole update.
    unsafe fn restore(var: &str, prior: Option<std::ffi::OsString>) {
        match prior {
            // SAFETY: the caller holds `ENV_LOCK` for this update.
            Some(value) => unsafe { std::env::set_var(var, value) },
            // SAFETY: the caller holds `ENV_LOCK` for this update.
            None => unsafe { std::env::remove_var(var) },
        }
    }

    #[test]
    fn an_empty_listing_is_not_a_workspace_database() {
        let (isolated, all) = isolated_resources(Vec::new(), "_ws_1");
        assert!(isolated.is_empty());
        assert!(!all, "no rows is not proof the hook suffixed every database");
    }

    #[test]
    fn a_name_without_the_suffix_is_not_a_workspace_database() {
        let listed = vec![
            Resource::PostgresDb { name: "app_development_ws_1".into() },
            Resource::PostgresDb { name: "app_development".into() },
        ];
        let (isolated, all) = isolated_resources(listed, "_ws_1");
        assert!(!all);
        assert_eq!(isolated, vec![Resource::PostgresDb { name: "app_development_ws_1".into() }]);
    }

    /// The reported failure: the app's name ends with the suffix and passes 63
    /// bytes, so the hook returns the server's cut, which no longer ends with it.
    /// That cut is still this workspace's database, and the primary's is not.
    #[test]
    fn a_name_cut_to_the_server_limit_is_still_a_workspace_database() {
        let suffix = "_ws_a4af64_quarterly_reports_summary";
        let cut = "mylittleapp_development_mylittleapp_ws_a4af64_quarterly_reports";
        assert_eq!(cut.len(), 63);
        let listed = vec![
            Resource::PostgresDb { name: cut.into() },
            Resource::PostgresDb { name: "mylittleapp_development_mylittleapp".into() },
        ];
        let (isolated, all) = isolated_resources(listed, suffix);
        assert!(!all);
        assert_eq!(isolated, vec![Resource::PostgresDb { name: cut.into() }]);
        let other =
            Resource::PostgresDb { name: "mylittleapp_development_mylittleapp_ws_b1c2d3_quarterly_reports".into() };
        assert!(isolated_resources(vec![other], suffix).0.is_empty(), "another workspace's cut is not this one's");
    }

    #[test]
    fn rubyopt_refuses_a_path_that_contains_whitespace() {
        let err = rubyopt_loading(None, Path::new("/tmp/work on/rails_db_suffix.rb")).unwrap_err();
        assert!(err.to_string().contains("whitespace"), "{err}");
    }

    proptest::proptest! {
        /// The suffix is `_` plus the workspace id, and every character that is
        /// not an identifier character has become `_`.
        #[test]
        fn the_suffix_is_an_identifier_and_keeps_the_workspace_id(
            ws_id in "[A-Za-z0-9_ !@-]{0,24}",
            extra in "[^A-Za-z0-9_]{0,3}",
        ) {
            let suffix = db_suffix(&ws_id);
            let kept: String = std::iter::once('_')
                .chain(ws_id.chars().map(|ch| if ch.is_ascii_alphanumeric() || ch == '_' { ch } else { '_' }))
                .collect();
            proptest::prop_assert_eq!(suffix.clone(), kept);
            proptest::prop_assert!(suffix.chars().all(|ch| ch == '_' || ch.is_ascii_alphanumeric()));
            if !extra.is_empty() {
                let with_extra = db_suffix(&format!("{ws_id}{extra}"));
                proptest::prop_assert_eq!(with_extra, format!("{suffix}{}", "_".repeat(extra.chars().count())));
            }
        }

        /// Loading the script once, then again, leaves a single `-r` flag and
        /// keeps every other flag.
        #[test]
        fn rubyopt_loads_the_script_once(
            existing in "[-A-Za-z0-9_./]{0,24}( [-A-Za-z0-9_./]{1,12}){0,3}",
            already in proptest::bool::ANY,
        ) {
            let script = Path::new("/tmp/workon/rails_db_suffix.rb");
            let flag = "-r/tmp/workon/rails_db_suffix.rb";
            let prior = if already { format!("{flag} {existing}") } else { existing.clone() };
            let once = rubyopt_loading(Some(&prior), script).unwrap();
            let twice = rubyopt_loading(Some(&once), script).unwrap();
            proptest::prop_assert_eq!(&once, &twice);
            proptest::prop_assert_eq!(once.split_whitespace().filter(|token| *token == flag).count(), 1);
            if !already {
                proptest::prop_assert!(once.starts_with(flag));
            }
            for token in existing.split_whitespace().filter(|token| *token != flag) {
                proptest::prop_assert!(once.split_whitespace().any(|kept| kept == token));
            }
        }

        /// A workspace id that does not fit still yields one identifier, and two
        /// ids that share a long prefix stay different.
        #[test]
        fn a_long_workspace_id_stays_short_and_distinct(body in "[a-z]{60}") {
            let mut other = body.clone();
            let flipped = if other.ends_with('a') { 'b' } else { 'a' };
            other.pop();
            other.push(flipped);
            let left = db_suffix(&body);
            let right = db_suffix(&other);
            proptest::prop_assert_ne!(&left, &right);
            for suffix in [&left, &right] {
                proptest::prop_assert!(suffix.len() <= 55);
                proptest::prop_assert!(suffix.chars().all(|ch| ch == '_' || ch.is_ascii_alphanumeric()));
            }
        }

        /// Flags from every source are kept once. An old copy of this script is not.
        #[test]
        fn rubyopt_merges_every_source_and_drops_an_old_hook(
            user in "-[A-Za-z0-9]{1,6}",
            mise in "-[A-Za-z0-9]{1,6}",
        ) {
            let script = Path::new("/tmp/workon/rails_db_suffix.rb");
            let old = "-r/elsewhere/rails_db_suffix.rb";
            let merged = compose_rubyopt([user.as_str(), &format!("{old} {mise}")], script).unwrap();
            proptest::prop_assert!(merged.starts_with("-r/tmp/workon/rails_db_suffix.rb"));
            proptest::prop_assert!(merged.split_whitespace().any(|token| token == user));
            proptest::prop_assert!(merged.split_whitespace().any(|token| token == mise));
            proptest::prop_assert!(!merged.contains("/elsewhere/"));
            proptest::prop_assert_eq!(merged.split_whitespace().filter(|token| token.contains("rails_db_suffix.rb")).count(), 1);
        }

        /// No rows, or rows that miss the suffix, are not a reason to prepare.
        #[test]
        fn an_empty_listing_is_not_isolated(
            suffix in "_[a-z0-9]{1,8}",
            name in "[a-z]{1,12}",
        ) {
            proptest::prop_assert!(matches!(judge_listing("", &suffix), Listing::Unproven(_)));
            proptest::prop_assert!(matches!(judge_listing("WORKON_DB_SERVER\t0\n", &suffix), Listing::NoServer));
            let row = format!("WORKON_DB\tdevelopment\tpostgresql\t{name}\nWORKON_DB_SERVER\t1\n");
            let decision = judge_listing(&row, &suffix);
            if name.ends_with(&suffix) {
                proptest::prop_assert!(matches!(decision, Listing::Isolated(_)));
            } else {
                proptest::prop_assert!(matches!(decision, Listing::Unproven(_)));
            }
        }

        /// Only a marked development or test row for a server adapter becomes a
        /// database to drop. Noise, SQLite, and other environments do not.
        #[test]
        fn a_listing_keeps_only_marked_development_and_test_server_rows(
            noise in "[^\n\t]{0,30}",
            env in "(development|test|production)",
            adapter in "(postgresql|postgres|mysql2|mysql|trilogy|postgis|sqlite3|other)",
            database in "[a-z0-9_]{1,20}",
        ) {
            let text = format!("{noise}\nWORKON_DB\t{env}\t{adapter}\t{database}\n{noise}\n");
            let got = parse_db_listing(&text);
            let server = matches!(
                adapter.as_str(),
                "postgresql" | "postgres" | "mysql2" | "mysql" | "trilogy" | "postgis"
            );
            let wanted = server && (env == "development" || env == "test");
            if wanted {
                proptest::prop_assert_eq!(got.len(), 1);
                proptest::prop_assert_eq!(got[0].db_name(), database);
            } else {
                proptest::prop_assert!(got.is_empty());
            }
        }

        /// Canonicalizing the existing anchor keeps both the existing relative
        /// prefix inside `repo` and the uncreated tail below `anchor`.
        #[test]
        fn the_path_inside_a_repository_keeps_the_uncreated_tail(
            existing in proptest::collection::vec("[a-z]{1,8}", 0..3),
            uncreated in proptest::collection::vec("[a-z]{1,8}", 1..4),
        ) {
            let raw_repo = PathBuf::from("/var/tmp/home");
            let canon_repo = PathBuf::from("/private/var/tmp/home");
            let anchor = existing.iter().fold(raw_repo.clone(), |acc, part| acc.join(part));
            let canon_anchor = existing.iter().fold(canon_repo.clone(), |acc, part| acc.join(part));
            let path = uncreated.iter().fold(anchor.clone(), |acc, part| acc.join(part));
            let expected = existing.iter().chain(&uncreated).fold(PathBuf::new(), |acc, part| acc.join(part));

            proptest::prop_assert_eq!(
                repo_relative(&canon_repo, &anchor, &canon_anchor, &path),
                Some(expected)
            );
            proptest::prop_assert_eq!(
                repo_relative(Path::new("/elsewhere"), &anchor, &canon_anchor, &path),
                None
            );
        }
    }
}
