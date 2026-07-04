//! Django provisioner (Python): framework-managed. Django's own test runner
//! creates and drops `test_<NAME>` per run, so workon does not migrate — it only
//! hands the project a per-workspace DB identity so that auto-created test DB
//! doesn't collide across workspaces.
//!
//! Isolation is only possible when the project reads `DATABASE_URL` (via
//! `dj-database-url`); otherwise the DB name is hard-coded in settings and workon
//! can't change it without editing files, so it no-ops with a note. When it can:
//! it creates the base DB (Django connects to it to `CREATE DATABASE test_<name>`)
//! and injects `DATABASE_URL`, recording both the base and `test_<name>` so
//! teardown drops whatever a `--keepdb`/interrupted run leaves behind.

use std::path::Path;

use anyhow::Result;

use super::{test_db_name, DbEngine, ProvisionCtx, Provisioner, Setup};

pub struct Django;

impl Provisioner for Django {
    fn name(&self) -> &'static str {
        "django"
    }

    fn detect(&self, ws_dir: &Path) -> bool {
        ws_dir.join("manage.py").is_file()
    }

    fn setup(&self, ctx: &ProvisionCtx<'_>) -> Result<Setup> {
        if !reads_database_url(ctx.ws_dir) {
            eprintln!(
                "Django settings don't read DATABASE_URL (no dj-database-url); can't isolate its \
                 test DB per workspace without editing settings — skipping"
            );
            return Ok(Setup::default());
        }

        let engine = DbEngine::Postgres;
        let base = test_db_name(ctx.project_name, ctx.ws_id);
        eprintln!("Creating base database {base} (Django's runner will create test_{base})...");
        if engine.create(&base).is_err() {
            eprintln!("Warning: could not create database {base}");
            return Ok(Setup::default());
        }
        let url = engine.url(&base);

        // Django prepends `test_` to the NAME for its test database. Record both
        // so teardown drops whatever survives (default runner drops test_<name>
        // itself; --keepdb or an interrupted run leaves it).
        let resources = vec![engine.resource(&base), engine.resource(&format!("test_{base}"))];
        Ok(Setup { resources, env: vec![("DATABASE_URL".to_string(), url)], ..Setup::default() })
    }
}

/// Whether a `settings.py` in the project reads `DATABASE_URL` (directly or via
/// `dj_database_url`). Checks the repo root and one level down (the usual
/// `<project>/settings.py` layout).
fn reads_database_url(ws_dir: &Path) -> bool {
    let mut candidates = vec![ws_dir.join("settings.py")];
    if let Ok(entries) = std::fs::read_dir(ws_dir) {
        for e in entries.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                candidates.push(e.path().join("settings.py"));
            }
        }
    }
    candidates.iter().any(|p| {
        std::fs::read_to_string(p)
            .map(|s| s.contains("dj_database_url") || s.contains("DATABASE_URL"))
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_manage_py() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!Django.detect(tmp.path()));
        std::fs::write(tmp.path().join("manage.py"), "# django\n").unwrap();
        assert!(Django.detect(tmp.path()));
    }

    #[test]
    fn reads_database_url_finds_it_one_level_down() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!reads_database_url(tmp.path()));
        std::fs::create_dir(tmp.path().join("proj")).unwrap();
        std::fs::write(tmp.path().join("proj/settings.py"), "import dj_database_url\n").unwrap();
        assert!(reads_database_url(tmp.path()));
    }

    #[test]
    fn no_op_without_database_url() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("manage.py"), "# django\n").unwrap();
        std::fs::create_dir(tmp.path().join("proj")).unwrap();
        std::fs::write(tmp.path().join("proj/settings.py"), "DATABASES = {'default': {'NAME': 'hard'}}\n").unwrap();
        let mise = std::collections::HashMap::new();
        let ctx = ProvisionCtx {
            project_dir: tmp.path(),
            project_name: "p",
            ws_id: "ws-x",
            ws_dir: tmp.path(),
            mise_vars: &mise,
        };
        // No DATABASE_URL reference -> nothing workon can isolate.
        assert!(Django.setup(&ctx).unwrap().resources.is_empty());
    }

    /// Full cycle: provision the fixture (creates the base DB + injects
    /// DATABASE_URL), then run Django's own test runner with `--keepdb` and
    /// assert it created the isolated `test_<base>` — i.e. the injected identity
    /// really isolates the runner's DB. Gated on Postgres + the fixture venv.
    #[test]
    fn django_cycle_isolates_the_runners_test_db() {
        use crate::provision::{venv_python, Resource};
        use std::collections::HashMap;
        use std::process::Command;

        let base = test_db_name("djangofix", "ws-cycle");
        let test_db = format!("test_{base}");
        for db in [&base, &test_db] {
            Resource::PostgresDb { name: db.clone() }.teardown();
        }
        if DbEngine::Postgres.create(&base).is_err() {
            return;
        }
        Resource::PostgresDb { name: base.clone() }.teardown();

        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/django"));
        if !fixture.join(".venv/bin/python").exists() {
            return;
        }

        let mise = HashMap::new();
        let ctx = ProvisionCtx {
            project_dir: fixture,
            project_name: "djangofix",
            ws_id: "ws-cycle",
            ws_dir: fixture,
            mise_vars: &mise,
        };
        let setup = Django.setup(&ctx).unwrap();
        let url = setup.env.iter().find(|(k, _)| k == "DATABASE_URL").map(|(_, v)| v.clone()).unwrap();

        // Run Django's runner with the injected URL; --keepdb preserves test_<base>.
        let ran = Command::new(venv_python(fixture))
            .args(["manage.py", "test", "--keepdb", "--noinput"])
            .env("DATABASE_URL", &url)
            .current_dir(fixture)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);

        let exists = Command::new("psql")
            .args(["-tAc", &format!("select datname from pg_database where datname='{test_db}'"), "postgres"])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).contains(&test_db))
            .unwrap_or(false);

        for db in [&base, &test_db] {
            Resource::PostgresDb { name: db.clone() }.teardown();
        }
        assert!(ran, "django test run should succeed against the injected DB");
        assert!(exists, "django should have created the isolated {test_db}");
    }
}
