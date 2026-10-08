//! The built `workon`, run piped in a cleared environment whose PATH is a stub directory
//! (`support::World`). The pseudo-terminal tests are in `tui/`, compiled into this crate so
//! they share the harness.

mod support;
mod tui;

use std::path::Path;

use predicates::prelude::*;
use support::World;

/// End-to-end shell-completion check: with HOME pointed at a tempdir holding one
/// worktree, the dynamic completer must surface that workspace's id when
/// completing `workon attach <TAB>`. Guards the `add = ArgValueCandidates`
/// wiring and the CompleteEnv shim, not just the candidate function.
#[test]
fn dynamic_completion_offers_workspace_ids() {
    let world = World::new();
    let proj = world.path().join("proj");
    std::fs::create_dir(&proj).unwrap();
    world.git(&proj, &["init", "-q"]);
    world.git(&proj, &["commit", "-q", "--allow-empty", "-m", "init"]);

    std::fs::create_dir_all(world.worktrees()).unwrap();
    let wt = world.worktrees().join("proj-ws-abc123");
    world.git(&proj, &["worktree", "add", "-q", "--detach", wt.to_str().unwrap(), "HEAD"]);

    // clap_complete's request protocol: `workon -- workon attach <cursor>` with
    // the cursor word index in _CLAP_COMPLETE_INDEX.
    world
        .workon(&world.path())
        .env("COMPLETE", "zsh")
        .env("_CLAP_COMPLETE_INDEX", "2")
        .args(["--", "workon", "attach", ""])
        .assert()
        .success()
        .stdout(predicate::str::contains("ws-abc123"));
}

/// `workon` with `args`, from an empty directory in a fresh world.
fn workon(args: &[&str]) -> assert_cmd::assert::Assert {
    let world = World::new();
    world.workon(&world.path()).args(args).assert()
}

#[test]
fn version_flag() {
    workon(&["--version"]).success().stdout(predicate::str::contains("workon 0."));
}

#[test]
fn help_flag() {
    workon(&["--help"]).success().stdout(predicate::str::contains("Development workspace launcher"));
}

#[test]
fn skip_copy_ignored_requires_workspace() {
    workon(&["--skip-copy-ignored"]).failure().stderr(predicate::str::contains("--skip-copy-ignored"));
}

#[test]
fn help_lists_config_flag() {
    workon(&["--help"]).success().stdout(predicate::str::contains("--config"));
}

#[test]
fn help_lists_subcommands() {
    workon(&["--help"])
        .success()
        .stdout(predicate::str::contains("create"))
        .stdout(predicate::str::contains("attach"))
        .stdout(predicate::str::contains("destroy"))
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("path"));
}

#[test]
fn create_help_lists_its_flags() {
    workon(&["create", "--help"])
        .success()
        .stdout(predicate::str::contains("--name"))
        .stdout(predicate::str::contains("--skip-copy-ignored"))
        .stdout(predicate::str::contains("--json"));
}

#[test]
fn path_unknown_reference_fails_cleanly() {
    workon(&["path", "definitely-no-such-ws"]).failure().stderr(predicate::str::contains("definitely-no-such-ws"));
}

#[test]
fn destroy_help_lists_no_save() {
    workon(&["destroy", "--help"]).success().stdout(predicate::str::contains("--no-save"));
}

/// A bare token (no slash) is a ws_id/nickname; with no matching workspace it
/// should fail cleanly rather than be misread as a flag or path. Proves the
/// subcommand + positional parse and reaches lookup.
#[test]
fn destroy_unknown_reference_fails_cleanly() {
    workon(&["destroy", "definitely-no-such-ws"]).failure().stderr(predicate::str::contains("definitely-no-such-ws"));
}

/// `--resume` is workspace-only. Guards the `requires = "workspace"` constraint
/// across the change of `-w` from an optional-value arg to a plain bool flag.
#[test]
fn resume_requires_workspace() {
    workon(&["--resume", "some-session-id"]).failure().stderr(predicate::str::contains("--resume"));
}

#[test]
fn help_lists_name_flag() {
    workon(&["--help"]).success().stdout(predicate::str::contains("--name"));
}

#[test]
fn help_lists_new_session_long_flag() {
    workon(&["--help"]).success().stdout(predicate::str::contains("--new-session"));
}

/// `-n` used to force a new session; it's now an inert no-op. It must still
/// *parse* (no "unexpected argument") — proven by reaching the config error
/// rather than a clap parse error.
#[test]
fn reserved_n_short_flag_is_accepted_as_noop() {
    workon(&["-n", "--config", "no-such-config"]).failure().stderr(predicate::str::contains("no-such-config"));
}

/// `-w` is now a pure boolean flag — it must not swallow the following
/// `--config` token as a value. If it did, config resolution wouldn't run and
/// we'd never see the "no-such-config" error.
#[test]
fn workspace_flag_takes_no_value() {
    workon(&["-w", "--config", "no-such-config"]).failure().stderr(predicate::str::contains("no-such-config"));
}

#[test]
fn missing_named_config_errors_cleanly() {
    workon(&["--config", "no-such-config"])
        .failure()
        .stderr(predicate::str::contains("no-such-config"))
        .stderr(predicate::str::contains("#creating-a-config"));
}

#[test]
fn invalid_config_name_with_path_traversal_is_rejected() {
    workon(&["--config", "../etc/hosts"]).failure().stderr(predicate::str::contains("invalid config name"));
}

/// `workon create --name <name> --json` in `proj`. Returns the JSON report.
fn create_json(world: &World, proj: &Path, name: &str) -> serde_json::Value {
    let out =
        world.workon(proj).args(["create", "--name", name, "--json"]).assert().success().get_output().stdout.clone();
    serde_json::from_slice(&out).expect("create --json prints JSON")
}

/// `--name` becomes the ws_id's label, and `--name ""` means no name: a bare
/// `ws-xxxxxx` rather than one ending in an empty `-` label.
#[test]
fn create_name_labels_the_ws_id_and_empty_name_is_no_name() {
    let world = World::new();
    let proj = world.project(&[]);
    let labelled = create_json(&world, &proj, "Fix Bug");
    assert_eq!(labelled["failed_steps"], serde_json::json!([]), "nothing failed: {labelled}");
    let labelled = labelled["ws_id"].as_str().unwrap().to_string();
    assert!(labelled.starts_with("ws-") && labelled.ends_with("-fix-bug"), "{labelled}");
    assert_eq!(labelled.len(), "ws-abcdef-fix-bug".len(), "{labelled}");

    let unnamed = create_json(&world, &proj, "")["ws_id"].as_str().unwrap().to_string();
    assert_eq!(unnamed.len(), "ws-abcdef".len(), "{unnamed}");
    assert!(unnamed.starts_with("ws-") && !unnamed.ends_with('-'), "{unnamed}");
}

/// Creating a workspace pre-accepts Claude Code's trust dialog for it, in the
/// `~/.claude.json` under `$HOME`.
#[test]
fn create_trusts_the_worktree_in_claude_json() {
    let world = World::new();
    let proj = world.project(&[]);
    let report = create_json(&world, &proj, "");
    let path = report["path"].as_str().unwrap();

    let claude: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(world.home().join(".claude.json")).unwrap()).unwrap();
    assert_eq!(claude["projects"][path]["hasTrustDialogAccepted"], serde_json::Value::Bool(true), "{claude}");
}

/// `workon create` copies the project's gitignored files into the worktree and
/// says what it did on stderr: the up-front notice (no time estimate for a
/// handful of files), a warning per file or nested repo it could not copy, and
/// a summary counting what was cloned whole and what was copied one by one.
/// Files under a tracked directory (`config/`) can't be cloned as a top-level
/// entry, so they are the ones copied individually.
#[test]
fn create_reports_the_gitignored_files_it_copies() {
    use std::os::unix::fs::PermissionsExt;

    let world = World::new();
    let proj = world
        .project(&[(".gitignore", ".env\nconfig/*.key\nconfig/gem/\nconfig/bad/\n"), ("config/app.yml", "tracked\n")]);

    std::fs::write(proj.join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(proj.join("config/master.key"), "key\n").unwrap();
    // Nested repos (a bundler git checkout) are listed as directories.
    for nested in ["config/gem", "config/bad"] {
        std::fs::create_dir_all(proj.join(nested)).unwrap();
        world.git(&proj.join(nested), &["init", "-q"]);
        std::fs::write(proj.join(nested).join("file"), "x\n").unwrap();
    }
    let unreadable = [proj.join("config/locked.key"), proj.join("config/bad/file")];
    for path in &unreadable {
        std::fs::write(path, "x\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    // Root reads anything, so it would see no failures to warn about.
    let can_fail = std::fs::File::open(&unreadable[0]).is_err();

    let output = world.workon(&proj).args(["create", "--json"]).output().unwrap();
    for path in &unreadable {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(stderr.contains("Cloning 5 gitignored files (skip with --skip-copy-ignored)..."), "{stderr}");
    if can_fail {
        assert!(stderr.contains("Warning: could not copy config/locked.key"), "{stderr}");
        assert!(stderr.contains("Warning: could not clone dir config/bad"), "{stderr}");
        assert!(stderr.contains("Cloned 5 gitignored files (1 dirs cloned, 2 copied individually)"), "{stderr}");
    }
}

/// A project with a mise config, created by a user whose mise shims directory
/// exists but is not on PATH and who has no `mise activate`, gets the warning that
/// non-interactive shells will miss the pinned tool versions. The shims directory
/// is looked up under `$HOME`; the stub `mise` answers `--version`, so `mise trust`
/// runs and the shims check after it.
#[test]
fn create_warns_when_mise_shims_are_off_path() {
    let world = World::new();
    let proj = world.project(&[("mise.toml", "[tools]\n")]);
    std::fs::create_dir_all(world.home().join(".local/share/mise/shims")).unwrap();

    let output = world.workon(&proj).args(["create", "--json"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Warning: mise shims directory is not on your PATH"), "{stderr}");
    let trusted = Path::new(&world.stubs.call("mise", "trust").args[1]).to_path_buf();
    assert!(trusted.starts_with(world.worktrees()) && trusted.ends_with("mise.toml"), "{trusted:?}");
}

const RAILS: (&str, &str) = ("config/database.yml", "test:\n  adapter: postgresql\n");

/// Runs `workon create` with `args` in a project holding `files`, where `stubs` replace the
/// default stand-ins; asserts exit 3 (not ready) and returns stdout and stderr.
fn create_not_ready(files: &[(&str, &str)], stubs: &[(&str, &str)], args: &[&str]) -> (World, String, String) {
    let world = World::new();
    let proj = world.project(files);
    for (name, body) in stubs {
        world.stubs.set(name, body);
    }
    let out = world.workon(&proj).arg("create").args(args).assert().code(3).get_output().clone();
    let (stdout, stderr) =
        (String::from_utf8_lossy(&out.stdout).into_owned(), String::from_utf8_lossy(&out.stderr).into_owned());
    (world, stdout, stderr)
}

fn last_lines(text: &str, n: usize) -> Vec<&str> {
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(n)..].to_vec()
}

/// Issue #2: a Rails schema load that fails is reported by `create`, in its JSON and in its last
/// lines, which name the command that opens the workspace anyway. It exits 3 and keeps the
/// workspace, so a script can tell it from a create that made nothing.
#[test]
fn create_reports_a_failed_schema_load() {
    let failing = [("bundle", "echo 'bundler: cannot load' >&2; exit 1")];
    let (_world, stdout, stderr) = create_not_ready(&[RAILS], &failing, &["--json"]);

    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["failed_steps"], serde_json::json!(["rails db:schema:load"]), "{report}");
    let ws_id = report["ws_id"].as_str().unwrap();
    assert!(Path::new(report["path"].as_str().unwrap()).is_dir(), "the workspace is kept: {report}");
    assert!(stderr.contains("bundler: cannot load"), "the step's own error is shown: {stderr}");
    assert_eq!(
        last_lines(&stderr, 2),
        [
            "Warning: the workspace is not ready; failed (output above): rails db:schema:load".to_string(),
            format!("Open it anyway with: workon attach {ws_id}"),
        ],
        "{stderr}"
    );
}

/// Without --json the path still goes to stdout, and the same two lines end stderr.
#[test]
fn create_without_json_ends_with_the_same_lines() {
    let failing = [("bundle", "exit 1")];
    let (_world, stdout, stderr) = create_not_ready(&[RAILS], &failing, &[]);

    let path = stdout.trim();
    assert!(Path::new(path).is_dir(), "{stdout}");
    let ws_id = path.rsplit_once("proj-").map(|(_, id)| id).unwrap();
    assert_eq!(
        last_lines(&stderr, 2),
        [
            "Warning: the workspace is not ready; failed (output above): rails db:schema:load".to_string(),
            format!("Open it anyway with: workon attach {ws_id}"),
        ],
        "{stderr}"
    );
}

/// A test database that cannot be created (the server is down) is a failed step too: the
/// workspace would otherwise run its tests against the shared database.
#[test]
fn create_reports_a_test_database_it_could_not_create() {
    let failing = [("createdb", "echo 'createdb: error: connection refused' >&2; exit 1")];
    let (_world, stdout, stderr) = create_not_ready(&[RAILS], &failing, &["--json"]);

    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    let steps = report["failed_steps"].as_array().unwrap();
    assert_eq!(steps.len(), 1, "{report}");
    assert!(steps[0].as_str().unwrap().starts_with("create test database proj_ws_"), "{report}");
    assert_eq!(report["dbs"], serde_json::json!([]), "nothing to drop: {report}");
    assert!(stderr.contains("is the Postgres server running?"), "the reason is shown: {stderr}");
}

/// Every provisioner with a setup step carries its failure through to `failed_steps`.
#[test]
fn each_provisioner_reports_its_failed_steps() {
    let csproj = "<Project><ItemGroup><PackageReference Include=\"Npgsql.EntityFrameworkCore.PostgreSQL\" Version=\"9.0.0\" /></ItemGroup></Project>\n";
    // The project's files, the stand-in that fails, and the steps it should report.
    type Case<'a> = (&'a [(&'a str, &'a str)], (&'a str, &'a str), &'a [&'a str]);
    let cases: [Case<'_>; 4] = [
        (
            &[("artisan", "#!/usr/bin/env php\n"), ("phpunit.xml", "<env name=\"DB_CONNECTION\" value=\"pgsql\"/>\n")],
            ("php", "exit 1"),
            &["php artisan migrate"],
        ),
        (
            &[("prisma/schema.prisma", "datasource db {\n  provider = \"postgresql\"\n}\n")],
            ("npx", "exit 1"),
            &["prisma schema apply", "prisma generate"],
        ),
        (&[("App/App.csproj", csproj)], ("dotnet", "exit 1"), &["dotnet tool restore", "dotnet ef database update"]),
        (
            &[("mix.exs", "def project do [app: :shop, deps: [{:ecto_sql, \"~> 3.10\"}]] end\n")],
            ("mix", "case \"$1\" in ecto.create) exit 0 ;; *) exit 1 ;; esac"),
            &["mix ecto.migrate"],
        ),
    ];
    for (files, stub, expected) in cases {
        let (_world, stdout, _stderr) = create_not_ready(files, &[stub], &["--json"]);
        let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(report["failed_steps"], serde_json::json!(expected), "{} failing: {report}", stub.0);
    }
}
