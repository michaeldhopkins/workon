//! Per-workspace provisioners: modular setup that copying gitignored files can't
//! provide (isolated test databases today; more per ecosystem later). Each
//! provisioner detects its project type and does only the irreducible work,
//! recording any external resource it created so teardown can undo it. Mirrors
//! the `vcs` trait + backend registry. See specs/workspace-provisioners.md.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use vcs_runner::Cmd;

mod rails;

pub trait Provisioner: Send + Sync {
    fn name(&self) -> &'static str;

    /// Detect from the (already-copied) worktree. May read a config file to
    /// decide (e.g. a sqlite datasource means there's no server DB to manage).
    fn detect(&self, ws_dir: &Path) -> bool;

    /// Do the irreducible setup. Returns resources to tear down and env to write
    /// to the workspace's generated env file. An empty `Setup` is a valid,
    /// intentional no-op (e.g. a framework that manages its own test DB).
    fn setup(&self, ctx: &ProvisionCtx<'_>) -> Result<Setup>;
}

pub struct ProvisionCtx<'a> {
    /// Source repo — the old path venv/editable repair rewrites (future).
    pub project_dir: &'a Path,
    pub project_name: &'a str,
    pub ws_id: &'a str,
    pub ws_dir: &'a Path,
    pub mise_vars: &'a HashMap<String, String>,
}

#[derive(Default)]
pub struct Setup {
    /// External resources teardown must undo (empty when framework-managed).
    pub resources: Vec<Resource>,
    /// Vars written to the workspace's generated env file (`.env.test.local`),
    /// which frameworks load only in their test environment.
    pub env: Vec<(String, String)>,
}

/// Something a provisioner created that teardown must undo. Serialized into
/// `.workon.json` so a fresh-process `destroy` can undo it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Resource {
    PostgresDb { name: String },
    // MysqlDb / others land with the provisioners that create them.
}

impl Resource {
    pub fn teardown(&self) {
        match self {
            Resource::PostgresDb { name } => {
                let _ = Cmd::new("dropdb").arg(name).run();
                eprintln!("Dropped test database {name}");
            }
        }
    }

    /// The DB name, for reporting (`destroy --json`).
    pub fn db_name(&self) -> &str {
        match self {
            Resource::PostgresDb { name } => name,
        }
    }
}

/// A test-database engine. Only Postgres today; MySQL/SQLite land with the
/// provisioners that need them.
pub enum DbEngine {
    Postgres,
}

impl DbEngine {
    pub fn create(&self, name: &str) -> Result<()> {
        match self {
            DbEngine::Postgres => {
                if Cmd::new("createdb").arg(name).run().is_ok() {
                    Ok(())
                } else {
                    bail!("createdb {name} failed (is the Postgres server running?)")
                }
            }
        }
    }

    pub fn url(&self, name: &str) -> String {
        match self {
            DbEngine::Postgres => format!("postgresql://localhost/{name}"),
        }
    }

    pub fn resource(&self, name: &str) -> Resource {
        match self {
            DbEngine::Postgres => Resource::PostgresDb { name: name.to_string() },
        }
    }
}

/// A collision-free test DB name that stays within Postgres's 63-byte identifier
/// limit. The random `ws_id` (which carries the uniqueness) and the `_test`
/// suffix are always kept; the project name is truncated if the whole would
/// overflow. Matches today's `{project}_{ws_id}_test` for normal-length names.
pub fn test_db_name(project_name: &str, ws_id: &str) -> String {
    let sanitize = |s: &str| {
        s.chars()
            .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
            .collect::<String>()
    };
    let suffix = format!("_{}_test", sanitize(ws_id));
    let proj = sanitize(project_name);
    let max_proj = 63usize.saturating_sub(suffix.len());
    let proj = if proj.len() > max_proj { &proj[..max_proj] } else { proj.as_str() };
    format!("{proj}{suffix}")
}

/// The ordered provisioner registry. Order matters (future: venv repair before
/// Python DB frameworks).
pub fn provisioners() -> Vec<Box<dyn Provisioner>> {
    vec![Box::new(rails::Rails)]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_db_name_matches_legacy_for_normal_names() {
        assert_eq!(test_db_name("mbc", "ws-abc123"), "mbc_ws_abc123_test");
        assert_eq!(test_db_name("my-app", "ws-abc123-fix-bug"), "my_app_ws_abc123_fix_bug_test");
    }

    #[test]
    fn test_db_name_stays_within_63_bytes() {
        let long = "a".repeat(200);
        let name = test_db_name(&long, "ws-abc123");
        assert!(name.len() <= 63, "len {} > 63: {name}", name.len());
        // The unique ws_id + suffix survive; the project is what gets trimmed.
        assert!(name.ends_with("_ws_abc123_test"));
    }

    #[test]
    fn resource_round_trips_through_json() {
        let r = Resource::PostgresDb { name: "mbc_ws_abc_test".into() };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"type":"postgres_db","name":"mbc_ws_abc_test"}"#);
        assert_eq!(serde_json::from_str::<Resource>(&json).unwrap(), r);
    }

    /// Real create + drop against Postgres. Skips (does not fail) when no server
    /// is reachable — like the jj tests gate on `jj_available()`. Runs in CI,
    /// which stands up a Postgres service.
    #[test]
    fn postgres_create_and_drop_roundtrip() {
        let name = "workon_provision_selftest_db";
        // Clean any leftover from a previous aborted run, then try to create.
        Resource::PostgresDb { name: name.into() }.teardown();
        if DbEngine::Postgres.create(name).is_err() {
            return; // no reachable Postgres server
        }
        // Drop it; a fresh create must then succeed — proving the drop worked.
        Resource::PostgresDb { name: name.into() }.teardown();
        let recreated = DbEngine::Postgres.create(name).is_ok();
        Resource::PostgresDb { name: name.into() }.teardown(); // final cleanup
        assert!(recreated, "dropdb should have removed the DB so createdb succeeds again");
    }
}
