//! Entity Framework Core provisioner (.NET). An isolated Postgres test database
//! per workspace with `dotnet ef database update` applied.
//!
//! Detected by a `*.csproj` referencing EF Core. Self-managing test setups
//! (`Microsoft.EntityFrameworkCore.InMemory`, a `:memory:` SQLite, or
//! `Testcontainers`) are a no-op — those own their DB. For a real provider
//! (Npgsql), workon creates the DB and runs `dotnet ef database update` with the
//! connection injected via `ConnectionStrings__<Name>` (EF reads it over
//! appsettings). EF wants a Npgsql key=value connection string, not a URL.

use std::path::{Path, PathBuf};

use anyhow::Result;
use vcs_runner::Cmd;

use super::{test_db_name, DbEngine, ProvisionCtx, Provisioner, Setup};

/// The connection-string key EF's `ConnectionStrings__<Name>` overrides. Real
/// projects vary; `Default` is the common convention.
const CONN_KEY: &str = "Default";

pub struct EfCore;

impl Provisioner for EfCore {
    fn name(&self) -> &'static str {
        "ef-core"
    }

    fn detect(&self, ws_dir: &Path) -> bool {
        csprojs(ws_dir).iter().any(|p| {
            std::fs::read_to_string(p).map(|s| s.contains("Microsoft.EntityFrameworkCore")).unwrap_or(false)
        })
    }

    fn setup(&self, ctx: &ProvisionCtx<'_>) -> Result<Setup> {
        let refs: String = csprojs(ctx.ws_dir).iter().filter_map(|p| std::fs::read_to_string(p).ok()).collect();
        // Self-managing test setups own their DB — stay out.
        if refs.contains("EntityFrameworkCore.InMemory")
            || refs.contains("EntityFrameworkCore.Sqlite")
            || refs.contains("Testcontainers")
        {
            return Ok(Setup::default());
        }
        if !refs.contains("Npgsql") {
            eprintln!("EF Core: no supported provider (Npgsql) detected; skipping DB setup");
            return Ok(Setup::default());
        }

        let engine = DbEngine::Postgres;
        let db = test_db_name(ctx.project_name, ctx.ws_id);
        eprintln!("Creating test database {db}...");
        if engine.create(&db).is_err() {
            eprintln!("Warning: could not create test database {db}");
            return Ok(Setup::default());
        }
        let resource = engine.resource(&db);
        let conn = npgsql_connection_string(&db);
        let env_key = format!("ConnectionStrings__{CONN_KEY}");

        // Restore the local dotnet-ef tool if a manifest declares one, then apply.
        let _ = Cmd::new("dotnet").arg("tool").arg("restore").in_dir(ctx.ws_dir).run();
        eprintln!("Applying migrations (dotnet ef database update)...");
        let mut cmd = Cmd::new("dotnet")
            .args(["ef", "database", "update"])
            .env(&env_key, &conn)
            .in_dir(ctx.ws_dir);
        for (k, v) in ctx.mise_vars {
            cmd = cmd.env(k, v);
        }
        let _ = cmd.run();

        Ok(Setup { resources: vec![resource], env: vec![(env_key, conn)], env_file: None })
    }
}

/// `*.csproj` files at the repo root and one level down.
fn csprojs(ws_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut dirs = vec![ws_dir.to_path_buf()];
    if let Ok(entries) = std::fs::read_dir(ws_dir) {
        for e in entries.flatten() {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                dirs.push(e.path());
            }
        }
    }
    for dir in dirs {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                if e.path().extension().is_some_and(|x| x == "csproj") {
                    out.push(e.path());
                }
            }
        }
    }
    out
}

/// A Npgsql `Host=…;Port=…;Database=…;Username=…[;Password=…]` string from the
/// `PG*` environment (OS-user fallback).
fn npgsql_connection_string(name: &str) -> String {
    let host = std::env::var("PGHOST").unwrap_or_default();
    let host = if host.is_empty() || host.starts_with('/') { "localhost".to_string() } else { host };
    let port = std::env::var("PGPORT").unwrap_or_else(|_| "5432".into());
    let user = std::env::var("PGUSER").ok().or_else(|| std::env::var("USER").ok()).unwrap_or_default();
    let mut conn = format!("Host={host};Port={port};Database={name};Username={user}");
    if let Some(pass) = std::env::var("PGPASSWORD").ok().filter(|p| !p.is_empty()) {
        conn.push_str(&format!(";Password={pass}"));
    }
    conn
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_by_efcore_csproj() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!EfCore.detect(tmp.path()));
        std::fs::write(
            tmp.path().join("app.csproj"),
            "<Project><PackageReference Include=\"Microsoft.EntityFrameworkCore.Design\"/></Project>",
        )
        .unwrap();
        assert!(EfCore.detect(tmp.path()));
    }

    #[test]
    fn inmemory_is_a_noop() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("app.csproj"),
            "<Project><PackageReference Include=\"Microsoft.EntityFrameworkCore.InMemory\"/></Project>",
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
        assert!(EfCore.setup(&ctx).unwrap().resources.is_empty());
    }

    /// Full cycle against the real EF fixture: provision it and assert
    /// `dotnet ef database update` created the `Widgets` table in the isolated
    /// DB. Gated on Postgres AND the project being restored (obj/ present).
    #[test]
    fn ef_core_cycle_updates_isolated_db() {
        use crate::provision::Resource;
        use std::collections::HashMap;
        use std::process::Command;

        let name = test_db_name("effix", "ws-cycle");
        Resource::PostgresDb { name: name.clone() }.teardown();
        if DbEngine::Postgres.create(&name).is_err() {
            return;
        }
        Resource::PostgresDb { name: name.clone() }.teardown();

        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/efcore"));
        if !fixture.join("obj").exists() {
            return; // `dotnet restore` not run
        }

        let mise = HashMap::new();
        let ctx = ProvisionCtx {
            project_dir: fixture,
            project_name: "effix",
            ws_id: "ws-cycle",
            ws_dir: fixture,
            mise_vars: &mise,
        };
        let setup = EfCore.setup(&ctx).unwrap();
        assert_eq!(setup.resources, vec![Resource::PostgresDb { name: name.clone() }]);

        let out = Command::new("psql").args(["-tAc", "select to_regclass('public.\"Widgets\"')", &name]).output().unwrap();
        let table = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Resource::PostgresDb { name: name.clone() }.teardown();
        assert!(table.contains("Widgets"), "database update should have created Widgets, got {table:?}");
    }
}
