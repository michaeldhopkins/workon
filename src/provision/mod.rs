//! Per-workspace provisioners: modular setup that copying gitignored files can't
//! provide (isolated test databases today; more per ecosystem later). Each
//! provisioner detects its project type and does only the irreducible work,
//! recording any external resource it created so teardown can undo it. Mirrors
//! the `vcs` trait + backend registry. See specs/workspace-provisioners.md.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use vcs_runner::Cmd;

pub(crate) mod alembic;
mod django;
pub(crate) mod ef_core;
pub(crate) mod laravel;
pub(crate) mod phoenix;
pub(crate) mod prisma;
mod python_venv;
pub(crate) mod rails;

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
    /// Vars to write to the workspace's generated env file, which the framework
    /// loads only in its test environment.
    pub env: Vec<(String, String)>,
    /// Vars to inject into the workspace *session* (merged with mise env on
    /// attach), for a framework whose test var is safe session-wide because it
    /// only takes effect under that framework's test env — Phoenix's
    /// `MIX_TEST_PARTITION` (read only under `MIX_ENV=test`). Distinct from `env`:
    /// a session-wide `DATABASE_URL` would wrongly point every dev command at the
    /// test DB, so URLs go in `env` (test-file-scoped) and never here.
    pub session_env: Vec<(String, String)>,
    /// Which generated env file the vars go in. `None` = `.env.test.local`
    /// (Rails/dotenv default); Laravel needs `.env.testing`, etc.
    pub env_file: Option<String>,
    /// Setup steps that ran and failed (`rails db:schema:load`, …). The resources and env
    /// above still stand, but the workspace is not ready, and `create` says so (issue #2).
    pub failed_steps: Vec<String>,
}

impl Setup {
    /// A test database reached through `DATABASE_URL` in the generated env file: what
    /// Rails, Alembic and Django return once the database exists. Kept apart from
    /// their `setup`, which needs a live server, so each field can be tested.
    pub(crate) fn database_url(resources: Vec<Resource>, url: String, failed_steps: Vec<String>) -> Self {
        Self { resources, env: vec![("DATABASE_URL".to_string(), url)], failed_steps, ..Self::default() }
    }
}

/// Something a provisioner created that teardown must undo. Serialized into
/// `.workon.json` so a fresh-process `destroy` can undo it.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Resource {
    PostgresDb { name: String },
    MysqlDb { name: String },
}

impl Resource {
    pub fn teardown(&self) {
        match self {
            Resource::PostgresDb { name } => {
                let _ = Cmd::new("dropdb").arg(name).run();
                eprintln!("Dropped test database {name}");
            }
            Resource::MysqlDb { name } => {
                let _ = mysqladmin(&["--force", "drop", name]).run();
                eprintln!("Dropped test database {name}");
            }
        }
    }

    /// The DB name, for reporting (`destroy --json`).
    pub fn db_name(&self) -> &str {
        match self {
            Resource::PostgresDb { name } | Resource::MysqlDb { name } => name,
        }
    }
}

/// A test-database engine. SQLite is never a `DbEngine` — it's a file, handled by
/// each provisioner as a no-op.
pub enum DbEngine {
    Postgres,
    Mysql,
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
            DbEngine::Mysql => {
                if mysqladmin(&["create", name]).run().is_ok() {
                    Ok(())
                } else {
                    bail!("mysqladmin create {name} failed (is the MySQL server running?)")
                }
            }
        }
    }

    /// A connection URL for the test DB, built from the client environment with
    /// OS-user fallback, because URL-driven clients (Prisma, dj-database-url, …)
    /// don't apply libpq/libmysql's implicit user default — a userless URL is
    /// rejected. `PG*` for Postgres; `MYSQL_HOST`/`MYSQL_TCP_PORT`/`MYSQL_USER`/
    /// `MYSQL_PWD` for MySQL. A socket-path host becomes `localhost`.
    pub fn url(&self, name: &str) -> String {
        self.url_in(name, &|var| std::env::var(var).ok())
    }

    /// [`Self::url`] with the variables read through `env`, so a test can supply them.
    fn url_in(&self, name: &str, env: &dyn Fn(&str) -> Option<String>) -> String {
        match self {
            DbEngine::Postgres => {
                let host = env_host(env("PGHOST"), "localhost");
                let user = env("PGUSER").or_else(|| env("USER")).unwrap_or_default();
                let auth = auth_prefix(&user, env("PGPASSWORD"));
                postgres_url(&auth, &host, env("PGPORT").as_deref(), name)
            }
            DbEngine::Mysql => {
                let host = env_host(env("MYSQL_HOST"), "127.0.0.1");
                let port = env("MYSQL_TCP_PORT").filter(|p| !p.is_empty());
                let user = mysql_user(env);
                let auth = auth_prefix(&user, env("MYSQL_PWD"));
                format!("mysql://{auth}{}/{name}", url_authority(&host, Some(port.as_deref().unwrap_or("3306"))))
            }
        }
    }

    pub fn resource(&self, name: &str) -> Resource {
        match self {
            DbEngine::Postgres => Resource::PostgresDb { name: name.to_string() },
            DbEngine::Mysql => Resource::MysqlDb { name: name.to_string() },
        }
    }
}

/// `PGPORT` is included when set: libpq's createdb honours it, and a URL
/// without it points the app at 5432 whatever server the database was made on.
pub(crate) fn postgres_url(auth: &str, host: &str, port: Option<&str>, name: &str) -> String {
    format!("postgresql://{auth}{}/{name}", url_authority(host, port))
}

/// `host[:port]` for a URL. An IPv6 literal is bracketed, or its colons would
/// read as a port.
pub(crate) fn url_authority(host: &str, port: Option<&str>) -> String {
    let host = if host.contains(':') && !host.starts_with('[') { format!("[{host}]") } else { host.to_string() };
    match port.filter(|p| !p.is_empty()) {
        Some(p) => format!("{host}:{p}"),
        None => host,
    }
}

/// Run a setup step (a migration, a client generate) whose failure does not stop
/// provisioning but must be seen: swallowed, a failed migration leaves a workspace
/// that looks provisioned and has no schema. Returns `what` when it failed, for
/// [`Setup::failed_steps`].
pub(crate) fn run_step(cmd: Cmd, what: &str) -> Option<String> {
    match cmd.run() {
        Ok(_) => None,
        Err(e) => {
            eprintln!("Warning: {what} failed: {e}");
            Some(what.to_string())
        }
    }
}

/// The last line `workon create` prints when setup steps failed, so a workspace whose test
/// database has no schema is never reported as simply created. Each failure's own output was
/// printed when it happened.
pub fn not_ready_note(failed_steps: &[String]) -> Option<String> {
    (!failed_steps.is_empty())
        .then(|| format!("Warning: the workspace is not ready; failed (output above): {}", failed_steps.join(", ")))
}

/// Print [`not_ready_note`] to stderr when there is one.
pub fn warn_if_not_ready(failed_steps: &[String]) {
    if let Some(note) = not_ready_note(failed_steps) {
        eprintln!("{note}");
    }
}

fn env_host(value: Option<String>, default: &str) -> String {
    let host = value.unwrap_or_default();
    if host.is_empty() || host.starts_with('/') {
        default.to_string()
    } else {
        host
    }
}

pub(crate) fn auth_prefix(user: &str, password: Option<String>) -> String {
    if user.is_empty() {
        // No username (PG*/MYSQL* user and $USER all unset) → no userinfo. Any
        // password is intentionally dropped: a password without a user isn't a
        // valid URL credential, and libpq/libmysql wouldn't emit one either.
        return String::new();
    }
    let user = percent_encode_userinfo(user);
    match password {
        Some(p) if !p.is_empty() => format!("{user}:{}@", percent_encode_userinfo(&p)),
        _ => format!("{user}@"),
    }
}

/// Percent-encode a URL userinfo component (username or password). A credential
/// containing `@`, `:`, `/`, `#`, `?`, `%`, … would otherwise be misparsed —
/// the `:` splits user/pass and the `@` ends the userinfo. Encodes every byte
/// outside the RFC 3986 unreserved set, which is always safe (over-encoding a
/// sub-delim just decodes back).
pub(crate) fn percent_encode_userinfo(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// MySQL clients don't read a user env var natively (unlike host/port/password),
/// so workon reads `MYSQL_USER`, falling back to the OS user then `root`.
fn mysql_user(env: &dyn Fn(&str) -> Option<String>) -> String {
    env("MYSQL_USER").or_else(|| env("USER")).unwrap_or_else(|| "root".into())
}

/// `mysqladmin` with the resolved user in front of the subcommand; host, port,
/// and password are read from the environment natively.
fn mysqladmin(args: &[&str]) -> Cmd {
    Cmd::new("mysqladmin").arg("-u").arg(mysql_user(&|var| std::env::var(var).ok())).args(args)
}

/// A collision-free test DB name that stays within Postgres's 63-byte identifier
/// limit. The random `ws_id` (which carries the uniqueness) and the `_test`
/// suffix are always kept; the project name is truncated if the whole would
/// overflow. Matches today's `{project}_{ws_id}_test` for normal-length names.
pub fn test_db_name(project_name: &str, ws_id: &str) -> String {
    let sanitize =
        |s: &str| s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' }).collect::<String>();
    let suffix = format!("_{}_test", sanitize(ws_id));
    let proj = sanitize(project_name);
    let max_proj = 63usize.saturating_sub(suffix.len());
    // `sanitize` leaves only ASCII, so a byte index is a char boundary.
    let proj = &proj[..proj.len().min(max_proj)];
    format!("{proj}{suffix}")
}

/// The Python interpreter to run project tools with: the workspace's `.venv`
/// (which `PythonVenv` has already repaired, since it runs first) if present,
/// else `python3` on `PATH`.
pub fn venv_python(ws_dir: &Path) -> PathBuf {
    let venv = ws_dir.join(".venv/bin/python");
    if venv.is_file() {
        venv
    } else {
        PathBuf::from("python3")
    }
}

/// The ordered provisioner registry. Order matters (future: venv repair before
/// Python DB frameworks).
pub fn provisioners() -> Vec<Box<dyn Provisioner>> {
    // Order matters: venv repair before any Python DB framework, which needs a
    // working interpreter.
    vec![
        Box::new(python_venv::PythonVenv),
        Box::new(rails::Rails),
        Box::new(prisma::Prisma),
        Box::new(alembic::Alembic),
        Box::new(django::Django),
        Box::new(laravel::Laravel),
        Box::new(ef_core::EfCore),
        Box::new(phoenix::Phoenix),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_step_reports_whether_the_step_succeeded() {
        assert_eq!(run_step(Cmd::new("true"), "a passing step"), None);
        assert_eq!(run_step(Cmd::new("false"), "a failing step").as_deref(), Some("a failing step"));
        assert_eq!(run_step(Cmd::new("workon-no-such-binary"), "a missing tool").as_deref(), Some("a missing tool"));
    }

    /// Each field is checked on its own, so dropping any of them from the builder fails.
    #[test]
    fn database_url_setup_carries_the_database_its_url_and_failed_steps() {
        let db = Resource::PostgresDb { name: "proj_ws_test".into() };
        let setup = Setup::database_url(
            vec![db.clone()],
            "postgresql://u@localhost/proj_ws_test".into(),
            vec!["migrate".into()],
        );

        assert_eq!(setup.resources, vec![db]);
        assert_eq!(setup.env, vec![("DATABASE_URL".to_string(), "postgresql://u@localhost/proj_ws_test".to_string())]);
        assert_eq!(setup.failed_steps, vec!["migrate".to_string()]);
        assert!(setup.session_env.is_empty(), "a URL never goes in the session env");
        assert_eq!(setup.env_file, None, "the default .env.test.local");
    }

    #[test]
    fn not_ready_note_names_every_failed_step() {
        assert_eq!(not_ready_note(&[]), None);
        assert_eq!(
            not_ready_note(&["prisma schema apply".into(), "prisma generate".into()]).as_deref(),
            Some("Warning: the workspace is not ready; failed (output above): prisma schema apply, prisma generate")
        );
    }

    #[test]
    fn postgres_url_carries_pgport_and_brackets_an_ipv6_host() {
        // PGPORT was dropped, so a server on 5433 got a URL for 5432 while createdb
        // (which reads PGPORT itself) made the database on 5433.
        assert_eq!(postgres_url("u@", "localhost", Some("5433"), "db"), "postgresql://u@localhost:5433/db");
        assert_eq!(postgres_url("u@", "localhost", None, "db"), "postgresql://u@localhost/db");
        assert_eq!(postgres_url("", "db.local", Some(""), "db"), "postgresql://db.local/db");
        // An unbracketed IPv6 host's colons would read as a port.
        assert_eq!(postgres_url("u@", "::1", Some("5432"), "db"), "postgresql://u@[::1]:5432/db");
        assert_eq!(url_authority("[::1]", None), "[::1]");
        assert_eq!(url_authority("fe80::1", Some("3306")), "[fe80::1]:3306");
    }

    fn env_of<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
    }

    #[test]
    fn url_reads_the_process_environment() {
        let process = |var: &str| std::env::var(var).ok();
        for engine in [DbEngine::Postgres, DbEngine::Mysql] {
            assert_eq!(engine.url("db"), engine.url_in("db", &process));
        }
        assert!(DbEngine::Postgres.url("db").starts_with("postgresql://"));
        assert!(DbEngine::Mysql.url("db").starts_with("mysql://"));
    }

    #[test]
    fn postgres_url_is_built_from_the_pg_variables() {
        let env =
            [("PGHOST", "db.local"), ("PGPORT", "5433"), ("PGUSER", "app"), ("PGPASSWORD", "s@cret"), ("USER", "os")];
        assert_eq!(DbEngine::Postgres.url_in("t", &env_of(&env)), "postgresql://app:s%40cret@db.local:5433/t");
        // No PGUSER: the OS user; a socket-path PGHOST: localhost, since a URL needs TCP.
        let env = [("PGHOST", "/tmp"), ("USER", "os")];
        assert_eq!(DbEngine::Postgres.url_in("t", &env_of(&env)), "postgresql://os@localhost/t");
        assert_eq!(DbEngine::Postgres.url_in("t", &env_of(&[])), "postgresql://localhost/t");
    }

    #[test]
    fn mysql_url_is_built_from_the_mysql_variables() {
        let env = [
            ("MYSQL_HOST", "db.local"),
            ("MYSQL_TCP_PORT", "3307"),
            ("MYSQL_USER", "app"),
            ("MYSQL_PWD", "pw"),
            ("USER", "os"),
        ];
        assert_eq!(DbEngine::Mysql.url_in("t", &env_of(&env)), "mysql://app:pw@db.local:3307/t");
        // An empty port is MySQL's default, not an empty `host:`.
        let env = [("MYSQL_HOST", "/var/run/mysqld.sock"), ("MYSQL_TCP_PORT", ""), ("USER", "os")];
        assert_eq!(DbEngine::Mysql.url_in("t", &env_of(&env)), "mysql://os@127.0.0.1:3306/t");
        assert_eq!(DbEngine::Mysql.url_in("t", &env_of(&[])), "mysql://root@127.0.0.1:3306/t");
    }

    proptest::proptest! {
        /// Any MYSQL_TCP_PORT that is set and non-empty is the URL's port.
        #[test]
        fn mysql_url_carries_any_given_port(port in "[0-9]{1,5}") {
            let env = [("MYSQL_TCP_PORT", port.as_str()), ("MYSQL_USER", "u")];
            proptest::prop_assert_eq!(DbEngine::Mysql.url_in("t", &env_of(&env)), format!("mysql://u@127.0.0.1:{port}/t"));
        }
    }

    #[test]
    fn userinfo_is_percent_encoded_in_urls() {
        assert_eq!(percent_encode_userinfo("p@ss:w0/d#x"), "p%40ss%3Aw0%2Fd%23x");
        assert_eq!(percent_encode_userinfo("plain-user_1.2~3"), "plain-user_1.2~3");
        // auth_prefix encodes both components while keeping the `:` and `@` that
        // structure the userinfo, so a credential with reserved chars is safe.
        assert_eq!(auth_prefix("us er", Some("p@w:d".to_string())), "us%20er:p%40w%3Ad@");
        assert_eq!(auth_prefix("bob", None), "bob@");
        assert_eq!(auth_prefix("", Some("x".to_string())), "");
        // An empty password is no password, not a `bob:` with nothing after it.
        assert_eq!(auth_prefix("bob", Some(String::new())), "bob@");
    }

    #[test]
    fn venv_python_prefers_the_workspace_venv() {
        let ws = tempfile::tempdir().unwrap();
        assert_eq!(venv_python(ws.path()), PathBuf::from("python3"));
        std::fs::create_dir_all(ws.path().join(".venv/bin")).unwrap();
        std::fs::write(ws.path().join(".venv/bin/python"), "").unwrap();
        assert_eq!(venv_python(ws.path()), ws.path().join(".venv/bin/python"));
    }

    #[test]
    fn provisioners_run_venv_repair_before_the_python_frameworks() {
        let names: Vec<&str> = provisioners().iter().map(|p| p.name()).collect();
        assert_eq!(names, ["python-venv", "rails", "prisma", "alembic", "django", "laravel", "ef-core", "phoenix"]);
    }

    #[test]
    fn test_db_name_matches_legacy_for_normal_names() {
        assert_eq!(test_db_name("acme", "ws-abc123"), "acme_ws_abc123_test");
        assert_eq!(test_db_name("my-app", "ws-abc123-fix-bug"), "my_app_ws_abc123_fix_bug_test");
    }

    #[test]
    fn test_db_name_stays_within_63_bytes() {
        let long = "a".repeat(200);
        let name = test_db_name(&long, "ws-abc123");
        assert!(name.len() <= 63, "len {} > 63: {name}", name.len());
        // The unique ws_id + suffix survive; the project is what gets trimmed.
        assert!(name.ends_with("_ws_abc123_test"));
        assert_eq!(name, format!("{}_ws_abc123_test", "a".repeat(63 - "_ws_abc123_test".len())));
        // A project that exactly fills the remaining bytes is kept whole.
        let exact = "b".repeat(63 - "_ws_abc123_test".len());
        assert_eq!(test_db_name(&exact, "ws-abc123"), format!("{exact}_ws_abc123_test"));
    }

    #[test]
    fn resource_round_trips_through_json() {
        let r = Resource::PostgresDb { name: "acme_ws_abc_test".into() };
        let json = serde_json::to_string(&r).unwrap();
        assert_eq!(json, r#"{"type":"postgres_db","name":"acme_ws_abc_test"}"#);
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

    /// Real create + drop against MySQL. Skips unless a server is reachable
    /// (needs `mysqladmin` + MYSQL_* env pointing at it). Runs in CI's MySQL job.
    #[test]
    fn mysql_create_and_drop_roundtrip() {
        let name = "workon_provision_selftest_mysql";
        Resource::MysqlDb { name: name.into() }.teardown();
        if DbEngine::Mysql.create(name).is_err() {
            return; // no reachable MySQL server
        }
        Resource::MysqlDb { name: name.into() }.teardown();
        let recreated = DbEngine::Mysql.create(name).is_ok();
        Resource::MysqlDb { name: name.into() }.teardown();
        assert!(recreated, "mysqladmin drop should have removed the DB so create succeeds again");
    }
}
