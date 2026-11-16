//! Rails provisioner: isolated development and test databases per workspace.
//!
//! `config/database.yml` keeps the primary checkout's names. The workspace
//! exports `WORKON_RAILS_DB_SUFFIX` and a `RUBYOPT` that loads a script from
//! workon's data directory. The script suffixes the development and test names
//! (replicas stay put) and holds no per-workspace data. `DATABASE_URL` is left
//! unset, so `db:prepare` creates and loads both databases. No file is written
//! into the workspace.

use std::path::Path;

use anyhow::Result;
use vcs_runner::Cmd;

use super::rails_suffix::{self, LIST_DATABASES, SUFFIX_VAR};
use super::{run_step, ProvisionCtx, Provisioner, Resource, Setup};

pub struct Rails;

impl Provisioner for Rails {
    fn name(&self) -> &'static str {
        "rails"
    }

    fn detect(&self, ws_dir: &Path) -> bool {
        ws_dir.join("config/database.yml").is_file()
    }

    fn setup(&self, ctx: &ProvisionCtx<'_>) -> Result<Setup> {
        let yml = std::fs::read_to_string(ctx.ws_dir.join("config/database.yml")).unwrap_or_default();
        if !provisions_server_db(&yml) {
            return Ok(Setup::default());
        }

        let script = match rails_suffix::install() {
            Ok(path) => path,
            Err(err) => {
                eprintln!("Warning: could not install the Rails database suffix: {err}");
                return Ok(Setup { failed_steps: vec!["install rails db suffix".to_string()], ..Setup::default() });
            }
        };
        let suffix = rails_suffix::db_suffix(ctx.ws_id);
        let inherited = ctx.mise_vars.get("RUBYOPT").cloned().or_else(|| std::env::var("RUBYOPT").ok());
        let rubyopt = match rails_suffix::rubyopt_loading(inherited.as_deref(), &script) {
            Ok(value) => value,
            Err(err) => {
                eprintln!("Warning: could not install the Rails database suffix: {err}");
                return Ok(Setup { failed_steps: vec!["install rails db suffix".to_string()], ..Setup::default() });
            }
        };

        let mut failed_steps: Vec<String> = bundle_install(ctx).into_iter().collect();
        // Names first. `db:prepare` creates whatever the hook resolved, so a hook
        // that did not load must not run against the primary checkout's databases.
        let mut resources = Vec::new();
        if failed_steps.is_empty() {
            match list_databases(ctx, &suffix, &rubyopt) {
                Ok(listed) => {
                    let (isolated, all_isolated) = rails_suffix::isolated_resources(listed, &suffix);
                    if all_isolated {
                        resources = isolated;
                    } else {
                        eprintln!(
                            "Warning: list rails databases failed: a database name did not end with the workspace suffix"
                        );
                        failed_steps.push("list rails databases".to_string());
                    }
                }
                Err(err) => {
                    eprintln!("Warning: list rails databases failed: {err}");
                    failed_steps.push("list rails databases".to_string());
                }
            }
        }
        if failed_steps.is_empty() {
            eprintln!("Preparing this workspace's Rails development and test databases...");
            failed_steps.extend(run_step(prepare_cmd(ctx, &suffix, &rubyopt), "rails db:prepare"));
        }
        if failed_steps.is_empty() && schema_format(ctx.ws_dir) == Some("sql") {
            failed_steps.extend(run_step(structure_load_cmd(ctx, &suffix, &rubyopt), "rails db:schema:load"));
        }

        Ok(Setup {
            resources,
            session_env: vec![(SUFFIX_VAR.to_string(), suffix), ("RUBYOPT".to_string(), rubyopt)],
            failed_steps,
            ..Setup::default()
        })
    }
}

/// `true` when the first adapter is a server database. SQLite is a file that
/// the worktree copy already has. An unknown adapter is skipped out loud.
fn provisions_server_db(database_yml: &str) -> bool {
    match adapter(database_yml).as_deref() {
        Some("sqlite3") => false,
        Some("postgresql" | "mysql2" | "mysql" | "trilogy") | None => true,
        Some(other) => {
            eprintln!("Rails adapter `{other}` not supported; skipping DB setup");
            false
        }
    }
}

fn prepare_cmd(ctx: &ProvisionCtx<'_>, suffix: &str, rubyopt: &str) -> Cmd {
    rails_cmd(ctx, suffix, rubyopt).args(["exec", "rails", "db:prepare"]).env("RAILS_ENV", "development")
}

/// `db:prepare` does not read `SCHEMA_FORMAT`. With `DATABASE_URL` unset this
/// load covers development and the test database together.
fn structure_load_cmd(ctx: &ProvisionCtx<'_>, suffix: &str, rubyopt: &str) -> Cmd {
    rails_cmd(ctx, suffix, rubyopt)
        .args(["exec", "rails", "db:schema:load"])
        .env("RAILS_ENV", "development")
        .env("SCHEMA_FORMAT", "sql")
}

fn list_databases(ctx: &ProvisionCtx<'_>, suffix: &str, rubyopt: &str) -> Result<Vec<Resource>> {
    let out = rails_cmd(ctx, suffix, rubyopt)
        .args(["exec", "rails", "runner", LIST_DATABASES])
        .env("RAILS_ENV", "development")
        .run()
        .map_err(|err| anyhow::anyhow!(err))?;
    Ok(rails_suffix::parse_db_listing(&out.stdout_lossy()))
}

fn rails_cmd(ctx: &ProvisionCtx<'_>, suffix: &str, rubyopt: &str) -> Cmd {
    let mut cmd = Cmd::new("bundle").in_dir(ctx.ws_dir);
    for (key, value) in ctx.mise_vars {
        cmd = cmd.env(key, value);
    }
    cmd.env(SUFFIX_VAR, suffix).env("RUBYOPT", rubyopt)
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

/// The first `adapter:` value in `database.yml` (usually under `default:`),
/// e.g. `postgresql`, `mysql2`, `sqlite3`.
pub(crate) fn adapter(database_yml: &str) -> Option<String> {
    database_yml.lines().find_map(|line| {
        let value = line.trim().strip_prefix("adapter:")?.trim();
        (!value.is_empty()).then(|| value.to_string())
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;
    use std::process::Command;

    use super::*;

    fn ctx<'a>(dir: &'a Path, project: &'a str, ws_id: &'a str, mise: &'a HashMap<String, String>) -> ProvisionCtx<'a> {
        ProvisionCtx { project_dir: dir, project_name: project, ws_id, ws_dir: dir, mise_vars: mise }
    }

    fn with_data_dir<T>(dir: &Path, body: impl FnOnce() -> T) -> T {
        rails_suffix::with_env_var("WORKON_DATA_DIR", dir, body)
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
    fn detects_a_rails_app_by_database_yml() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!Rails.detect(tmp.path()), "no config/database.yml -> not detected");

        std::fs::create_dir(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config/database.yml"), "test:\n  adapter: postgresql\n").unwrap();
        assert!(Rails.detect(tmp.path()), "config/database.yml -> detected");
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
        let setup = with_data_dir(&data, || Rails.setup(&ctx(tmp.path(), "prov", "ws-1", &mise)).unwrap());

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
        let script = data.join(rails_suffix::SCRIPT_NAME);
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
        assert!(setup.session_env.is_empty(), "no hook, so no suffix that would miss");
        assert!(setup.env.is_empty());
        assert!(!data.join(rails_suffix::SCRIPT_NAME).exists());
    }

    fn bundle_ready(dir: &Path) -> bool {
        Command::new("bundle").arg("check").current_dir(dir).output().is_ok_and(|out| out.status.success())
    }

    fn postgres_up() -> bool {
        Command::new("psql").args(["-tAc", "SELECT 1"]).output().is_ok_and(|out| out.status.success())
    }

    fn widgets(name: &str) -> String {
        let out = Command::new("psql").args(["-tAc", "select to_regclass('public.widgets')", name]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Full cycle against the real Rails fixture: `db:prepare` loads `widgets`
    /// into the suffixed development and test databases. The replica is not
    /// created and `DATABASE_URL` is not exported.
    #[test]
    fn rails_cycle_loads_schema_into_isolated_databases() {
        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rails"));
        if !postgres_up() || !bundle_ready(fixture) {
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
        assert_eq!(
            setup.resources,
            names.iter().map(|name| Resource::PostgresDb { name: name.clone() }).collect::<Vec<_>>()
        );
        assert_eq!(tables, ["widgets", "widgets"]);
        assert!(!db_exists("workon_rails_fixture_replica"), "the replica stays unsuffixed and uncreated");
    }

    /// When a Rails app keeps `db/structure.sql` instead of `db/schema.rb`,
    /// prepare creates the databases and `SCHEMA_FORMAT=sql db:schema:load`
    /// loads `db/structure.sql` into both.
    #[test]
    fn rails_cycle_loads_structure_sql_when_schema_rb_is_absent() {
        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rails"));
        if !postgres_up() || !bundle_ready(fixture) {
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
        if !vcs_runner::jj_available() || !postgres_up() {
            return;
        }
        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rails"));
        if !bundle_ready(fixture) {
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
            assert_eq!(widgets(name), "widgets", "{name}");
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

    fn rails_ok(ws: &Path, env: &[(String, String)], args: &[&str]) {
        let mut cmd = Command::new("bundle");
        cmd.args(args).current_dir(ws).env_remove("DATABASE_URL");
        for (key, value) in env {
            cmd.env(key, value);
        }
        let out = cmd.output().unwrap();
        assert!(
            out.status.success(),
            "bundle {args:?} failed\nstdout:\n{}\nstderr:\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
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

    proptest::proptest! {
        /// Whatever surrounds it, the first `adapter:` line with a value names the adapter.
        #[test]
        fn adapter_is_the_first_adapter_line_with_a_value(
            before in proptest::collection::vec("  (database|host|pool): [a-z0-9_]{0,8}", 0..4),
            blank_first in proptest::bool::ANY,
            indent in " {0,4}",
            value in "[a-z0-9_]{1,12}",
            after in proptest::collection::vec("  adapter: [a-z]{1,8}", 0..3),
        ) {
            let blank = if blank_first { "  adapter:\n" } else { "" };
            let yml = format!("test:\n{}\n{blank}{indent}adapter: {value}\n{}\n", before.join("\n"), after.join("\n"));
            proptest::prop_assert_eq!(adapter(&yml), Some(value));
        }
    }
}
