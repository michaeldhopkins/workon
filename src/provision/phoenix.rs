//! Phoenix / Ecto provisioner (Elixir): an isolated Postgres test database per
//! workspace, created and migrated.
//!
//! Phoenix's generated test config names the DB `#{app}_test#{MIX_TEST_PARTITION}`,
//! so a per-workspace `MIX_TEST_PARTITION` isolates it with zero file edits.
//! workon runs `MIX_ENV=test mix ecto.create && ecto.migrate` with that partition
//! so the DB exists at provision time, and records the resulting name for
//! teardown.
//!
//! For the user's own `mix test` to reach the same DB, `MIX_TEST_PARTITION` has
//! to be set when they run it, so it's returned as `Setup.session_env` and
//! injected into the workspace session on attach (safe session-wide — it's read
//! only under `MIX_ENV=test`). Detection keys on `:ecto_sql` (a `--no-ecto`
//! Phoenix app has no DB).

use std::path::Path;

use anyhow::Result;
use vcs_runner::Cmd;

use super::{DbEngine, ProvisionCtx, Provisioner, Setup};

pub struct Phoenix;

impl Provisioner for Phoenix {
    fn name(&self) -> &'static str {
        "phoenix"
    }

    fn detect(&self, ws_dir: &Path) -> bool {
        std::fs::read_to_string(ws_dir.join("mix.exs")).map(|s| s.contains(":ecto_sql")).unwrap_or(false)
    }

    fn setup(&self, ctx: &ProvisionCtx<'_>) -> Result<Setup> {
        let mix = std::fs::read_to_string(ctx.ws_dir.join("mix.exs")).unwrap_or_default();
        let Some(app) = app_name(&mix) else {
            eprintln!("Phoenix: could not read the app name from mix.exs; skipping");
            return Ok(Setup::default());
        };

        let partition = partition_for(&app, ctx.ws_id);
        let db = format!("{app}_test{partition}");

        eprintln!("Creating test database {db} (MIX_TEST_PARTITION={partition})...");
        let mut create = Cmd::new("mix")
            .args(["ecto.create", "--quiet"])
            .env("MIX_ENV", "test")
            .env("MIX_TEST_PARTITION", &partition)
            .in_dir(ctx.ws_dir);
        for (k, v) in ctx.mise_vars {
            create = create.env(k, v);
        }
        if create.run().is_err() {
            eprintln!("Warning: mix ecto.create failed for {db}");
            return Ok(Setup::default());
        }
        // The DB exists now — record it before migrating (resource-before-risk).
        let resource = DbEngine::Postgres.resource(&db);

        let mut migrate = Cmd::new("mix")
            .args(["ecto.migrate", "--quiet"])
            .env("MIX_ENV", "test")
            .env("MIX_TEST_PARTITION", &partition)
            .in_dir(ctx.ws_dir);
        for (k, v) in ctx.mise_vars {
            migrate = migrate.env(k, v);
        }
        let _ = migrate.run();

        // Session env, not a file: MIX_TEST_PARTITION must be set when the user
        // runs `mix test` in the workspace, and it's safe session-wide because it
        // only takes effect under MIX_ENV=test.
        Ok(Setup {
            resources: vec![resource],
            session_env: vec![("MIX_TEST_PARTITION".to_string(), partition)],
            ..Setup::default()
        })
    }
}

/// The `MIX_TEST_PARTITION` value for this workspace. Phoenix's config forms the
/// DB name as `{app}_test{partition}`, so the partition must be identifier-safe
/// and short enough that the whole stays within Postgres's 63-byte limit —
/// otherwise Postgres silently truncates the created name and the (full) name we
/// record for teardown no longer matches it, breaking isolation. workon owns the
/// partition, so capping it keeps the created and recorded names identical.
fn partition_for(app: &str, ws_id: &str) -> String {
    let sanitized: String =
        ws_id.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect();
    let raw = format!("_{sanitized}");
    let budget = 63usize.saturating_sub(app.len() + "_test".len());
    if raw.len() > budget {
        raw[..budget].to_string() // sanitized to ASCII, so a byte slice is char-safe
    } else {
        raw
    }
}

/// The OTP app atom from `mix.exs` (`app: :my_app` -> `my_app`).
fn app_name(mix: &str) -> Option<String> {
    let after = mix.split("app:").nth(1)?;
    let atom = after.trim_start().trim_start_matches(':');
    let end = atom.find(|c: char| !c.is_ascii_alphanumeric() && c != '_')?;
    let name = &atom[..end];
    (!name.is_empty()).then(|| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_ecto_sql_in_mix_exs() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(!Phoenix.detect(tmp.path()));
        std::fs::write(tmp.path().join("mix.exs"), "defp deps do [{:phoenix, \"~> 1.7\"}] end").unwrap();
        assert!(!Phoenix.detect(tmp.path()), "phoenix without ecto -> not detected");
        std::fs::write(tmp.path().join("mix.exs"), "defp deps do [{:ecto_sql, \"~> 3.10\"}] end").unwrap();
        assert!(Phoenix.detect(tmp.path()));
    }

    #[test]
    fn partition_keeps_db_name_within_63_bytes() {
        // Short names pass through unchanged (and match the cycle test's DB).
        assert_eq!(partition_for("phx_fixture", "ws-cycle"), "_ws_cycle");
        // A long umbrella app + branch-slug ws_id would overflow 63 bytes.
        let app = "my_umbrella_application_service";
        let ws = "feature-add-new-billing-integration-flow-and-then-some-more";
        let db = format!("{app}_test{}", partition_for(app, ws));
        assert!(db.len() <= 63, "len {}: {db}", db.len());
    }

    #[test]
    fn app_name_reads_the_otp_atom() {
        assert_eq!(app_name("def project do\n[app: :my_app,\n version: \"0.1.0\"]\nend").as_deref(), Some("my_app"));
        assert_eq!(app_name("no app here"), None);
    }

    /// Full cycle against the real fixture: provision it and assert
    /// `mix ecto.create/migrate` created the `widgets` table in the isolated
    /// partitioned DB. Gated on Postgres AND the fixture being fetched/compiled
    /// (deps/ present) AND `mix` on PATH.
    #[test]
    fn phoenix_cycle_creates_and_migrates_isolated_db() {
        use crate::provision::Resource;
        use std::collections::HashMap;
        use std::process::Command;

        if !vcs_runner::binary_available("mix") {
            return;
        }
        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/phoenix"));
        if !fixture.join("deps").exists() {
            return; // mix deps.get not run
        }
        let db = "phx_fixture_test_ws_cycle".to_string();
        Resource::PostgresDb { name: db.clone() }.teardown();

        let mise = HashMap::new();
        let ctx = ProvisionCtx {
            project_dir: fixture,
            project_name: "phxfix",
            ws_id: "ws-cycle",
            ws_dir: fixture,
            mise_vars: &mise,
        };
        let setup = Phoenix.setup(&ctx).unwrap();
        // ws_id "ws-cycle" -> partition "_ws_cycle" -> db "phx_fixture_test_ws_cycle".
        assert_eq!(setup.resources, vec![Resource::PostgresDb { name: db.clone() }]);
        // The partition is delivered via the session env (so `mix test` reads it),
        // never a dotenv file.
        assert_eq!(setup.session_env, vec![("MIX_TEST_PARTITION".to_string(), "_ws_cycle".to_string())]);
        assert!(setup.env.is_empty(), "Phoenix uses session_env, not the dotenv file");

        let out = Command::new("psql").args(["-tAc", "select to_regclass('public.widgets')", &db]).output().unwrap();
        let table = String::from_utf8_lossy(&out.stdout).trim().to_string();
        Resource::PostgresDb { name: db.clone() }.teardown();
        assert_eq!(table, "widgets", "ecto.migrate should have created the widgets table");
    }
}
