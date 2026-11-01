//! Rails provisioner: an isolated Postgres test database per workspace, schema
//! loaded. Preserves the behavior of the pre-registry `setup_rails_db`.
//!
//! Isolation is delivered through `.env.test.local` (written by the caller from
//! `Setup.env`), not the session env: dotenv-rails loads that file only under
//! `RAILS_ENV=test`, so the workspace's dev commands keep using the dev DB while
//! test commands use the isolated one. Injecting `DATABASE_URL` into the session
//! would wrongly point every command at the test DB.

use std::path::Path;

use anyhow::Result;
use vcs_runner::Cmd;

use super::{run_step, test_db_name, DbEngine, ProvisionCtx, Provisioner, Setup};

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
        let engine = match adapter(&yml).as_deref() {
            Some("postgresql") | None => DbEngine::Postgres,
            Some("mysql2") | Some("mysql") | Some("trilogy") => DbEngine::Mysql,
            Some("sqlite3") => return Ok(Setup::default()), // file-based, copied with the worktree
            Some(other) => {
                eprintln!("Rails adapter `{other}` not supported; skipping DB setup");
                return Ok(Setup::default());
            }
        };
        let db = test_db_name(ctx.project_name, ctx.ws_id);

        eprintln!("Creating test database {db}...");
        if let Err(e) = engine.create(&db) {
            return Ok(Setup::database_not_created(&db, &e));
        }

        // The DB exists now — record it (resource-before-risk) so teardown drops
        // it even if the schema load below fails.
        let resource = engine.resource(&db);
        let url = engine.url(&db);

        // Gems before any bundler/Rails step (issue #4), then the schema load, which
        // the install going wrong must not leave half-run.
        let mut failed_steps: Vec<String> = bundle_install(ctx).into_iter().collect();
        if failed_steps.is_empty() {
            eprintln!("Loading schema...");
            let mut cmd = Cmd::new("bundle")
                .args(["exec", "rails", "db:schema:load"])
                .env("RAILS_ENV", "test")
                .env("DATABASE_URL", &url)
                .in_dir(ctx.ws_dir);
            if let Some(fmt) = schema_format(ctx.ws_dir) {
                cmd = cmd.env("SCHEMA_FORMAT", fmt);
            }
            for (k, v) in ctx.mise_vars {
                cmd = cmd.env(k, v);
            }
            failed_steps.extend(run_step(cmd, "rails db:schema:load"));
        }

        // No env_file: .env.test.local, which dotenv-rails loads under RAILS_ENV=test.
        Ok(Setup::database_url(vec![resource], url, failed_steps))
    }
}

/// `bundle check || bundle install` in the workspace when it has a Gemfile. Git-sourced
/// gems are checked out into a per-checkout bundler path the gitignored-file clone does
/// not cover (issue #4), so `db:schema:load` would fail with "not yet checked out" until
/// this runs. `None` means the gems are ready (or there is no Gemfile); a failed install
/// is reported like every other step, naming its command.
fn bundle_install(ctx: &ProvisionCtx<'_>) -> Option<String> {
    if !ctx.ws_dir.join("Gemfile").is_file() {
        return None;
    }
    let bundle = |args: &[&str]| {
        let mut cmd = Cmd::new("bundle").args(args).in_dir(ctx.ws_dir);
        for (k, v) in ctx.mise_vars {
            cmd = cmd.env(k, v);
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
/// in favour of `db:schema:load` reading `SCHEMA_FORMAT=sql`.
pub(crate) fn schema_format(ws_dir: &Path) -> Option<&'static str> {
    (ws_dir.join("db/structure.sql").is_file() && !ws_dir.join("db/schema.rb").is_file()).then_some("sql")
}

/// The first `adapter:` value in `database.yml` (usually under `default:`),
/// e.g. `postgresql`, `mysql2`, `sqlite3`.
pub(crate) fn adapter(database_yml: &str) -> Option<String> {
    database_yml.lines().find_map(|l| {
        let v = l.trim().strip_prefix("adapter:")?.trim();
        (!v.is_empty()).then(|| v.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// `setup` creates the isolated DB and returns the resource + `DATABASE_URL`.
    /// Gated on a reachable Postgres server; skips otherwise (schema load no-ops
    /// without a real Rails app, which is fine — we're asserting the DB + Setup,
    /// not the schema). Runs in CI's Postgres job.
    #[test]
    fn setup_creates_db_and_returns_resource_and_env() {
        use crate::provision::Resource;
        use std::collections::HashMap;

        let name = test_db_name("provtest", "ws-selftest");
        // Skip unless we can create DBs; also clear any leftover so `setup`'s own
        // createdb succeeds.
        Resource::PostgresDb { name: name.clone() }.teardown();
        if DbEngine::Postgres.create(&name).is_err() {
            return; // no reachable Postgres server
        }
        Resource::PostgresDb { name: name.clone() }.teardown();

        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("config")).unwrap();
        std::fs::write(tmp.path().join("config/database.yml"), "test:\n  adapter: postgresql\n").unwrap();
        let mise = HashMap::new();
        let ctx = ProvisionCtx {
            project_dir: tmp.path(),
            project_name: "provtest",
            ws_id: "ws-selftest",
            ws_dir: tmp.path(),
            mise_vars: &mise,
        };

        let setup = Rails.setup(&ctx).unwrap();
        Resource::PostgresDb { name: name.clone() }.teardown(); // clean up before asserting

        assert_eq!(setup.resources, vec![Resource::PostgresDb { name: name.clone() }]);
        assert_eq!(setup.env.len(), 1);
        assert_eq!(setup.env[0].0, "DATABASE_URL");
        assert!(setup.env[0].1.contains(&name), "url targets the db: {}", setup.env[0].1);
    }

    /// Full cycle against the real Rails fixture: provision it, then assert the
    /// schema was actually loaded into the isolated DB (the `widgets` table
    /// exists), then drop it. Gated on a reachable Postgres AND the fixture's
    /// bundle being installed (CI installs Ruby + runs `bundle install`; this
    /// machine has both), skips otherwise.
    #[test]
    fn rails_cycle_loads_schema_into_isolated_db() {
        use crate::provision::Resource;
        use std::collections::HashMap;
        use std::process::Command;

        let name = test_db_name("railsfix", "ws-cycle");
        Resource::PostgresDb { name: name.clone() }.teardown();
        if DbEngine::Postgres.create(&name).is_err() {
            return; // no reachable Postgres server
        }
        Resource::PostgresDb { name: name.clone() }.teardown();

        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rails");
        let bundle_ready = Command::new("bundle")
            .arg("check")
            .current_dir(fixture)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !bundle_ready {
            return; // fixture gems not installed
        }

        // Provision runs `bundle exec rails db:schema:load` in ws_dir, so give it
        // its own copy of the fixture.
        let tmp = tempfile::tempdir().unwrap();
        let ws_dir = tmp.path().join("railsfix-ws-cycle");
        assert!(
            Command::new("cp").args(["-R", fixture, ws_dir.to_str().unwrap()]).status().unwrap().success(),
            "copy fixture",
        );

        let mise = HashMap::new();
        let ctx = ProvisionCtx {
            project_dir: &ws_dir,
            project_name: "railsfix",
            ws_id: "ws-cycle",
            ws_dir: &ws_dir,
            mise_vars: &mise,
        };
        let setup = Rails.setup(&ctx).unwrap();
        assert_eq!(setup.resources, vec![Resource::PostgresDb { name: name.clone() }]);

        let out = Command::new("psql").args(["-tAc", "select to_regclass('public.widgets')", &name]).output().unwrap();
        let table = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Resource::PostgresDb { name: name.clone() }.teardown(); // clean up before asserting

        assert_eq!(table, "widgets", "db:schema:load should have created the widgets table");
    }

    /// When a Rails app keeps `db/structure.sql` instead of `db/schema.rb`,
    /// `Rails::setup` passes `SCHEMA_FORMAT=sql` to `db:schema:load` so Rails 7+
    /// loads `db/structure.sql` instead of aborting on the missing `db/schema.rb`.
    #[test]
    fn rails_cycle_loads_structure_sql_when_schema_rb_is_absent() {
        use crate::provision::Resource;
        use std::collections::HashMap;
        use std::process::Command;

        let name = test_db_name("railsfix", "ws-struct");
        Resource::PostgresDb { name: name.clone() }.teardown();
        if DbEngine::Postgres.create(&name).is_err() {
            return; // no reachable Postgres server
        }
        Resource::PostgresDb { name: name.clone() }.teardown();

        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rails");
        let bundle_ready = Command::new("bundle")
            .arg("check")
            .current_dir(fixture)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !bundle_ready {
            return; // fixture gems not installed
        }

        let tmp = tempfile::tempdir().unwrap();
        let ws_dir = tmp.path().join("railsfix-ws-struct");
        assert!(
            Command::new("cp").args(["-R", fixture, ws_dir.to_str().unwrap()]).status().unwrap().success(),
            "copy fixture",
        );
        std::fs::remove_file(ws_dir.join("db/schema.rb")).unwrap();
        std::fs::write(
            ws_dir.join("db/structure.sql"),
            "CREATE TABLE public.widgets (id bigint PRIMARY KEY, name character varying);\n",
        )
        .unwrap();

        let mise = HashMap::new();
        let ctx = ProvisionCtx {
            project_dir: &ws_dir,
            project_name: "railsfix",
            ws_id: "ws-struct",
            ws_dir: &ws_dir,
            mise_vars: &mise,
        };
        let setup = Rails.setup(&ctx).unwrap();
        let out = Command::new("psql").args(["-tAc", "select to_regclass('public.widgets')", &name]).output().unwrap();
        let table = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Resource::PostgresDb { name: name.clone() }.teardown();

        assert!(setup.failed_steps.is_empty(), "structure.sql load should succeed: {:?}", setup.failed_steps);
        assert_eq!(setup.resources, vec![Resource::PostgresDb { name: name.clone() }]);
        assert_eq!(table, "widgets", "db:schema:load with SCHEMA_FORMAT=sql should have created the widgets table");
    }

    /// Proves what a Postgres Rails workspace's commands actually resolve to:
    /// `Setup::database_url` writes `DATABASE_URL` only to `.env.test.local`
    /// (`session_env` is empty) and leaves `config/database.yml` untouched, so
    /// `RAILS_ENV=development` (`rails server`, `rails db:migrate`) still
    /// resolves `ActiveRecord::Base.connection_db_config.database` to the
    /// primary `development:` database from `config/database.yml`, while
    /// `RAILS_ENV=test` without `dotenv-rails` resolves to the primary `test:`
    /// database unless `.env.test.local` is loaded into `ENV["DATABASE_URL"]`.
    #[test]
    fn rails_postgres_workspace_leaves_development_pointing_at_primary_db() {
        use crate::provision::Resource;
        use std::process::Command;

        let url = "postgresql://localhost/railsfix_ws_dev_share_test".to_string();
        let setup = Setup::database_url(
            vec![Resource::PostgresDb { name: "railsfix_ws_dev_share_test".into() }],
            url.clone(),
            Vec::new(),
        );
        assert!(setup.session_env.is_empty(), "DATABASE_URL must not be injected into session_env");
        assert_eq!(setup.env_file, None, "defaults to .env.test.local");

        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/rails");
        let bundle_ready = Command::new("bundle")
            .arg("check")
            .current_dir(fixture)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !bundle_ready {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let ws_dir = tmp.path().join("railsfix-ws-dev-share");
        assert!(
            Command::new("cp").args(["-R", fixture, ws_dir.to_str().unwrap()]).status().unwrap().success(),
            "copy fixture",
        );
        let yml = "default: &default\n  adapter: postgresql\n  pool: 5\n\ndevelopment:\n  <<: *default\n  database: primary_app_development\n\ntest:\n  <<: *default\n  database: primary_app_test\n";
        std::fs::write(ws_dir.join("config/database.yml"), yml).unwrap();
        std::fs::write(ws_dir.join(".env.test.local"), format!("DATABASE_URL={url}\n")).unwrap();

        let resolve_db = |rails_env: &str, db_url: Option<&str>| -> String {
            let mut cmd = Command::new("bundle");
            cmd.args([
                "exec",
                "rails",
                "runner",
                "puts ActiveRecord::Base.configurations.configs_for(env_name: Rails.env, name: 'primary').database",
            ])
            .env("RAILS_ENV", rails_env)
            .env_remove("DATABASE_URL")
            .current_dir(&ws_dir);
            if let Some(u) = db_url {
                cmd.env("DATABASE_URL", u);
            }
            let out = cmd.output().unwrap();
            assert!(out.status.success(), "rails runner failed: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };

        assert_eq!(
            resolve_db("development", None),
            "primary_app_development",
            "rails server / db:migrate (RAILS_ENV=development) touches the primary development DB"
        );
        assert_eq!(
            resolve_db("test", None),
            "primary_app_test",
            "without dotenv-rails loading .env.test.local, RAILS_ENV=test stays on database.yml's test DB"
        );
        assert_eq!(
            resolve_db("test", Some(&url)),
            "railsfix_ws_dev_share_test",
            "when .env.test.local is loaded into DATABASE_URL under RAILS_ENV=test, Rails targets the isolated test DB"
        );
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
