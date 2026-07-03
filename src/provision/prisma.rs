//! Prisma provisioner (Node/TypeScript): an isolated database per workspace with
//! the committed migrations applied and the client regenerated.
//!
//! The DB URL comes from `env("DATABASE_URL")` in `schema.prisma`, so isolation
//! is delivered by writing that var to `.env.test.local` (Prisma auto-loads only
//! `.env`, so dev commands keep their own URL). Provisioning runs the CLI with
//! the isolated URL in the command env. `sqlite` datasources are file-based and
//! self-isolating, so they're a no-op. Only `postgresql` is handled today;
//! `mysql` is recognized and skipped until the engine lands.

use std::path::Path;

use anyhow::Result;
use vcs_runner::Cmd;

use super::{test_db_name, DbEngine, ProvisionCtx, Provisioner, Setup};

pub struct Prisma;

impl Provisioner for Prisma {
    fn name(&self) -> &'static str {
        "prisma"
    }

    fn detect(&self, ws_dir: &Path) -> bool {
        ws_dir.join("prisma/schema.prisma").is_file()
    }

    fn setup(&self, ctx: &ProvisionCtx<'_>) -> Result<Setup> {
        let schema = std::fs::read_to_string(ctx.ws_dir.join("prisma/schema.prisma"))?;
        match datasource_provider(&schema).as_deref() {
            Some("postgresql") => {}
            Some("sqlite") | None => return Ok(Setup::default()), // file-based / self-managed
            Some(other) => {
                eprintln!("Prisma provider `{other}` not supported yet; skipping DB setup");
                return Ok(Setup::default());
            }
        }
        let env_var = url_env_var(&schema).unwrap_or_else(|| "DATABASE_URL".to_string());

        let engine = DbEngine::Postgres;
        let db = test_db_name(ctx.project_name, ctx.ws_id);
        eprintln!("Creating test database {db}...");
        if engine.create(&db).is_err() {
            eprintln!("Warning: could not create test database {db}");
            return Ok(Setup::default());
        }
        let resource = engine.resource(&db);
        let url = engine.url(&db);

        // `migrate deploy` applies committed migrations (creates the DB if
        // needed, non-interactive); `db push` for a migration-less schema.
        let has_migrations = ctx.ws_dir.join("prisma/migrations").is_dir();
        let mut apply = if has_migrations {
            Cmd::new("npx").args(["prisma", "migrate", "deploy"])
        } else {
            Cmd::new("npx").args(["prisma", "db", "push", "--skip-generate", "--accept-data-loss"])
        };
        apply = apply.env(&env_var, &url).in_dir(ctx.ws_dir);
        for (k, v) in ctx.mise_vars {
            apply = apply.env(k, v);
        }
        let _ = apply.run();

        // Regenerate the client: the copied one can be stale or built for another
        // platform target.
        let mut generate = Cmd::new("npx").args(["prisma", "generate"]).env(&env_var, &url).in_dir(ctx.ws_dir);
        for (k, v) in ctx.mise_vars {
            generate = generate.env(k, v);
        }
        let _ = generate.run();

        Ok(Setup { resources: vec![resource], env: vec![(env_var, url)] })
    }
}

/// The `provider` inside the `datasource` block (postgresql/mysql/sqlite/…),
/// ignoring the `generator`'s `provider = "prisma-client-js"`.
fn datasource_provider(schema: &str) -> Option<String> {
    const DB_PROVIDERS: &[&str] = &["postgresql", "mysql", "sqlite", "sqlserver", "cockroachdb", "mongodb"];
    for line in schema.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("provider") {
            let value = rest.trim_start_matches(['=', ' ']).trim().trim_matches('"');
            if DB_PROVIDERS.contains(&value) {
                return Some(value.to_string());
            }
        }
    }
    None
}

/// The env var name from `url = env("NAME")` (usually `DATABASE_URL`).
fn url_env_var(schema: &str) -> Option<String> {
    let after = schema.split("env(").nth(1)?;
    let inner = after.split(')').next()?;
    Some(inner.trim().trim_matches('"').to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_schema_prisma() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!Prisma.detect(tmp.path()));
        std::fs::create_dir(tmp.path().join("prisma")).unwrap();
        std::fs::write(tmp.path().join("prisma/schema.prisma"), "datasource db {}\n").unwrap();
        assert!(Prisma.detect(tmp.path()));
    }

    #[test]
    fn parses_provider_and_env_var_ignoring_the_generator() {
        let schema = r#"
            generator client { provider = "prisma-client-js" }
            datasource db {
              provider = "postgresql"
              url      = env("DATABASE_URL")
            }
        "#;
        assert_eq!(datasource_provider(schema).as_deref(), Some("postgresql"));
        assert_eq!(url_env_var(schema).as_deref(), Some("DATABASE_URL"));
    }

    #[test]
    fn sqlite_datasource_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("prisma")).unwrap();
        std::fs::write(
            tmp.path().join("prisma/schema.prisma"),
            "datasource db {\n provider = \"sqlite\"\n url = env(\"DATABASE_URL\")\n}\n",
        )
        .unwrap();
        let mise = std::collections::HashMap::new();
        let ctx = ProvisionCtx {
            project_dir: tmp.path(),
            project_name: "p",
            ws_id: "ws-x",
            ws_dir: tmp.path(),
            mise_vars: &mise,
        };
        let setup = Prisma.setup(&ctx).unwrap();
        assert!(setup.resources.is_empty() && setup.env.is_empty(), "sqlite -> no-op");
    }

    /// Full cycle: provision the fixture and assert `migrate deploy` created the
    /// `Widget` table in the isolated DB. Gated on a reachable Postgres AND the
    /// fixture's node_modules being installed (CI runs `npm install`).
    #[test]
    fn prisma_cycle_applies_migrations_to_isolated_db() {
        use crate::provision::Resource;
        use std::collections::HashMap;
        use std::process::Command;

        let name = test_db_name("prismafix", "ws-cycle");
        Resource::PostgresDb { name: name.clone() }.teardown();
        if DbEngine::Postgres.create(&name).is_err() {
            return; // no reachable Postgres
        }
        Resource::PostgresDb { name: name.clone() }.teardown();

        let fixture = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/prisma");
        if !Path::new(fixture).join("node_modules/.bin/prisma").exists() {
            return; // deps not installed
        }

        let tmp = tempfile::tempdir().unwrap();
        let ws_dir = tmp.path().join("prismafix-ws-cycle");
        assert!(Command::new("cp").args(["-R", fixture, ws_dir.to_str().unwrap()]).status().unwrap().success());

        let mise = HashMap::new();
        let ctx = ProvisionCtx {
            project_dir: &ws_dir,
            project_name: "prismafix",
            ws_id: "ws-cycle",
            ws_dir: &ws_dir,
            mise_vars: &mise,
        };
        let setup = Prisma.setup(&ctx).unwrap();
        assert_eq!(setup.resources, vec![Resource::PostgresDb { name: name.clone() }]);

        let out = Command::new("psql")
            .args(["-tAc", "select to_regclass('public.\"Widget\"')", &name])
            .output()
            .unwrap();
        let table = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Resource::PostgresDb { name: name.clone() }.teardown();
        assert!(table.contains("Widget"), "migrate deploy should have created the Widget table, got {table:?}");
    }
}
