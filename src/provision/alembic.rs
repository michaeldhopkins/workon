//! Alembic provisioner (SQLAlchemy — Flask/FastAPI): an isolated Postgres test
//! database per workspace with migrations applied.
//!
//! Alembic reads no env var by default, so isolation only works when the
//! project's `env.py` reads `DATABASE_URL` (the idiomatic pattern) — workon runs
//! `alembic upgrade head` with the isolated URL in the command env and writes it
//! to `.env.test.local`. A sqlite `sqlalchemy.url` is file-based/self-managed, so
//! it's a no-op. Migrations run through the workspace's `.venv` interpreter.

use std::path::Path;

use anyhow::Result;
use vcs_runner::Cmd;

use super::{test_db_name, venv_python, DbEngine, ProvisionCtx, Provisioner, Setup};

pub struct Alembic;

impl Provisioner for Alembic {
    fn name(&self) -> &'static str {
        "alembic"
    }

    fn detect(&self, ws_dir: &Path) -> bool {
        ws_dir.join("alembic.ini").is_file()
    }

    fn setup(&self, ctx: &ProvisionCtx<'_>) -> Result<Setup> {
        let ini = std::fs::read_to_string(ctx.ws_dir.join("alembic.ini")).unwrap_or_default();
        if configured_url(&ini).is_some_and(|u| u.starts_with("sqlite")) {
            return Ok(Setup::default()); // file-based / self-managed
        }

        let engine = DbEngine::Postgres;
        let db = test_db_name(ctx.project_name, ctx.ws_id);
        eprintln!("Creating test database {db}...");
        if engine.create(&db).is_err() {
            eprintln!("Warning: could not create test database {db}");
            return Ok(Setup::default());
        }
        let resource = engine.resource(&db);
        let url = engine.url(&db);

        eprintln!("Applying migrations (alembic upgrade head)...");
        let mut cmd = Cmd::new(venv_python(ctx.ws_dir))
            .args(["-m", "alembic", "upgrade", "head"])
            .env("DATABASE_URL", &url)
            .in_dir(ctx.ws_dir);
        for (k, v) in ctx.mise_vars {
            cmd = cmd.env(k, v);
        }
        let _ = cmd.run();

        Ok(Setup { resources: vec![resource], env: vec![("DATABASE_URL".to_string(), url)], env_file: None })
    }
}

/// The `sqlalchemy.url` value from `alembic.ini`, if non-empty. Blank means the
/// project drives it from env (`env.py`), which we treat as "manage it".
fn configured_url(ini: &str) -> Option<String> {
    ini.lines().find_map(|l| {
        let v = l.trim().strip_prefix("sqlalchemy.url")?.trim_start_matches(['=', ' ']).trim();
        (!v.is_empty()).then(|| v.to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_alembic_ini() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!Alembic.detect(tmp.path()));
        std::fs::write(tmp.path().join("alembic.ini"), "[alembic]\n").unwrap();
        assert!(Alembic.detect(tmp.path()));
    }

    #[test]
    fn configured_url_reads_non_blank() {
        assert_eq!(configured_url("[alembic]\nsqlalchemy.url =\n"), None);
        assert_eq!(configured_url("sqlalchemy.url = sqlite:///x.db").as_deref(), Some("sqlite:///x.db"));
    }

    /// Full cycle against the real fixture (its `.venv` has alembic). Provision
    /// it and assert `upgrade head` created the `widgets` table in the isolated
    /// DB. Gated on a reachable Postgres AND the fixture venv being present.
    #[test]
    fn alembic_cycle_applies_migrations_to_isolated_db() {
        use crate::provision::Resource;
        use std::collections::HashMap;
        use std::process::Command;

        let name = test_db_name("alembicfix", "ws-cycle");
        Resource::PostgresDb { name: name.clone() }.teardown();
        if DbEngine::Postgres.create(&name).is_err() {
            return;
        }
        Resource::PostgresDb { name: name.clone() }.teardown();

        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/alembic"));
        if !fixture.join(".venv/bin/python").exists() {
            return; // fixture venv not set up
        }

        // Alembic touches only the DB, not fixture files, so run in place.
        let mise = HashMap::new();
        let ctx = ProvisionCtx {
            project_dir: fixture,
            project_name: "alembicfix",
            ws_id: "ws-cycle",
            ws_dir: fixture,
            mise_vars: &mise,
        };
        let setup = Alembic.setup(&ctx).unwrap();
        assert_eq!(setup.resources, vec![Resource::PostgresDb { name: name.clone() }]);

        let out = Command::new("psql").args(["-tAc", "select to_regclass('public.widgets')", &name]).output().unwrap();
        let table = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Resource::PostgresDb { name: name.clone() }.teardown();
        assert_eq!(table, "widgets", "alembic upgrade should have created the widgets table");
    }
}
