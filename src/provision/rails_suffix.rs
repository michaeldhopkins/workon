//! The Rails database-name suffix: a script embedded in the binary, written to
//! workon's data directory, and loaded with `RUBYOPT=-r`. Nothing in here is
//! per workspace. The workspace id is exported beside the script, and the script
//! reads it from the environment.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use super::{DbEngine, Resource};

pub(crate) const SCRIPT_NAME: &str = "rails_db_suffix.rb";
pub(crate) const SUFFIX_VAR: &str = "WORKON_RAILS_DB_SUFFIX";

const SCRIPT: &str = include_str!("rails_db_suffix.rb");

/// `rails runner` prints one row per development and test database. Boot noise
/// is anything that does not start with this marker.
pub(crate) const LIST_DATABASES: &str = r#"%w[development test].each do |env| ActiveRecord::Base.configurations.configs_for(env_name: env).each do |config| adapter = config.configuration_hash[:adapter] || config.configuration_hash["adapter"]; database = config.database; next if adapter.nil? || database.nil? || database.to_s.empty?; $stdout.puts ["WORKON_DB", env, adapter, database].join("\t"); end; end"#;

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
    let path = dir.join(SCRIPT_NAME);
    refuse_repository(&path)?;
    if path.is_file() && std::fs::read(&path).ok().as_deref() == Some(SCRIPT.as_bytes()) {
        return Ok(path);
    }
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{SCRIPT_NAME}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, SCRIPT)?;
    if let Err(err) = std::fs::rename(&tmp, &path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(err).with_context(|| format!("replacing {}", path.display()));
    }
    Ok(path)
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
    suffix
}

/// `RUBYOPT` that loads `script` once. An existing value keeps its other flags.
/// `RUBYOPT` splits on whitespace, so a path that contains whitespace is refused.
pub(crate) fn rubyopt_loading(existing: Option<&str>, script: &Path) -> Result<String> {
    let path = script.to_str().context("rails db suffix path is not Unicode")?;
    if path.chars().any(char::is_whitespace) {
        bail!("rails db suffix path contains whitespace, so RUBYOPT cannot load it: {path}");
    }
    let flag = format!("-r{path}");
    let prior = existing.unwrap_or("").trim();
    if prior.split_whitespace().any(|token| token == flag) {
        return Ok(prior.to_string());
    }
    if prior.is_empty() {
        Ok(flag)
    } else {
        Ok(format!("{flag} {prior}"))
    }
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
    let isolated: Vec<Resource> =
        listed.iter().filter(|resource| resource.db_name().ends_with(suffix)).cloned().collect();
    let all_isolated = isolated.len() == listed.len();
    (isolated, all_isolated)
}

fn engine_for(adapter: &str) -> Option<DbEngine> {
    match adapter {
        "postgresql" | "postgres" => Some(DbEngine::Postgres),
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

        /// Only a marked development or test row for a server adapter becomes a
        /// database to drop. Noise, SQLite, and other environments do not.
        #[test]
        fn a_listing_keeps_only_marked_development_and_test_server_rows(
            noise in "[^\n\t]{0,30}",
            env in "(development|test|production)",
            adapter in "(postgresql|postgres|mysql2|mysql|trilogy|sqlite3|other)",
            database in "[a-z0-9_]{1,20}",
        ) {
            let text = format!("{noise}\nWORKON_DB\t{env}\t{adapter}\t{database}\n{noise}\n");
            let got = parse_db_listing(&text);
            let server = matches!(adapter.as_str(), "postgresql" | "postgres" | "mysql2" | "mysql" | "trilogy");
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
