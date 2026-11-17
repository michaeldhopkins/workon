//! Rails provisioner: isolated development and test databases per workspace.
//!
//! `config/database.yml` keeps the primary checkout's names. The workspace
//! exports `WORKON_RAILS_DB_SUFFIX` and a `RUBYOPT` that loads a script from
//! workon's data directory. The script suffixes development and test names,
//! including local replicas, and holds no per-workspace data. `DATABASE_URL`
//! is not exported. A server URL already in the environment is part of the
//! listing. A server URL in a dotenv file Rails loads is part of that decision
//! too. `db:prepare` for the file omits parent and mise variables, and when a
//! server URL is set it also runs with that URL. Prepare runs only after `rails runner`
//! reports that every owned server database ends with the suffix. No file is
//! written into the workspace.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use vcs_runner::Cmd;

use super::rails_dotenv::{self, DotenvFault};
use super::rails_suffix::{self, Listing, LIST_DATABASES, SUFFIX_VAR};
use super::rails_yml;
use super::{run_step, ProvisionCtx, Provisioner, Resource, Setup};

pub struct Rails;

impl Provisioner for Rails {
    fn name(&self) -> &'static str {
        "rails"
    }

    fn detect(&self, ws_dir: &Path) -> bool {
        ws_dir.join("config/database.yml").is_file() && rails_app(ws_dir)
    }

    fn setup(&self, ctx: &ProvisionCtx<'_>) -> Result<Setup> {
        let yml = std::fs::read_to_string(ctx.ws_dir.join("config/database.yml")).unwrap_or_default();
        let mut urls = session_database_urls(ctx.mise_vars);
        match rails_dotenv::database_urls(ctx.ws_dir, &dotenv_env(ctx.mise_vars)) {
            Ok(from_files) => urls.extend(from_files),
            Err(fault) => return Ok(unproven_dotenv(ctx, &fault)),
        }
        if !rails_yml::needs_isolation(&yml, urls.iter().map(|(_, value)| value.as_str())) {
            return Ok(Setup::default());
        }

        let suffix = rails_suffix::db_suffix(ctx.ws_id);
        let rubyopt = match hook_rubyopt(ctx, &suffix) {
            Hook::Ready(value) => value,
            Hook::Missing(setup) => return Ok(setup),
        };

        let mut failed_steps: Vec<String> = bundle_install(ctx).into_iter().collect();
        // Names first. `db:prepare` creates whatever the hook resolved, so a listing
        // that does not prove the suffix must not run against the primary checkout.
        let mut resources = Vec::new();
        if failed_steps.is_empty() {
            match prove_isolated(ctx, &suffix, &rubyopt, &yml, &urls) {
                Listed::Ready(proven) => {
                    resources = proven.resources;
                    failed_steps.extend(prepare_databases(ctx, &suffix, &rubyopt, &proven.modes));
                }
                Listed::NoServer => return Ok(Setup::default()),
                Listed::Failed(reason) => {
                    eprintln!("Warning: list rails databases failed: {reason}");
                    failed_steps.push("list rails databases".to_string());
                }
            }
        }

        Ok(Setup {
            resources,
            session_env: vec![(SUFFIX_VAR.to_string(), suffix), ("RUBYOPT".to_string(), rubyopt)],
            failed_steps,
            ..Setup::default()
        })
    }
}

enum Hook {
    Ready(String),
    Missing(Setup),
}

struct Proven {
    resources: Vec<Resource>,
    modes: Vec<PrepareMode>,
}

enum Listed {
    Ready(Proven),
    NoServer,
    Failed(String),
}

enum PrepareMode {
    /// The file's databases, with `DATABASE_URL` removed so test is prepared.
    File,
    /// The database a server URL in the environment makes the app open.
    SessionUrl,
}

fn hook_rubyopt(ctx: &ProvisionCtx<'_>, suffix: &str) -> Hook {
    let script = match rails_suffix::install() {
        Ok(path) => path,
        Err(err) => {
            eprintln!("Warning: could not install the Rails database suffix: {err}");
            return Hook::Missing(uninstalled(suffix.to_string()));
        }
    };
    let process_rubyopt = std::env::var("RUBYOPT").ok();
    match rails_suffix::compose_rubyopt(
        [ctx.mise_vars.get("RUBYOPT").map(String::as_str), process_rubyopt.as_deref()].into_iter().flatten(),
        &script,
    ) {
        Ok(value) => Hook::Ready(value),
        Err(err) => {
            eprintln!("Warning: could not install the Rails database suffix: {err}");
            Hook::Missing(uninstalled(suffix.to_string()))
        }
    }
}

/// The listing keeps environment URLs, because that is the database the app
/// opens. When the file itself has a server database, a second listing without
/// those URLs proves the names `db:prepare` will create for the file.
fn prove_isolated(ctx: &ProvisionCtx<'_>, suffix: &str, rubyopt: &str, yml: &str, urls: &[(String, String)]) -> Listed {
    let kept = match list_databases(ctx, suffix, rubyopt, false) {
        Ok(stdout) => stdout,
        Err(err) => return Listed::Failed(err.to_string()),
    };
    let mut resources = match decide(&kept, suffix) {
        Decide::Isolated(resources) => resources,
        // A development listing does not load `.env.test`. A server URL that
        // lives only there still has to keep the hook. Preparing would run
        // before that URL is proven, so record nothing and create nothing.
        Decide::NoServer if has_server_url(urls) => {
            return Listed::Ready(Proven { resources: Vec::new(), modes: Vec::new() });
        }
        Decide::NoServer => return Listed::NoServer,
        Decide::Failed(reason) => return Listed::Failed(reason),
    };
    let mut modes = Vec::new();
    if rails_yml::needs_server_database(yml) && has_server_url(urls) {
        match file_listing(ctx, suffix, rubyopt, &mut resources) {
            FileList::Prepare => modes.push(PrepareMode::File),
            FileList::Nothing => {}
            FileList::Failed(reason) => return Listed::Failed(reason),
        }
    } else if rails_yml::needs_server_database(yml) {
        modes.push(PrepareMode::File);
    }
    if has_server_url(urls) {
        modes.push(PrepareMode::SessionUrl);
    }
    Listed::Ready(Proven { resources, modes })
}

enum Decide {
    Isolated(Vec<Resource>),
    NoServer,
    Failed(String),
}

fn decide(stdout: &str, suffix: &str) -> Decide {
    match rails_suffix::judge_listing(stdout, suffix) {
        Listing::Isolated(resources) => Decide::Isolated(resources),
        Listing::NoServer => Decide::NoServer,
        Listing::Unproven(reason) => Decide::Failed(reason.to_string()),
    }
}

enum FileList {
    Prepare,
    Nothing,
    Failed(String),
}

fn file_listing(ctx: &ProvisionCtx<'_>, suffix: &str, rubyopt: &str, resources: &mut Vec<Resource>) -> FileList {
    let stdout = match list_databases(ctx, suffix, rubyopt, true) {
        Ok(stdout) => stdout,
        Err(err) => return FileList::Failed(err.to_string()),
    };
    match decide(&stdout, suffix) {
        Decide::Isolated(file_rows) => {
            for resource in file_rows {
                if !resources.contains(&resource) {
                    resources.push(resource);
                }
            }
            FileList::Prepare
        }
        Decide::NoServer => FileList::Nothing,
        Decide::Failed(reason) => FileList::Failed(reason),
    }
}

fn prepare_databases(ctx: &ProvisionCtx<'_>, suffix: &str, rubyopt: &str, modes: &[PrepareMode]) -> Vec<String> {
    let mut failed = Vec::new();
    let mut announced = false;
    for mode in modes {
        if !announced {
            eprintln!("Preparing this workspace's Rails development and test databases...");
            announced = true;
        }
        let strip_urls = matches!(mode, PrepareMode::File);
        failed.extend(run_step(prepare_cmd(ctx, suffix, rubyopt, strip_urls), "rails db:prepare"));
        if !failed.is_empty() {
            break;
        }
        if schema_format(ctx.ws_dir) == Some("sql") {
            failed.extend(run_step(structure_load_cmd(ctx, suffix, rubyopt, strip_urls), "rails db:schema:load"));
        }
        if !failed.is_empty() {
            break;
        }
    }
    failed
}

fn has_server_url(urls: &[(String, String)]) -> bool {
    urls.iter().any(|(_, url)| rails_yml::server_database_url(url))
}

/// Process environment, with mise `[env]` overlaid. Only database URL keys.
fn session_database_urls(mise: &HashMap<String, String>) -> Vec<(String, String)> {
    let mut urls: HashMap<String, String> = std::env::vars().filter(|(key, _)| database_url_var(key)).collect();
    for (key, value) in mise {
        if database_url_var(key) {
            urls.insert(key.clone(), value.clone());
        }
    }
    let mut urls: Vec<_> = urls.into_iter().collect();
    urls.sort();
    urls
}

/// The environment dotenv-rails sees: the process, with mise overlaid. A key
/// already set here wins over every dotenv file.
fn dotenv_env(mise: &HashMap<String, String>) -> HashMap<String, String> {
    let mut env: HashMap<String, String> = std::env::vars().collect();
    for (key, value) in mise {
        env.insert(key.clone(), value.clone());
    }
    env
}

/// The dotenv file could not be read or parsed. Install the hook and stop
/// before prepare: an unreadable file is not proof that Rails stays on sqlite.
fn unproven_dotenv(ctx: &ProvisionCtx<'_>, fault: &DotenvFault) -> Setup {
    eprintln!(
        "Warning: Rails dotenv file {} could not be read or parsed, so isolation is not proven ({})",
        fault.path.display(),
        fault.reason()
    );
    let suffix = rails_suffix::db_suffix(ctx.ws_id);
    match hook_rubyopt(ctx, &suffix) {
        Hook::Ready(rubyopt) => Setup {
            session_env: vec![(SUFFIX_VAR.to_string(), suffix), ("RUBYOPT".to_string(), rubyopt)],
            failed_steps: vec!["read rails dotenv".to_string()],
            ..Setup::default()
        },
        Hook::Missing(mut setup) => {
            setup.failed_steps.insert(0, "read rails dotenv".to_string());
            setup
        }
    }
}

/// The hook is not loaded. Record the suffix anyway so a later attach or exec
/// refreshes the script and refuses to start if it still cannot.
fn uninstalled(suffix: String) -> Setup {
    Setup {
        failed_steps: vec!["install rails db suffix".to_string()],
        session_env: vec![(SUFFIX_VAR.to_string(), suffix)],
        ..Setup::default()
    }
}

fn rails_app(ws_dir: &Path) -> bool {
    ws_dir.join("bin/rails").is_file()
        || ws_dir.join("config/application.rb").is_file()
        || std::fs::read_to_string(ws_dir.join("Gemfile")).is_ok_and(|text| rails_yml::gemfile_depends_on_rails(&text))
}

fn prepare_cmd(ctx: &ProvisionCtx<'_>, suffix: &str, rubyopt: &str, strip_urls: bool) -> Cmd {
    rails_cmd(ctx, suffix, rubyopt, strip_urls).args(["exec", "rails", "db:prepare"]).env("RAILS_ENV", "development")
}

/// `db:prepare` does not read `SCHEMA_FORMAT`. With `DATABASE_URL` unset this
/// load covers development and the test database together.
fn structure_load_cmd(ctx: &ProvisionCtx<'_>, suffix: &str, rubyopt: &str, strip_urls: bool) -> Cmd {
    rails_cmd(ctx, suffix, rubyopt, strip_urls)
        .args(["exec", "rails", "db:schema:load"])
        .env("RAILS_ENV", "development")
        .env("SCHEMA_FORMAT", "sql")
}

fn list_databases(ctx: &ProvisionCtx<'_>, suffix: &str, rubyopt: &str, strip_urls: bool) -> Result<String> {
    let out = rails_cmd(ctx, suffix, rubyopt, strip_urls)
        .args(["exec", "rails", "runner", LIST_DATABASES])
        .env("RAILS_ENV", "development")
        .run()
        .map_err(|err| anyhow::anyhow!(err))?;
    Ok(out.stdout_lossy().into_owned())
}

fn rails_cmd(ctx: &ProvisionCtx<'_>, suffix: &str, rubyopt: &str, strip_urls: bool) -> Cmd {
    let mut cmd = Cmd::new("bundle").in_dir(ctx.ws_dir);
    for (key, value) in ctx.mise_vars {
        if !(strip_urls && database_url_var(key)) {
            cmd = cmd.env(key, value);
        }
    }
    if strip_urls {
        for (key, _) in std::env::vars() {
            if database_url_var(&key) {
                cmd = cmd.env_remove(key);
            }
        }
    }
    cmd.env(SUFFIX_VAR, suffix).env("RUBYOPT", rubyopt)
}

fn database_url_var(key: &str) -> bool {
    rails_yml::is_database_url_key(key)
}

/// `bundle check || bundle install` in the workspace when it has a Gemfile. Git-sourced
/// gems are checked out into a per-checkout bundler path the gitignored-file clone does
/// not cover (issue #4), so Rails would fail with "not yet checked out" until this runs.
/// `None` means the gems are ready (or there is no Gemfile); a failed install is reported
/// like every other step, naming its command.
fn bundle_install(ctx: &ProvisionCtx<'_>) -> Option<String> {
    if !ctx.ws_dir.join("Gemfile").is_file() {
        return None;
    }
    let bundle = |args: &[&str]| {
        let mut cmd = Cmd::new("bundle").args(args).in_dir(ctx.ws_dir);
        for (key, value) in ctx.mise_vars {
            cmd = cmd.env(key, value);
        }
        cmd
    };
    if bundle(&["check"]).run().is_ok() {
        return None;
    }
    run_step(bundle(&["install"]), "bundle install")
}

/// `sql` when the app keeps `db/structure.sql` without `db/schema.rb`, else `None`
/// (Rails' own default is `ruby`, `db/schema.rb`). Rails 7+ removed `db:structure:load`
/// in favour of `db:schema:load` reading `SCHEMA_FORMAT=sql`. `db:prepare` does not
/// read that variable, so a structure-only app is loaded with a following
/// `db:schema:load`.
pub(crate) fn schema_format(ws_dir: &Path) -> Option<&'static str> {
    (ws_dir.join("db/structure.sql").is_file() && !ws_dir.join("db/schema.rb").is_file()).then_some("sql")
}

/// The server adapter in `database.yml`, or the first adapter when there is none.
#[cfg(test)]
pub(crate) fn adapter(database_yml: &str) -> Option<String> {
    rails_yml::first_adapter(database_yml)
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::*;
    use crate::provision::Resource;

    fn ctx<'a>(dir: &'a Path, project: &'a str, ws_id: &'a str, mise: &'a HashMap<String, String>) -> ProvisionCtx<'a> {
        ProvisionCtx { project_dir: dir, project_name: project, ws_id, ws_dir: dir, mise_vars: mise }
    }

    fn with_data_dir<T>(dir: &Path, body: impl FnOnce() -> T) -> T {
        rails_suffix::with_env_var("WORKON_DATA_DIR", dir, || {
            let saved: Vec<(String, String)> = std::env::vars().filter(|(key, _)| database_url_var(key)).collect();
            for (key, _) in &saved {
                // SAFETY: `with_env_var` holds `ENV_LOCK` across this closure.
                unsafe { std::env::remove_var(key) };
            }
            let out = body();
            for (key, value) in &saved {
                // SAFETY: still under `ENV_LOCK`.
                unsafe { std::env::set_var(key, value) };
            }
            out
        })
    }

    /// The hook is loaded the way a workspace loads it (`RUBYOPT=-r` the script)
    /// and must suffix the databases ActiveRecord actually builds. No connection.
    #[test]
    fn the_hook_suffixes_real_activerecord_configurations() {
        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rails"));
        let script = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/src/provision/rails_db_suffix.rb"));
        let tmp = tempfile::tempdir().unwrap();
        let proof = tmp.path().join("proof.rb");
        std::fs::write(&proof, include_str!("rails_db_suffix_proof.rb")).unwrap();
        let mut cmd = if bundle_ready(fixture) {
            let mut cmd = Command::new("bundle");
            cmd.arg("exec").arg("ruby").current_dir(fixture);
            cmd
        } else if std::env::var("GITHUB_JOB").ok().as_deref() == Some("provisioners") {
            panic!("the provisioners job has no Rails fixture bundle, so the ActiveRecord hook was not proven");
        } else {
            Command::new(ruby_with_activerecord().unwrap_or_else(|| PathBuf::from("ruby")))
        };
        // A parent GEM_HOME would hide the Ruby that actually has ActiveRecord.
        let out = cmd
            .arg(&proof)
            .env("RUBYOPT", format!("-r{}", script.display()))
            .env("WORKON_RAILS_DB_SUFFIX", "_ws_test1")
            .env_remove("DATABASE_URL")
            .env_remove("GEM_HOME")
            .env_remove("GEM_PATH")
            .env_remove("BUNDLE_GEMFILE")
            .env_remove("RUBYLIB")
            .output()
            .expect("ruby");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        if stdout.trim().starts_with("NO_AR") {
            assert_ne!(
                std::env::var("GITHUB_JOB").ok().as_deref(),
                Some("provisioners"),
                "the provisioners job could not require active_record: {stderr}"
            );
            eprintln!("skipping ActiveRecord hook proof: active_record is not installed ({stdout} {stderr})");
            return;
        }
        assert!(out.status.success(), "stdout:\n{stdout}\nstderr:\n{stderr}");
    }

    #[test]
    fn schema_format_is_sql_only_when_structure_sql_exists_without_schema_rb() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("db");
        std::fs::create_dir(&db).unwrap();

        assert_eq!(schema_format(tmp.path()), None, "neither file -> default");

        std::fs::write(db.join("structure.sql"), "-- sql\n").unwrap();
        assert_eq!(schema_format(tmp.path()), Some("sql"), "only structure.sql -> sql");

        std::fs::write(db.join("schema.rb"), "# rb\n").unwrap();
        assert_eq!(schema_format(tmp.path()), None, "both files -> schema.rb wins");

        std::fs::remove_file(db.join("structure.sql")).unwrap();
        assert_eq!(schema_format(tmp.path()), None, "only schema.rb -> default");
    }

    #[test]
    fn adapter_reads_first_adapter_line() {
        assert_eq!(adapter("default: &default\n  adapter: postgresql\n").as_deref(), Some("postgresql"));
        assert_eq!(adapter("test:\n  adapter: mysql2\n").as_deref(), Some("mysql2"));
        assert_eq!(adapter("test:\n  database: x\n"), None);
    }

    #[test]
    fn detects_a_rails_app_by_its_own_files() {
        let tmp = tempfile::tempdir().unwrap();
        let config = tmp.path().join("config");
        std::fs::create_dir(&config).unwrap();
        std::fs::write(config.join("database.yml"), "test:\n  adapter: postgresql\n").unwrap();
        assert!(!Rails.detect(tmp.path()), "database.yml alone is not a Rails app");

        std::fs::write(config.join("application.rb"), "module App\nend\n").unwrap();
        assert!(Rails.detect(tmp.path()), "config/application.rb");
        std::fs::remove_file(config.join("application.rb")).unwrap();
        assert!(!Rails.detect(tmp.path()));

        std::fs::create_dir(tmp.path().join("bin")).unwrap();
        std::fs::write(tmp.path().join("bin/rails"), "#!/usr/bin/env ruby\n").unwrap();
        assert!(Rails.detect(tmp.path()), "bin/rails");
        std::fs::remove_file(tmp.path().join("bin/rails")).unwrap();

        std::fs::write(tmp.path().join("Gemfile"), "# gem \"rails\"\ngem \"rails\"\n").unwrap();
        assert!(Rails.detect(tmp.path()), "gem \"rails\"");
        std::fs::write(tmp.path().join("Gemfile"), "gem \"haml-rails\"\n").unwrap();
        assert!(!Rails.detect(tmp.path()), "a name that merely contains rails");
    }

    #[test]
    fn sqlite_writes_nothing_and_exports_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(tmp.path().join("config")).unwrap();
        std::fs::write(
            tmp.path().join("config/database.yml"),
            "development:\n  adapter: sqlite3\n  database: db/development.sqlite3\n",
        )
        .unwrap();
        let mise = HashMap::new();
        let setup = with_data_dir(&data, || {
            let prior_redis = std::env::var_os("REDIS_URL");
            // SAFETY: `with_data_dir` holds `ENV_LOCK` across this closure.
            unsafe {
                std::env::set_var("DATABASE_URL", "sqlite3:storage/development.sqlite3");
                std::env::set_var("REDIS_URL", "redis://localhost/0");
            }
            let setup = Rails.setup(&ctx(tmp.path(), "prov", "ws-1", &mise)).unwrap();
            // SAFETY: still under `ENV_LOCK`.
            unsafe {
                match prior_redis {
                    Some(value) => std::env::set_var("REDIS_URL", value),
                    None => std::env::remove_var("REDIS_URL"),
                }
            }
            setup
        });

        assert!(setup.env.is_empty());
        assert!(setup.session_env.is_empty());
        assert!(setup.resources.is_empty());
        assert!(setup.failed_steps.is_empty());
        assert!(!data.join(rails_suffix::SCRIPT_NAME).exists());
        assert!(!tmp.path().join(rails_suffix::SCRIPT_NAME).exists());
    }

    /// A server database exports the suffix and `RUBYOPT`, never `DATABASE_URL`,
    /// and does not write the script into the app. Without a real Rails app the
    /// listing fails and nothing is prepared or recorded.
    #[test]
    fn setup_exports_the_suffix_and_does_not_write_into_the_app() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config/database.yml"), "test:\n  adapter: postgresql\n").unwrap();
        let mise = HashMap::new();
        let setup = with_data_dir(&data, || Rails.setup(&ctx(tmp.path(), "provtest", "ws-selftest", &mise)).unwrap());

        assert!(setup.env.is_empty(), "Rails no longer writes an env file: {:?}", setup.env);
        assert!(setup.resources.is_empty(), "a failed prepare records no database: {:?}", setup.resources);
        assert_eq!(setup.failed_steps, ["list rails databases"]);
        assert_eq!(setup.session_env[0], (SUFFIX_VAR.to_string(), "_ws_selftest".to_string()));
        assert_eq!(setup.session_env[1].0, "RUBYOPT");
        let script = data.join(rails_suffix::SCRIPT_NAME).canonicalize().unwrap();
        assert!(script.is_file(), "the script lands in the data dir");
        assert!(setup.session_env[1].1.contains(&format!("-r{}", script.display())));
        assert!(!tmp.path().join(rails_suffix::SCRIPT_NAME).exists());
        assert!(!tmp.path().join(".env.test.local").exists());
    }

    #[test]
    fn setup_does_not_prepare_when_the_script_would_land_in_a_repository() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("config")).unwrap();
        std::fs::create_dir_all(tmp.path().join("data").join(".git")).unwrap();
        std::fs::write(tmp.path().join("config/database.yml"), "test:\n  adapter: postgresql\n").unwrap();
        let mise = HashMap::new();
        let data = tmp.path().join("data");
        let setup = with_data_dir(&data, || Rails.setup(&ctx(tmp.path(), "prov", "ws-1", &mise)).unwrap());

        assert_eq!(setup.failed_steps, ["install rails db suffix"]);
        assert_eq!(
            setup.session_env,
            vec![(SUFFIX_VAR.to_string(), "_ws_1".to_string())],
            "the suffix is recorded so attach refuses to start without a hook"
        );
        assert!(setup.env.is_empty());
        assert!(!data.join(rails_suffix::SCRIPT_NAME).exists());
    }

    /// A sqlite file plus a server `DATABASE_URL` is the database the app opens.
    /// The hook has to be installed. Without Rails the listing fails closed and
    /// nothing is prepared.
    #[test]
    fn a_sqlite_file_plus_a_server_url_installs_the_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(tmp.path().join("config")).unwrap();
        std::fs::write(
            tmp.path().join("config/database.yml"),
            "development:\n  adapter: sqlite3\n  database: storage/development.sqlite3\n",
        )
        .unwrap();
        let mise = HashMap::new();
        let setup = with_data_dir(&data, || {
            let prior = std::env::var_os("DATABASE_URL");
            // SAFETY: `with_data_dir` holds `ENV_LOCK` across this closure.
            unsafe { std::env::set_var("DATABASE_URL", "postgres://localhost/app_development") };
            let setup = Rails.setup(&ctx(tmp.path(), "prov", "ws-url", &mise)).unwrap();
            // SAFETY: still under `ENV_LOCK`.
            unsafe {
                match prior {
                    Some(value) => std::env::set_var("DATABASE_URL", value),
                    None => std::env::remove_var("DATABASE_URL"),
                }
            }
            setup
        });

        assert!(setup.env.is_empty(), "{:?}", setup.env);
        assert!(setup.resources.is_empty(), "{:?}", setup.resources);
        assert_eq!(setup.failed_steps, ["list rails databases"]);
        assert_eq!(setup.session_env[0], (SUFFIX_VAR.to_string(), "_ws_url".to_string()));
        assert_eq!(setup.session_env[1].0, "RUBYOPT");
        assert!(data.join(rails_suffix::SCRIPT_NAME).is_file());
        assert!(!tmp.path().join(rails_suffix::SCRIPT_NAME).exists());
    }

    /// mise `[env]` overlays the process. A server URL from mise on a sqlite
    /// file installs the hook the same way a parent `DATABASE_URL` does.
    #[test]
    fn a_mise_server_url_on_a_sqlite_file_installs_the_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(tmp.path().join("config")).unwrap();
        std::fs::write(
            tmp.path().join("config/database.yml"),
            "development:\n  adapter: sqlite3\n  database: storage/development.sqlite3\n",
        )
        .unwrap();
        let mise = HashMap::from([("DATABASE_URL".to_string(), "postgres://localhost/from_mise".to_string())]);
        let setup = with_data_dir(&data, || Rails.setup(&ctx(tmp.path(), "prov", "ws-mise", &mise)).unwrap());

        assert_eq!(setup.failed_steps, ["list rails databases"]);
        assert_eq!(setup.session_env[0], (SUFFIX_VAR.to_string(), "_ws_mise".to_string()));
        assert_eq!(setup.session_env[1].0, "RUBYOPT");
        assert!(setup.session_env.iter().all(|(key, _)| key != "DATABASE_URL"));
    }

    /// A Rails 8 sqlite file whose server URL lives only in the dotenv files
    /// dotenv-rails loads. The parent and mise have neither variable. Without
    /// the hook, Rails opens the primary checkout's database.
    #[test]
    fn a_sqlite_file_plus_a_dotenv_server_url_installs_the_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(tmp.path().join("config")).unwrap();
        std::fs::write(
            tmp.path().join("config/database.yml"),
            "\
default: &default
  adapter: sqlite3
  pool: <%= ENV.fetch(\"RAILS_MAX_THREADS\") { 5 } %>
  timeout: 5000
development:
  <<: *default
  database: storage/development.sqlite3
test:
  <<: *default
  database: storage/test.sqlite3
",
        )
        .unwrap();
        std::fs::write(tmp.path().join("Gemfile.lock"), "    dotenv-rails (3.2.0)\n").unwrap();
        std::fs::write(tmp.path().join(".env.development"), "DATABASE_URL=postgres://localhost/app_development\n")
            .unwrap();
        std::fs::write(tmp.path().join(".env"), "DATABASE_URL=postgres://localhost/app_development\n").unwrap();
        std::fs::write(tmp.path().join(".env.test"), "DATABASE_URL=postgres://localhost/app_test\n").unwrap();
        let mise = HashMap::new();
        let setup = with_data_dir(&data, || Rails.setup(&ctx(tmp.path(), "prov", "ws-dotenv", &mise)).unwrap());

        assert!(setup.env.is_empty(), "{:?}", setup.env);
        assert!(setup.resources.is_empty(), "{:?}", setup.resources);
        assert_eq!(
            setup.failed_steps,
            ["list rails databases"],
            "failed_steps={:?} session_env={:?} resources={:?}",
            setup.failed_steps,
            setup.session_env,
            setup.resources
        );
        assert_eq!(setup.session_env[0], (SUFFIX_VAR.to_string(), "_ws_dotenv".to_string()));
        assert_eq!(setup.session_env[1].0, "RUBYOPT");
        assert!(data.join(rails_suffix::SCRIPT_NAME).is_file());
        assert!(!tmp.path().join(rails_suffix::SCRIPT_NAME).exists());
        assert!(setup.session_env.iter().all(|(key, _)| key != "DATABASE_URL"));
    }

    /// `db:test:purge` boots the test env, which loads `.env.test` and not the
    /// development files. That URL alone still has to install the hook.
    #[test]
    fn a_server_url_only_in_env_test_installs_the_hook() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(tmp.path().join("config")).unwrap();
        std::fs::write(
            tmp.path().join("config/database.yml"),
            "development:\n  adapter: sqlite3\n  database: storage/development.sqlite3\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("Gemfile.lock"), "    dotenv (3.2.0)\n").unwrap();
        std::fs::write(tmp.path().join(".env.test"), "DATABASE_URL=postgres://localhost/app_test\n").unwrap();
        let mise = HashMap::new();
        let setup = with_data_dir(&data, || Rails.setup(&ctx(tmp.path(), "prov", "ws-testenv", &mise)).unwrap());
        assert_eq!(setup.failed_steps, ["list rails databases"], "{:?}", setup.session_env);
        assert_eq!(setup.session_env[0].0, SUFFIX_VAR);
    }

    /// A line that does not parse is not "no URL". Setup warns and stops
    /// before prepare, with the hook recorded so a later session still suffixes.
    #[test]
    fn an_unparsed_dotenv_file_does_not_finish_setup_as_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(tmp.path().join("config")).unwrap();
        std::fs::write(
            tmp.path().join("config/database.yml"),
            "development:\n  adapter: sqlite3\n  database: storage/development.sqlite3\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("Gemfile.lock"), "    dotenv (3.2.0)\n").unwrap();
        std::fs::write(tmp.path().join(".env"), "NOT A VALID LINE\n").unwrap();
        let mise = HashMap::new();
        let setup = with_data_dir(&data, || Rails.setup(&ctx(tmp.path(), "prov", "ws-bad", &mise)).unwrap());
        assert_eq!(setup.failed_steps, ["read rails dotenv"], "{:?}", setup.failed_steps);
        assert_eq!(setup.session_env[0], (SUFFIX_VAR.to_string(), "_ws_bad".to_string()));
        assert_eq!(setup.session_env[1].0, "RUBYOPT");
        assert!(setup.resources.is_empty(), "{:?}", setup.resources);
        assert!(data.join(rails_suffix::SCRIPT_NAME).is_file());
    }

    /// A `.env` written for docker compose or a shell wrapper is not read by an
    /// app that has no dotenv gem, so it must not refuse setup.
    #[test]
    fn an_unparsed_env_file_is_ignored_when_the_app_has_no_dotenv_gem() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(tmp.path().join("config")).unwrap();
        std::fs::write(
            tmp.path().join("config/database.yml"),
            "development:\n  adapter: sqlite3\n  database: storage/development.sqlite3\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("Gemfile.lock"), "GEM\n  specs:\n    rails (8.0.0)\n").unwrap();
        std::fs::write(tmp.path().join(".env"), "GIT_SHA=$(git rev-parse HEAD)\nKEY=a b\n").unwrap();
        let mise = HashMap::new();
        let setup = with_data_dir(&data, || Rails.setup(&ctx(tmp.path(), "prov", "ws-nodotenv", &mise)).unwrap());
        assert!(setup.failed_steps.is_empty(), "{:?}", setup.failed_steps);
        assert!(setup.session_env.is_empty(), "{:?}", setup.session_env);
    }

    /// The URL dotenv will not apply is not a server database. A sqlite file
    /// whose winning URL is sqlite stays a sqlite file.
    #[test]
    fn a_sqlite_dotenv_url_and_a_losing_server_url_are_left_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir(tmp.path().join("config")).unwrap();
        std::fs::write(
            tmp.path().join("config/database.yml"),
            "development:\n  adapter: sqlite3\n  database: storage/development.sqlite3\ntest:\n  adapter: sqlite3\n  database: storage/test.sqlite3\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("Gemfile.lock"), "    dotenv (3.2.0)\n").unwrap();
        std::fs::write(tmp.path().join(".env.development.local"), "DATABASE_URL=sqlite3:storage/development.sqlite3\n")
            .unwrap();
        std::fs::write(tmp.path().join(".env.test"), "DATABASE_URL=sqlite3:storage/test.sqlite3\n").unwrap();
        std::fs::write(tmp.path().join(".env"), "DATABASE_URL=postgres://localhost/app_development\n").unwrap();
        let mise = HashMap::from([("DATABASE_URL".to_string(), "sqlite3:storage/from_mise.sqlite3".to_string())]);
        let setup = with_data_dir(&data, || Rails.setup(&ctx(tmp.path(), "prov", "ws-sqlite", &mise)).unwrap());
        assert!(setup.failed_steps.is_empty(), "{:?}", setup.failed_steps);
        assert!(setup.session_env.is_empty(), "{:?}", setup.session_env);
        assert!(!data.join(rails_suffix::SCRIPT_NAME).exists());
    }

    /// A server adapter that the old first-line scan would skip still has to be
    /// proven. The runner cannot boot here, so prepare does not run.
    #[test]
    fn a_hidden_server_database_is_not_silently_skipped() {
        for yml in [
            "development:\n  adapter: sqlite3\n  database: db/q.sqlite3\nprimary:\n  adapter: postgresql\n",
            "development:\n  adapter: \"postgresql\"\n",
            "development:\n  adapter: postgresql # primary\n",
            "development:\n  adapter: postgis\n",
            "development:\n  adapter: <%= ENV[\"DB_ADAPTER\"] %>\n",
        ] {
            let tmp = tempfile::tempdir().unwrap();
            let data = tmp.path().join("data");
            std::fs::create_dir(tmp.path().join("config")).unwrap();
            std::fs::write(tmp.path().join("config/database.yml"), yml).unwrap();
            let mise = HashMap::new();
            let setup = with_data_dir(&data, || Rails.setup(&ctx(tmp.path(), "prov", "ws-1", &mise)).unwrap());
            assert_eq!(setup.failed_steps, ["list rails databases"], "{yml}: {:?}", setup.failed_steps);
            assert!(!setup.session_env.is_empty(), "{yml} exported nothing, so Rails would use the primary names");
            assert!(setup.resources.is_empty(), "{yml}");
        }
    }

    fn bundle_ready(dir: &Path) -> bool {
        Command::new("bundle").arg("check").current_dir(dir).output().is_ok_and(|out| out.status.success())
    }

    /// A mise-installed Ruby whose own gem directory contains ActiveRecord.
    /// The `ruby` on PATH often does not. The newest version wins.
    fn ruby_with_activerecord() -> Option<PathBuf> {
        let home = std::env::var_os("HOME")?;
        let root = PathBuf::from(home).join(".local/share/mise/installs/ruby");
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&root).ok()?.flatten() {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if !name.chars().next().is_some_and(|ch| ch.is_ascii_digit()) || !name.contains('.') {
                continue;
            }
            let ruby = entry.path().join("bin/ruby");
            if ruby.is_file() && activerecord_gem(&entry.path()) {
                found.push((name.to_string(), ruby));
            }
        }
        found.sort();
        found.pop().map(|(_, ruby)| ruby)
    }

    fn activerecord_gem(prefix: &Path) -> bool {
        let Ok(apis) = std::fs::read_dir(prefix.join("lib/ruby/gems")) else {
            return false;
        };
        for api in apis.flatten() {
            let Ok(gems) = std::fs::read_dir(api.path().join("gems")) else {
                continue;
            };
            if gems.flatten().any(|gem| gem.file_name().to_string_lossy().starts_with("activerecord-")) {
                return true;
            }
        }
        false
    }

    fn postgres_up() -> bool {
        Command::new("psql").args(["-tAc", "SELECT 1"]).output().is_ok_and(|out| out.status.success())
    }

    /// With `WORKON_REQUIRE_DB_CYCLES` set (ci.yml's provisioners job), a
    /// missing Postgres, jj or fixture bundle fails the test: skipping would
    /// report the cycle green when it never touched a database. Elsewhere the
    /// cycle skips, as the other provisioners' cycles do.
    fn require_ready(ready: bool, what: &str) -> bool {
        if !ready && std::env::var_os("WORKON_REQUIRE_DB_CYCLES").is_some() {
            panic!("{what} is required for the Rails cycle");
        }
        ready
    }

    fn widgets(name: &str) -> String {
        let out = Command::new("psql").args(["-tAc", "select to_regclass('public.widgets')", name]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Full cycle against the real Rails fixture: `db:prepare` loads `widgets`
    /// into the suffixed development and test databases. The replica is
    /// suffixed and listed, but Rails creates none for it, and `DATABASE_URL` is not exported.
    #[test]
    fn rails_cycle_loads_schema_into_isolated_databases() {
        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rails"));
        if !require_ready(postgres_up(), "Postgres") {
            return;
        }
        if !require_ready(bundle_ready(fixture), "the Rails fixture bundle") {
            return;
        }
        let suffix = rails_suffix::db_suffix("ws-cycle");
        let names = [format!("workon_rails_fixture_development{suffix}"), format!("workon_rails_fixture_test{suffix}")];
        for name in &names {
            Resource::PostgresDb { name: name.clone() }.teardown();
        }

        let tmp = tempfile::tempdir().unwrap();
        let ws_dir = tmp.path().join("railsfix-ws-cycle");
        let data = tmp.path().join("data");
        assert!(Command::new("cp")
            .args(["-R", fixture.to_str().unwrap(), ws_dir.to_str().unwrap()])
            .status()
            .unwrap()
            .success());
        let mise = HashMap::new();
        let setup = with_data_dir(&data, || Rails.setup(&ctx(&ws_dir, "railsfix", "ws-cycle", &mise)).unwrap());

        let tables: Vec<String> = names.iter().map(|name| widgets(name)).collect();
        for name in &names {
            Resource::PostgresDb { name: name.clone() }.teardown();
        }
        assert!(setup.failed_steps.is_empty(), "{:?}", setup.failed_steps);
        assert!(setup.env.is_empty());
        let replica = format!("workon_rails_fixture_replica{suffix}");
        assert_eq!(
            setup.resources,
            [&names[0], &replica, &names[1]].map(|name| Resource::PostgresDb { name: name.clone() }),
            "the suffixed replica is listed so teardown covers it"
        );
        assert_eq!(tables, ["widgets", "widgets"]);
        assert!(!db_exists(&replica), "Rails creates no replica");
        assert!(!db_exists("workon_rails_fixture_replica"), "the unsuffixed replica is never created");
    }

    /// When a Rails app keeps `db/structure.sql` instead of `db/schema.rb`,
    /// prepare creates the databases and `SCHEMA_FORMAT=sql db:schema:load`
    /// loads `db/structure.sql` into both.
    #[test]
    fn rails_cycle_loads_structure_sql_when_schema_rb_is_absent() {
        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rails"));
        if !require_ready(postgres_up(), "Postgres") {
            return;
        }
        if !require_ready(bundle_ready(fixture), "the Rails fixture bundle") {
            return;
        }
        let suffix = rails_suffix::db_suffix("ws-struct");
        let names = [format!("workon_rails_fixture_development{suffix}"), format!("workon_rails_fixture_test{suffix}")];
        for name in &names {
            Resource::PostgresDb { name: name.clone() }.teardown();
        }

        let tmp = tempfile::tempdir().unwrap();
        let ws_dir = tmp.path().join("railsfix-ws-struct");
        let data = tmp.path().join("data");
        assert!(Command::new("cp")
            .args(["-R", fixture.to_str().unwrap(), ws_dir.to_str().unwrap()])
            .status()
            .unwrap()
            .success());
        std::fs::remove_file(ws_dir.join("db/schema.rb")).unwrap();
        std::fs::write(
            ws_dir.join("db/structure.sql"),
            "CREATE TABLE public.widgets (id bigint PRIMARY KEY, name character varying);\n",
        )
        .unwrap();
        let mise = HashMap::new();
        let setup = with_data_dir(&data, || Rails.setup(&ctx(&ws_dir, "railsfix", "ws-struct", &mise)).unwrap());
        let tables: Vec<String> = names.iter().map(|name| widgets(name)).collect();
        for name in &names {
            Resource::PostgresDb { name: name.clone() }.teardown();
        }

        assert!(setup.failed_steps.is_empty(), "{:?}", setup.failed_steps);
        assert_eq!(tables, ["widgets", "widgets"]);
    }

    fn db_exists(name: &str) -> bool {
        assert!(name.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_'), "{name}");
        let out = Command::new("psql")
            .args(["-tAc", &format!("SELECT 1 FROM pg_database WHERE datname = '{name}'")])
            .output()
            .unwrap();
        String::from_utf8_lossy(&out.stdout).trim() == "1"
    }

    /// A real jj workspace: setup leaves `jj status` clean and the main checkout
    /// untouched, then the workspace's Rails commands use the suffixed databases.
    #[test]
    fn rails_workspace_isolates_databases_and_leaves_jj_clean() {
        if !require_ready(vcs_runner::jj_available(), "jj") {
            return;
        }
        if !require_ready(postgres_up(), "Postgres") {
            return;
        }
        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rails"));
        if !require_ready(bundle_ready(fixture), "the Rails fixture bundle") {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let ws = tmp.path().join("ws");
        let data = tmp.path().join("workon-data");
        assert!(Command::new("cp")
            .args(["-R", fixture.to_str().unwrap(), repo.to_str().unwrap()])
            .status()
            .unwrap()
            .success());

        let token = format!("w{:x}{:x}", std::process::id(), rand::random::<u32>());
        let dev_bare = format!("{token}_{}", "d".repeat(70));
        let test_bare = format!("{token}_test");
        let replica_bare = format!("{token}_replica");
        let yml = format!(
            "\
default: &default
  adapter: postgresql
  pool: 5

development:
  primary:
    <<: *default
    database: {dev_bare}
  primary_replica:
    <<: *default
    database: {replica_bare}
    replica: true

test:
  primary:
    <<: *default
    database: {test_bare}
"
        );
        std::fs::write(repo.join("config/database.yml"), &yml).unwrap();
        for bare in [&dev_bare, &test_bare, &replica_bare] {
            assert!(!db_exists(bare), "the unsuffixed name must not already exist: {bare}");
        }

        git(&repo, &["init", "--initial-branch=main"]);
        git(&repo, &["config", "user.email", "t@t.com"]);
        git(&repo, &["config", "user.name", "T"]);
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-m", "init"]);
        vcs_runner::run_jj(&repo, &["git", "init", "--colocate"]).unwrap();
        vcs_runner::run_jj(&repo, &["workspace", "add", ws.to_str().unwrap(), "--name", "rails-db", "-r", "main"])
            .unwrap();
        copy_bundle(&repo, &ws);

        let before = snapshot(&repo);
        let mise = HashMap::new();
        let ws_ctx = ProvisionCtx {
            project_dir: &repo,
            project_name: "railsfix",
            ws_id: "ws-isolate",
            ws_dir: &ws,
            mise_vars: &mise,
        };
        let setup = with_data_dir(&data, || Rails.setup(&ws_ctx).unwrap());
        let suffix = rails_suffix::db_suffix("ws-isolate");

        let guard = DropDbs(setup.resources.clone());
        let status = vcs_runner::run_jj_utf8(&ws, &["status"]).unwrap();
        assert!(status.contains("no changes") || status.trim().is_empty(), "jj status after setup:\n{status}");
        assert_eq!(snapshot(&repo), before, "the main checkout changed");
        assert!(data.join(rails_suffix::SCRIPT_NAME).is_file());
        assert!(!ws.join(rails_suffix::SCRIPT_NAME).exists());
        assert!(!ws.join(".env.test.local").exists());
        assert!(setup.env.is_empty());
        assert!(setup.failed_steps.is_empty(), "{:?}", setup.failed_steps);
        assert!(setup.resources.len() >= 2, "{:?}", setup.resources);
        for resource in &setup.resources {
            let name = resource.db_name();
            assert!(name.ends_with(&suffix), "{name}");
            assert!(name.len() <= 63, "{name}");
            if !name.contains("_replica_") {
                assert_eq!(widgets(name), "widgets", "{name}");
            }
        }
        let bare_prefix = dev_bare.chars().take(63).collect::<String>();
        for bare in [&dev_bare, &test_bare, &replica_bare, &bare_prefix] {
            if db_exists(bare) {
                Resource::PostgresDb { name: bare.clone() }.teardown();
                panic!("unsuffixed database was created: {bare}");
            }
        }

        let env = &setup.session_env;
        rails_ok(&ws, env, &["exec", "rails", "runner", "raise 'suffix missing' unless ENV['WORKON_RAILS_DB_SUFFIX']"]);
        rails_ok(&ws, env, &["exec", "bin/rails", "test"]);
        rails_ok(&ws, env, &["exec", "bin/rails", "test:all"]);
        rails_ok(&ws, env, &["exec", "bin/rails", "db:test:prepare"]);
        assert_database_url_is_suffixed(&ws, env, &dev_bare, &suffix);
        let test_db = setup.resources.iter().find(|resource| resource.db_name().contains("_test")).unwrap();
        assert_eq!(widgets(test_db.db_name()), "widgets");
        drop(guard);
    }

    struct DropDbs(Vec<Resource>);

    impl Drop for DropDbs {
        fn drop(&mut self) {
            for resource in &self.0 {
                resource.teardown();
            }
        }
    }

    fn assert_database_url_is_suffixed(ws: &Path, env: &[(String, String)], bare: &str, suffix: &str) {
        let url = format!("postgres://localhost/{bare}");
        let listed = rails_output(
            ws,
            env,
            &[
                "exec",
                "rails",
                "runner",
                "puts ActiveRecord::Base.configurations.configs_for(env_name: 'development', include_hidden: true).map { |config| config.database }",
            ],
            Some(&url),
        );
        assert!(
            listed.status.success(),
            "DATABASE_URL runner failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&listed.stdout),
            String::from_utf8_lossy(&listed.stderr)
        );
        let printed = String::from_utf8_lossy(&listed.stdout);
        assert!(printed.lines().any(|line| line.trim_end().ends_with(suffix)), "{printed}");
        assert!(printed.lines().all(|line| line.trim() != bare), "{printed}");
        assert!(!db_exists(bare), "DATABASE_URL must not create the primary checkout's database");
    }

    fn rails_ok(ws: &Path, env: &[(String, String)], args: &[&str]) {
        let out = rails_output(ws, env, args, None);
        assert!(
            out.status.success(),
            "bundle {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// A session keeps `DATABASE_URL`. `bin/rails test` is not given one here:
    /// that variable would point the test run at the development database.
    fn rails_output(
        ws: &Path,
        env: &[(String, String)],
        args: &[&str],
        database_url: Option<&str>,
    ) -> std::process::Output {
        let mut cmd = Command::new("bundle");
        // Other tests set DATABASE_URL in this process for a while, and a child
        // inherits whatever it holds when it starts.
        cmd.args(args).current_dir(ws).env_remove("DATABASE_URL");
        if let Some(url) = database_url {
            cmd.env("DATABASE_URL", url);
        }
        for (key, value) in env {
            cmd.env(key, value);
        }
        cmd.output().unwrap()
    }

    fn copy_bundle(from: &Path, to: &Path) {
        for name in ["Gemfile.lock", ".bundle", "vendor"] {
            let src = from.join(name);
            if !src.exists() {
                continue;
            }
            let dest = to.join(name);
            assert!(Command::new("cp")
                .args(["-R", src.to_str().unwrap(), dest.to_str().unwrap()])
                .status()
                .unwrap()
                .success());
        }
    }

    fn git(repo: &Path, args: &[&str]) {
        let mut full = vec!["-C", repo.to_str().unwrap()];
        full.extend_from_slice(args);
        let status = Command::new("git").args(&full).status().unwrap();
        assert!(status.success(), "git {args:?}");
    }

    fn snapshot(dir: &Path) -> std::collections::BTreeMap<std::path::PathBuf, Vec<u8>> {
        let mut out = std::collections::BTreeMap::new();
        walk(dir, dir, &mut out);
        out
    }

    fn walk(root: &Path, dir: &Path, out: &mut std::collections::BTreeMap<std::path::PathBuf, Vec<u8>>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let name = entry.file_name();
            if name == ".git" || name == ".jj" {
                continue;
            }
            if path.is_dir() {
                walk(root, &path, out);
            } else if path.is_file() {
                out.insert(path.strip_prefix(root).unwrap().to_path_buf(), std::fs::read(&path).unwrap());
            }
        }
    }
}
