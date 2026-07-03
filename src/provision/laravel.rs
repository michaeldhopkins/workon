//! Laravel provisioner (PHP). Laravel's default test posture is sqlite
//! `:memory:` with `RefreshDatabase` (self-managed) — a no-op. When `phpunit.xml`
//! configures a real Postgres connection, workon creates an isolated DB, applies
//! migrations, and writes the connection to `.env.testing` (the file Laravel
//! loads under `APP_ENV=testing`). MySQL is recognized but not yet handled.
//!
//! Laravel reads `DB_URL` from `config/database.php`, so a single URL isolates
//! it; `APP_KEY` rides in via the copied `.env`.

use std::path::Path;

use anyhow::Result;
use vcs_runner::Cmd;

use super::{test_db_name, DbEngine, ProvisionCtx, Provisioner, Setup};

pub struct Laravel;

impl Provisioner for Laravel {
    fn name(&self) -> &'static str {
        "laravel"
    }

    fn detect(&self, ws_dir: &Path) -> bool {
        ws_dir.join("artisan").is_file()
    }

    fn setup(&self, ctx: &ProvisionCtx<'_>) -> Result<Setup> {
        let xml = std::fs::read_to_string(ctx.ws_dir.join("phpunit.xml")).unwrap_or_default();
        let connection = phpunit_db_connection(&xml);
        let engine = match connection.as_deref() {
            Some("pgsql") => DbEngine::Postgres,
            Some("mysql") | Some("mariadb") => DbEngine::Mysql,
            _ => return Ok(Setup::default()), // sqlite / :memory: / unset -> self-managed
        };
        let connection = connection.unwrap();

        let db = test_db_name(ctx.project_name, ctx.ws_id);
        eprintln!("Creating test database {db}...");
        if engine.create(&db).is_err() {
            eprintln!("Warning: could not create test database {db}");
            return Ok(Setup::default());
        }
        let resource = engine.resource(&db);
        let url = engine.url(&db);

        eprintln!("Migrating (php artisan migrate)...");
        let mut cmd = Cmd::new("php")
            .args(["artisan", "migrate", "--force"])
            .env("DB_CONNECTION", &connection)
            .env("DB_URL", &url)
            .in_dir(ctx.ws_dir);
        for (k, v) in ctx.mise_vars {
            cmd = cmd.env(k, v);
        }
        let _ = cmd.run();

        Ok(Setup {
            resources: vec![resource],
            env: vec![("DB_CONNECTION".to_string(), connection), ("DB_URL".to_string(), url)],
            env_file: Some(".env.testing".to_string()),
        })
    }
}

/// The `DB_CONNECTION` test value from `phpunit.xml`
/// (`<env name="DB_CONNECTION" value="X"/>`).
fn phpunit_db_connection(xml: &str) -> Option<String> {
    let line = xml.lines().find(|l| l.contains("DB_CONNECTION"))?;
    let value = line.split("value=").nth(1)?.trim().trim_start_matches('"');
    value.split('"').next().map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_artisan() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!Laravel.detect(tmp.path()));
        std::fs::write(tmp.path().join("artisan"), "#!/usr/bin/env php\n").unwrap();
        assert!(Laravel.detect(tmp.path()));
    }

    #[test]
    fn phpunit_db_connection_parsing() {
        assert_eq!(
            phpunit_db_connection("<env name=\"DB_CONNECTION\" value=\"pgsql\"/>").as_deref(),
            Some("pgsql")
        );
        assert_eq!(phpunit_db_connection("<phpunit></phpunit>"), None);
    }

    #[test]
    fn sqlite_default_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("artisan"), "#!/usr/bin/env php\n").unwrap();
        std::fs::write(tmp.path().join("phpunit.xml"), "<env name=\"DB_CONNECTION\" value=\"sqlite\"/>").unwrap();
        let mise = std::collections::HashMap::new();
        let ctx = ProvisionCtx {
            project_dir: tmp.path(),
            project_name: "p",
            ws_id: "ws-x",
            ws_dir: tmp.path(),
            mise_vars: &mise,
        };
        assert!(Laravel.setup(&ctx).unwrap().resources.is_empty());
    }

    /// Full cycle against the real Laravel fixture (configured for pgsql):
    /// provision it and assert `artisan migrate` created the `migrations` table
    /// in the isolated DB. Gated on Postgres AND the fixture's vendor + .env.
    #[test]
    fn laravel_cycle_migrates_isolated_db() {
        use crate::provision::Resource;
        use std::collections::HashMap;
        use std::process::Command;

        let name = test_db_name("laravelfix", "ws-cycle");
        Resource::PostgresDb { name: name.clone() }.teardown();
        if DbEngine::Postgres.create(&name).is_err() {
            return;
        }
        Resource::PostgresDb { name: name.clone() }.teardown();

        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/laravel"));
        if !fixture.join("vendor/autoload.php").exists() || !fixture.join(".env").exists() {
            return; // composer install / key:generate not done
        }

        let mise = HashMap::new();
        let ctx = ProvisionCtx {
            project_dir: fixture,
            project_name: "laravelfix",
            ws_id: "ws-cycle",
            ws_dir: fixture,
            mise_vars: &mise,
        };
        let setup = Laravel.setup(&ctx).unwrap();
        assert_eq!(setup.resources, vec![Resource::PostgresDb { name: name.clone() }]);
        assert_eq!(setup.env_file.as_deref(), Some(".env.testing"));

        let out = Command::new("psql").args(["-tAc", "select to_regclass('public.migrations')", &name]).output().unwrap();
        let table = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Resource::PostgresDb { name: name.clone() }.teardown();
        assert_eq!(table, "migrations", "artisan migrate should have created the migrations table");
    }
}
