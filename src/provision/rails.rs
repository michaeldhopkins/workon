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

use super::{test_db_name, DbEngine, ProvisionCtx, Provisioner, Setup};

pub struct Rails;

impl Provisioner for Rails {
    fn name(&self) -> &'static str {
        "rails"
    }

    fn detect(&self, ws_dir: &Path) -> bool {
        ws_dir.join("config/database.yml").is_file()
    }

    fn setup(&self, ctx: &ProvisionCtx<'_>) -> Result<Setup> {
        let engine = DbEngine::Postgres;
        let db = test_db_name(ctx.project_name, ctx.ws_id);

        eprintln!("Creating test database {db}...");
        if engine.create(&db).is_err() {
            eprintln!("Warning: could not create test database {db}");
            return Ok(Setup::default());
        }

        // The DB exists now — record it (resource-before-risk) so teardown drops
        // it even if the schema load below fails.
        let resource = engine.resource(&db);
        let url = engine.url(&db);

        eprintln!("Loading schema...");
        let mut cmd = Cmd::new("bundle")
            .args(["exec", "rails", "db:schema:load"])
            .env("RAILS_ENV", "test")
            .env("DATABASE_URL", &url)
            .in_dir(ctx.ws_dir);
        for (k, v) in ctx.mise_vars {
            cmd = cmd.env(k, v);
        }
        let _ = cmd.run();

        Ok(Setup {
            resources: vec![resource],
            env: vec![("DATABASE_URL".to_string(), url)],
            env_file: None, // .env.test.local — dotenv-rails loads it under RAILS_ENV=test
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        let out = Command::new("psql")
            .args(["-tAc", "select to_regclass('public.widgets')", &name])
            .output()
            .unwrap();
        let table = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Resource::PostgresDb { name: name.clone() }.teardown(); // clean up before asserting

        assert_eq!(table, "widgets", "db:schema:load should have created the widgets table");
    }
}
