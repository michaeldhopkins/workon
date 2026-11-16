//! The Rails database-name suffix: a script embedded in the binary, written to
//! workon's data directory, and loaded with `RUBYOPT=-r`. Nothing in here is
//! per workspace. The workspace id is exported beside the script, and the script
//! reads it from the environment.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};

use super::{DbEngine, Resource};

pub(crate) const SCRIPT_NAME: &str = "rails_db_suffix.rb";
pub(crate) const SUFFIX_VAR: &str = "WORKON_RAILS_DB_SUFFIX";

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
/// repository is refused before any directory is created.
pub(crate) fn install_at(dir: &Path) -> Result<PathBuf> {
    let dir = absolute_dir(dir)?;
    let path = dir.join(SCRIPT_NAME);
    refuse_repository(&path)?;
    std::fs::create_dir_all(&dir)?;
    let dir = dir.canonicalize().with_context(|| format!("resolving {}", dir.display()))?;
    let path = dir.join(SCRIPT_NAME);
    refuse_repository(&path)?;
    if path.is_file() && std::fs::read(&path).ok().as_deref() == Some(SCRIPT.as_bytes()) {
        return Ok(path);
    }
    let tmp = dir.join(format!(".{SCRIPT_NAME}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, SCRIPT)?;
    if let Err(err) = std::fs::rename(&tmp, &path) {
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

/// Names that do not end with `suffix` are the primary checkout's databases.
/// They are dropped from the list, and `false` says the hook missed a row.
pub(crate) fn isolated_resources(listed: Vec<Resource>, suffix: &str) -> (Vec<Resource>, bool) {
    if listed.is_empty() || suffix.is_empty() {
        return (Vec::new(), false);
    }
    let isolated: Vec<Resource> =
        listed.iter().filter(|resource| resource.db_name().ends_with(suffix)).cloned().collect();
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

/// Refuse when `path`, or a directory it would be created under, is inside a
/// repository. The first existing ancestor is canonicalized, so a symlink into
/// a repository is the repository.
fn refuse_repository(path: &Path) -> Result<()> {
    let Some(anchor) = first_existing(path) else {
        return Ok(());
    };
    let canon = anchor.canonicalize().unwrap_or(anchor);
    let mut cursor = Some(canon.as_path());
    while let Some(dir) = cursor {
        if dir.join(".git").exists() || dir.join(".jj").exists() {
            bail!("refusing to write {} inside a repository ({})", path.display(), dir.display());
        }
        cursor = dir.parent();
    }
    Ok(())
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
    }
}
