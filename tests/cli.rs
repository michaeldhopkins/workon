//! The built `workon`, run piped in a cleared environment whose PATH is a stub directory
//! (`support::World`). The pseudo-terminal tests are in `tui/`, compiled into this crate so
//! they share the harness.

mod support;
mod tui;

use std::path::{Path, PathBuf};
use std::process::Command;

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
        .stdout(predicate::str::contains("--base"))
        .stdout(predicate::str::contains("--json"));
}

#[test]
fn path_unknown_reference_fails_cleanly() {
    workon(&["path", "definitely-no-such-ws"])
        .failure()
        .stderr(predicate::str::contains("no workspace matches 'definitely-no-such-ws'"));
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

/// workon#6: `workon create` used to clone build output (`public/packs-test`) that no longer matched the commit the workspace checked out. The
/// clone now skips every Rails asset-output dir, while the sibling content a
/// workspace does want (`public/uploads`, `node_modules`) still arrives.
#[test]
fn create_skips_stale_build_output_and_keeps_its_siblings() {
    let world = World::new();
    let proj = world.project(&[
        (".gitignore", "node_modules/\npublic/packs-test/\npublic/uploads/\n"),
        ("public/robots.txt", "tracked\n"),
    ]);
    for (rel, body) in [
        ("node_modules/leftpad/index.js", "module.exports = {}\n"),
        ("public/packs-test/js/manifest.js", "{}\n"),
        ("public/uploads/avatar.png", "\nPNG\n"),
    ] {
        let path = proj.join(rel);
        std::fs::create_dir_all(path.parent().expect("a parent")).expect("the parent dirs");
        std::fs::write(path, body).expect("the file");
    }

    let output = world.workon(&proj).args(["create", "--json"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    let ws = PathBuf::from(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).expect("create --json prints JSON")["path"]
            .as_str()
            .expect("a path"),
    );

    assert!(ws.join("node_modules/leftpad/index.js").is_file(), "dependencies are cloned: {stderr}");
    assert!(ws.join("public/uploads/avatar.png").is_file(), "sibling content is cloned: {stderr}");
    assert!(!ws.join("public/packs-test").exists(), "stale build output must not be cloned");
    // Only the two kept files are counted, and only the dependency dir is
    // cloned whole: `public` already exists in the workspace (it is tracked),
    // so the upload is copied individually.
    assert!(stderr.contains("Cloning 2 gitignored files (skip with --skip-copy-ignored)..."), "{stderr}");
    assert!(stderr.contains("Cloned 2 gitignored files (1 dirs cloned, 1 copied individually)"), "{stderr}");
}

/// workon#6: node_modules installed from the project's current lockfile are
/// cloned into a workspace whose own commit still has the previous lockfile.
/// That drift — a dependency tree that can no longer match the workspace's
/// build — now gets one warning line naming the tree, the lockfile and the
/// reinstall.
#[test]
fn create_warns_when_the_cloned_deps_came_from_a_different_lockfile() {
    let world = World::new();
    let proj = world.project(&[
        (".gitignore", "node_modules/\n"),
        ("package-lock.json", "{\"lockfileVersion\":3,\"state\":\"committed\"}\n"),
    ]);
    // The project moved on after the last commit: a new lockfile, and a
    // node_modules installed from it.
    std::fs::write(proj.join("package-lock.json"), "{\"lockfileVersion\":3,\"state\":\"working copy\"}\n").unwrap();
    std::fs::create_dir_all(proj.join("node_modules/leftpad")).unwrap();
    std::fs::write(proj.join("node_modules/leftpad/index.js"), "module.exports = {}\n").unwrap();

    let output = world.workon(&proj).args(["create", "--json"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "Warning: node_modules was cloned from a package-lock.json that differs from this workspace's; run 'npm install' in the workspace."
        ),
        "{stderr}"
    );
}

/// No drift — the tree was installed from the very lockfile the workspace
/// checks out — means no warning line.
#[test]
fn create_does_not_warn_when_the_cloned_deps_match_the_workspace_lockfile() {
    let world = World::new();
    let proj = world.project(&[
        (".gitignore", "node_modules/\n"),
        ("package-lock.json", "{\"lockfileVersion\":3,\"state\":\"committed\"}\n"),
    ]);
    std::fs::create_dir_all(proj.join("node_modules/leftpad")).unwrap();
    std::fs::write(proj.join("node_modules/leftpad/index.js"), "module.exports = {}\n").unwrap();

    let output = world.workon(&proj).args(["create", "--json"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("node_modules was cloned from"), "no drift, no warning: {stderr}");
}

/// workon#6: a tree installed from a lockfile the project never committed is
/// drift too — the workspace's commit has none to match it against — and the
/// warning says so.
#[test]
fn create_warns_when_the_workspace_has_no_lockfile() {
    let world = World::new();
    let proj = world.project(&[(".gitignore", "node_modules/\n")]);
    std::fs::write(proj.join("package-lock.json"), "{\"lockfileVersion\":3}\n").unwrap();
    std::fs::create_dir_all(proj.join("node_modules/leftpad")).unwrap();
    std::fs::write(proj.join("node_modules/leftpad/index.js"), "module.exports = {}\n").unwrap();

    let output = world.workon(&proj).args(["create", "--json"]).output().unwrap();
    assert!(output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains(
            "Warning: node_modules was cloned from the project's package-lock.json, which this workspace does not have; run 'npm install' in the workspace."
        ),
        "{stderr}"
    );
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

/// Issue #4: git-sourced gems live in a per-checkout bundler path the gitignored-file clone
/// does not cover, so a workspace with a Gemfile gets `bundle check || bundle install` before
/// the schema load. A passing check means the gems are already there: no install runs.
#[test]
fn create_checks_gems_before_loading_the_schema() {
    let world = World::new();
    let proj = world.project(&[RAILS, ("Gemfile", "source \"https://rubygems.org\"\n")]);

    let out = world.workon(&proj).args(["create", "--json"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");

    let report: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    let ws = Path::new(report["path"].as_str().unwrap());
    let calls = world.stubs.calls("bundle");
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0].args, ["check"], "the gems are checked first: {calls:?}");
    assert_eq!(&calls[0].cwd, ws, "in the new workspace: {calls:?}");
    assert_eq!(calls[1].args, ["exec", "rails", "db:schema:load"], "the schema load follows: {calls:?}");
}

/// A failing check (the git-sourced gem is not yet checked out) runs the install,
/// then the schema load.
#[test]
fn create_installs_gems_when_the_check_fails() {
    let world = World::new();
    let proj = world.project(&[RAILS, ("Gemfile", "source \"https://rubygems.org\"\n")]);
    world.stubs.set("bundle", "if [ \"$1\" = check ]; then exit 1; fi\nexit 0");

    let out = world.workon(&proj).args(["create", "--json"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");

    let calls = world.stubs.calls("bundle");
    let args: Vec<Vec<&str>> = calls.iter().map(|c| c.args.iter().map(String::as_str).collect()).collect();
    assert_eq!(args, [vec!["check"], vec!["install"], vec!["exec", "rails", "db:schema:load"]], "{calls:?}");
}

/// A failed install is a failed step, and the schema load it would have left half-run
/// does not run.
#[test]
fn create_reports_a_failed_bundle_install() {
    let failing = [(
        "bundle",
        "if [ \"$1\" = check ]; then exit 1; fi\n\
         if [ \"$1\" = install ]; then echo 'bundler: not yet checked out' >&2; exit 1; fi\nexit 0",
    )];
    let (world, stdout, stderr) =
        create_not_ready(&[RAILS, ("Gemfile", "source \"https://rubygems.org\"\n")], &failing, &["--json"]);

    let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert_eq!(report["failed_steps"], serde_json::json!(["bundle install"]), "{report}");
    assert!(stderr.contains("bundler: not yet checked out"), "the step's own error is shown: {stderr}");
    let calls = world.stubs.calls("bundle");
    assert!(calls.iter().all(|c| c.args.first().is_some_and(|a| a != "exec")), "no schema load: {calls:?}");
}

/// Without a Gemfile there is nothing to check or install: the schema load is the only
/// bundle call.
#[test]
fn create_without_a_gemfile_skips_the_gem_check() {
    let world = World::new();
    let proj = world.project(&[RAILS]);

    world.workon(&proj).args(["create", "--json"]).assert().success();

    let calls = world.stubs.calls("bundle");
    let args: Vec<Vec<&str>> = calls.iter().map(|c| c.args.iter().map(String::as_str).collect()).collect();
    assert_eq!(args, [vec!["exec", "rails", "db:schema:load"]], "{calls:?}");
}

/// Every provisioner with a setup step carries its failure through to `failed_steps`.
#[test]
fn each_provisioner_reports_its_failed_steps() {
    let csproj = "<Project><ItemGroup><PackageReference Include=\"Npgsql.EntityFrameworkCore.PostgreSQL\" Version=\"9.0.0\" /></ItemGroup></Project>\n";
    // The project's files, the stand-in that fails, the steps it should report, and the variable
    // its Setup still records, and the env file the framework reads it from.
    type Case<'a> = (&'a [(&'a str, &'a str)], (&'a str, &'a str), &'a [&'a str], &'a str, Option<&'a str>);
    let cases: [Case<'_>; 4] = [
        (
            &[("artisan", "#!/usr/bin/env php\n"), ("phpunit.xml", "<env name=\"DB_CONNECTION\" value=\"pgsql\"/>\n")],
            ("php", "exit 1"),
            &["php artisan migrate"],
            "DB_URL",
            Some(".env.testing"),
        ),
        (
            &[("prisma/schema.prisma", "datasource db {\n  provider = \"postgresql\"\n}\n")],
            ("npx", "exit 1"),
            &["prisma schema apply", "prisma generate"],
            "DATABASE_URL",
            Some(".env.test.local"),
        ),
        (
            &[("App/App.csproj", csproj)],
            ("dotnet", "exit 1"),
            &["dotnet tool restore", "dotnet ef database update"],
            "ConnectionStrings__",
            Some(".env.test.local"),
        ),
        (
            &[("mix.exs", "def project do [app: :shop, deps: [{:ecto_sql, \"~> 3.10\"}]] end\n")],
            ("mix", "case \"$1\" in ecto.create) exit 0 ;; *) exit 1 ;; esac"),
            &["mix ecto.migrate"],
            "MIX_TEST_PARTITION",
            None,
        ),
    ];
    for (files, stub, expected, var, env_file) in cases {
        let (_world, stdout, _stderr) = create_not_ready(files, &[stub], &["--json"]);
        let report: serde_json::Value = serde_json::from_str(&stdout).unwrap();
        assert_eq!(report["failed_steps"], serde_json::json!(expected), "{} failing: {report}", stub.0);
        assert_eq!(report["dbs"].as_array().map(Vec::len), Some(1), "the database is kept for teardown: {report}");
        let meta = std::fs::read_to_string(Path::new(report["path"].as_str().unwrap()).join(".workon.json")).unwrap();
        assert!(meta.contains(var), "{} keeps {var} for the session: {meta}", stub.0);
        if let Some(file) = env_file {
            let written =
                std::fs::read_to_string(Path::new(report["path"].as_str().unwrap()).join(file)).unwrap_or_default();
            assert!(written.contains(var), "{} writes {var} to {file}: {written:?}", stub.0);
        }
    }
}

/// A running session for this project whose process tree lacks the layout's focused command
/// (`claude`) was started with another layout: plain `workon` refuses to attach to it rather
/// than silently giving the user the running layout.
#[test]
fn workon_refuses_to_attach_to_a_session_running_another_layout() {
    let world = World::new();
    let proj = world.project(&[]);
    world.stubs.set(
        "zellij",
        "case \"$1\" in --version) echo 'zellij 0.43.1' ;; list-sessions) echo 'proj [Created 1m ago]' ;; esac",
    );
    world.stubs.set("pgrep", "echo 4242");
    world.stubs.set("ps", "printf '4243 4242 /bin/zsh\\n4244 4243 /opt/homebrew/bin/opencode\\n'");

    let out = world.workon(&proj).assert().failure().get_output().clone();

    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        stderr.contains("zellij session 'proj' is already running in the main worktree with a different layout"),
        "{stderr}"
    );
    assert!(world.stubs.calls("zellij").iter().all(|c| c.args.first().is_none_or(|a| a != "attach")), "never attached");
}

/// With no reference, `path` finds the workspace the cwd is inside, from any depth, and
/// refuses outside one.
#[test]
fn path_without_a_reference_finds_the_enclosing_workspace() {
    let world = World::new();
    let proj = world.project(&[]);
    let created = create_json(&world, &proj, "deep");
    let ws = Path::new(created["path"].as_str().unwrap()).to_path_buf();
    let deep = ws.join("a/b");
    std::fs::create_dir_all(&deep).unwrap();

    for dir in [&ws, &deep] {
        let out = world.workon(dir).arg("path").assert().success().get_output().stdout.clone();
        assert_eq!(String::from_utf8_lossy(&out).trim(), ws.to_str().unwrap(), "from {}", dir.display());
    }
    world.workon(&proj).arg("path").assert().failure().stderr(predicate::str::contains("not inside a workspace"));
}

/// `create --base` is parsed and handed to the backend before any workspace is made: with no
/// jj on PATH the git backend refuses it and nothing is created.
#[test]
fn create_base_is_refused_before_anything_is_made_without_jj() {
    let world = World::new();
    let proj = world.project(&[]);
    world
        .workon(&proj)
        .args(["create", "--base", "main"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--base needs jj installed"));
    assert!(!world.worktrees().exists() || std::fs::read_dir(world.worktrees()).unwrap().next().is_none());
}

/// A jj repo whose workspaces are registered by jj, not by the `<project>-<name>`
/// directory convention. `None` when jj is not installed (the main CI job has no jj).
struct JjRepo {
    world: World,
    proj: PathBuf,
}

fn jj_bin() -> Option<PathBuf> {
    let out = Command::new("/bin/sh").arg("-c").arg("command -v jj").output().ok()?;
    if !out.status.success() {
        return None;
    }
    let path = String::from_utf8(out.stdout).ok()?;
    let path = path.trim();
    (!path.is_empty()).then(|| PathBuf::from(path))
}

fn jj(dir: &Path, args: &[&str]) -> String {
    let out = Command::new(jj_bin().expect("jj")).args(args).current_dir(dir).output().expect("jj runs");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    assert!(out.status.success(), "jj {args:?} in {} failed\n{stdout}{stderr}", dir.display());
    stdout
}

/// `destroy` removes the directory with a background `rm -rf`.
fn assert_removed_soon(dir: &Path) {
    let start = std::time::Instant::now();
    loop {
        if !dir.exists() {
            return;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(10), "{} was never removed", dir.display());
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

fn jj_names(proj: &Path) -> Vec<String> {
    jj(proj, &["--ignore-working-copy", "--color=never", "workspace", "list", "-T", "name ++ \"\\n\""])
        .lines()
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect()
}

/// Give a jj workspace the git worktree pointer workon already writes, so
/// `project_dir_of` resolves and destroy reaches the name check.
fn link_git_worktree(world: &World, proj: &Path, ws: &Path, git_name: &str) {
    let git_dir = world.git(proj, &["rev-parse", "--absolute-git-dir"]);
    let git_dir = git_dir.trim();
    let wt = format!("{git_dir}/worktrees/{git_name}");
    std::fs::create_dir_all(&wt).expect("git worktree dir");
    let head = world.git(proj, &["rev-parse", "HEAD"]);
    std::fs::write(format!("{wt}/gitdir"), format!("{}/.git\n", ws.display())).expect("gitdir pointer");
    std::fs::write(format!("{wt}/commondir"), "../..\n").expect("commondir pointer");
    std::fs::write(format!("{wt}/HEAD"), head).expect("worktree HEAD");
    std::fs::write(ws.join(".git"), format!("gitdir: {wt}\n")).expect("workspace git file");
}

impl JjRepo {
    fn new() -> Option<Self> {
        let bin = jj_bin()?;
        let world = World::new();
        world.stubs.set("jj", &format!("exec '{}' \"$@\"", bin.display()));
        let proj = world.path().join("proj");
        std::fs::create_dir_all(&proj).expect("project dir");
        world.git(&proj, &["init", "-q", "-b", "main"]);
        world.git(&proj, &["commit", "-q", "--allow-empty", "-m", "init"]);
        jj(&proj, &["git", "init", "--colocate"]);
        Some(Self { world, proj })
    }

    /// `jj workspace add` at `dir`, registered as `name`, with a git pointer back to the project.
    fn add(&self, dir: &Path, name: &str) {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).expect("workspace parent");
        }
        let path = dir.to_str().expect("workspace path is utf-8");
        jj(&self.proj, &["workspace", "add", path, "--name", name, "-r", "@"]);
        link_git_worktree(&self.world, &self.proj, dir, name);
    }
}

/// Issue #5, case 1. A workspace `list` shows whose directory is not
/// `<project>-<name>`: `path` and `destroy` of the name jj registered must
/// resolve it, and destroy by its path must not demand that directory spelling.
#[test]
fn listed_workspace_with_an_unprefixed_directory_can_be_pathed_and_destroyed() {
    let Some(repo) = JjRepo::new() else { return };
    let dir = repo.world.worktrees().join("customdir");
    repo.add(&dir, "odd");

    let listed = repo.world.workon(&repo.proj).arg("list").assert().success().get_output().stdout.clone();
    let listed = String::from_utf8_lossy(&listed);
    assert!(listed.contains("odd"), "list must show jj's registered name, not the directory's: {listed}");

    let path = repo.world.workon(&repo.proj).args(["path", "odd"]).assert().success().get_output().stdout.clone();
    assert_eq!(String::from_utf8_lossy(&path).trim(), dir.to_str().unwrap());

    let destroyed = repo
        .world
        .workon(&repo.proj)
        .args(["destroy", "--no-save", dir.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let destroyed = String::from_utf8_lossy(&destroyed);
    assert!(destroyed.contains("Forgot jj workspace odd"), "{destroyed}");
    assert!(!jj_names(&repo.proj).iter().any(|n| n == "odd"), "jj still has odd: {:?}", jj_names(&repo.proj));
}

/// A jj workspace name with a space is a `RefSymbol`. `list` quotes it, and
/// `forget` has to be given the name without those quotes.
#[test]
fn destroy_forgets_a_workspace_whose_name_has_a_space() {
    let Some(repo) = JjRepo::new() else { return };
    let dir = repo.world.worktrees().join("spaced");
    repo.add(&dir, "my workspace");

    let path =
        repo.world.workon(&repo.proj).args(["path", "my workspace"]).assert().success().get_output().stdout.clone();
    assert_eq!(String::from_utf8_lossy(&path).trim(), dir.to_str().unwrap());

    let destroyed = repo
        .world
        .workon(&repo.proj)
        .args(["destroy", "--no-save", dir.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let destroyed = String::from_utf8_lossy(&destroyed);
    assert!(destroyed.lines().any(|line| line == "Forgot jj workspace my workspace"), "{destroyed}");
    let listed = jj(
        &repo.proj,
        &["--ignore-working-copy", "--color=never", "workspace", "list", "-T", "name.escape_json() ++ \"\\n\""],
    );
    assert!(!listed.lines().any(|line| line == "\"my workspace\""), "jj still has the workspace:\n{listed}");
}

/// `jj workspace forget` failed. The directory and the registration stay, and
/// workon does not say it forgot the workspace.
#[test]
fn destroy_leaves_the_directory_when_jj_forget_fails() {
    let Some(repo) = JjRepo::new() else { return };
    let dir = repo.world.worktrees().join("proj-feature");
    repo.add(&dir, "proj-feature");
    let bin = jj_bin().expect("jj");
    repo.world.stubs.set(
        "jj",
        &format!(
            "if [ \"$1\" = workspace ] && [ \"$2\" = forget ]; then echo 'Error: forget failed' >&2; exit 1; fi\n\
             exec '{}' \"$@\"",
            bin.display()
        ),
    );

    let destroyed = repo
        .world
        .workon(&repo.proj)
        .args(["destroy", "--no-save", dir.to_str().unwrap()])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let destroyed = String::from_utf8_lossy(&destroyed);
    assert!(destroyed.contains("forget failed"), "{destroyed}");
    assert!(destroyed.lines().all(|line| !line.starts_with("Forgot jj workspace")), "{destroyed}");
    assert!(dir.exists(), "deleted the directory after forget failed");
    assert!(jj_names(&repo.proj).iter().any(|n| n == "proj-feature"), "registration was removed");
}

/// `default` is the project's own checkout. `path` prints it. `destroy` leaves
/// the checkout and the registration where they are.
#[test]
fn path_and_destroy_default_leave_the_project_checkout() {
    let Some(repo) = JjRepo::new() else { return };
    std::fs::create_dir_all(repo.world.worktrees()).unwrap();

    let path = repo.world.workon(&repo.proj).args(["path", "default"]).assert().success().get_output().stdout.clone();
    assert_eq!(String::from_utf8_lossy(&path).trim(), repo.proj.to_str().unwrap());

    let destroyed = repo
        .world
        .workon(&repo.proj)
        .args(["destroy", "--no-save", "default"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let destroyed = String::from_utf8_lossy(&destroyed);
    assert!(destroyed.contains("not a workspace under"), "{destroyed}");
    assert!(repo.proj.exists(), "deleted the project checkout");
    assert!(jj_names(&repo.proj).iter().any(|n| n == "default"), "registration was removed");
    assert!(destroyed.lines().all(|line| !line.starts_with("Forgot jj workspace")), "{destroyed}");
}

/// A registration whose root is a sibling of `~/.worktrees` (the name shares
/// the prefix) is not a workspace destroy may remove.
#[test]
fn destroy_of_a_worktrees_sibling_leaves_the_directory_and_registration() {
    let Some(repo) = JjRepo::new() else { return };
    std::fs::create_dir_all(repo.world.worktrees()).unwrap();
    let dir = repo.world.home().join(".worktrees-evil");
    repo.add(&dir, "beside");

    let destroyed = repo
        .world
        .workon(&repo.proj)
        .args(["destroy", "--no-save", dir.to_str().unwrap()])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let destroyed = String::from_utf8_lossy(&destroyed);
    assert!(destroyed.contains("not a workspace under"), "{destroyed}");
    assert!(dir.exists(), "deleted the sibling");
    assert!(jj_names(&repo.proj).iter().any(|n| n == "beside"), "registration was removed");
    assert!(destroyed.lines().all(|line| !line.starts_with("Forgot jj workspace")), "{destroyed}");
}

/// A registration outside `~/.worktrees` is reported, and destroy leaves it.
#[test]
fn destroy_of_an_outside_registration_leaves_the_directory_and_registration() {
    let Some(repo) = JjRepo::new() else { return };
    std::fs::create_dir_all(repo.world.worktrees()).unwrap();
    let dir = repo.world.path().join("outside").join("extra");
    repo.add(&dir, "extra");

    let destroyed = repo
        .world
        .workon(&repo.proj)
        .args(["destroy", "--no-save", "extra"])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let destroyed = String::from_utf8_lossy(&destroyed);
    assert!(destroyed.contains("not a workspace under"), "{destroyed}");
    assert!(dir.exists(), "deleted the outside workspace");
    assert!(jj_names(&repo.proj).iter().any(|n| n == "extra"), "registration was removed");
    assert!(destroyed.lines().all(|line| !line.starts_with("Forgot jj workspace")), "{destroyed}");
}

/// The registration was forgotten and the directory was not. A later destroy of
/// that directory, inside `~/.worktrees`, removes the directory.
#[test]
fn destroy_finishes_a_forgotten_workspace_by_its_directory() {
    let Some(repo) = JjRepo::new() else { return };
    let dir = repo.world.worktrees().join("proj-feature");
    let sibling = repo.world.worktrees().join("proj-other");
    repo.add(&dir, "proj-feature");
    repo.add(&sibling, "proj-other");
    jj(&repo.proj, &["workspace", "forget", "proj-feature"]);
    assert!(dir.exists(), "the fixture removed the directory");
    assert!(!jj_names(&repo.proj).iter().any(|n| n == "proj-feature"));

    let destroyed = repo
        .world
        .workon(&repo.proj)
        .args(["destroy", "--no-save", dir.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let destroyed = String::from_utf8_lossy(&destroyed);
    assert!(destroyed.lines().all(|line| !line.starts_with("Forgot jj workspace")), "{destroyed}");
    assert_removed_soon(&dir);
    assert!(sibling.exists(), "destroyed the sibling");
    let names = jj_names(&repo.proj);
    assert!(names.iter().any(|n| n == "proj-other"), "jj lost the sibling: {names:?}");
    assert!(!names.iter().any(|n| n == "proj-feature"), "jj still has proj-feature: {names:?}");
}

/// A path inside a workspace is not the workspace. Destroy leaves that subdirectory.
#[test]
fn destroy_of_a_subdirectory_inside_a_workspace_leaves_it() {
    let Some(repo) = JjRepo::new() else { return };
    let dir = repo.world.worktrees().join("proj-feature");
    repo.add(&dir, "proj-feature");
    let nested = dir.join("src");
    std::fs::create_dir(&nested).unwrap();

    let destroyed = repo
        .world
        .workon(&repo.proj)
        .args(["destroy", "--no-save", nested.to_str().unwrap()])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let destroyed = String::from_utf8_lossy(&destroyed);
    assert!(destroyed.contains("is not named"), "{destroyed}");
    assert!(nested.exists(), "deleted a subdirectory of the workspace: {destroyed}");
    assert!(dir.exists(), "deleted the workspace: {destroyed}");
    assert!(jj_names(&repo.proj).iter().any(|n| n == "proj-feature"), "registration was removed: {destroyed}");
    assert!(destroyed.lines().all(|line| !line.starts_with("Forgot jj workspace")), "{destroyed}");
}

/// A forgotten registration outside `~/.worktrees` is not a directory destroy may remove.
#[test]
fn destroy_of_a_forgotten_outside_directory_leaves_it() {
    let Some(repo) = JjRepo::new() else { return };
    std::fs::create_dir_all(repo.world.worktrees()).unwrap();
    let dir = repo.world.path().join("outside").join("extra");
    repo.add(&dir, "extra");
    jj(&repo.proj, &["workspace", "forget", "extra"]);
    assert!(dir.exists(), "the fixture removed the directory");

    let destroyed = repo
        .world
        .workon(&repo.proj)
        .args(["destroy", "--no-save", dir.to_str().unwrap()])
        .assert()
        .failure()
        .get_output()
        .stderr
        .clone();
    let destroyed = String::from_utf8_lossy(&destroyed);
    assert!(dir.exists(), "deleted the outside directory: {destroyed}");
    assert!(destroyed.lines().all(|line| !line.starts_with("Forgot jj workspace")), "{destroyed}");
}

/// Issue #5, case 2. The directory and the jj registration are both
/// `<project>-<name>`. Destroying that directory must forget the registered
/// name, not the short name left after stripping the project prefix.
#[test]
fn destroy_forgets_the_jj_workspace_whose_root_is_the_directory() {
    let Some(repo) = JjRepo::new() else { return };
    let dir = repo.world.worktrees().join("proj-feature");
    let sibling = repo.world.worktrees().join("proj-other");
    repo.add(&dir, "proj-feature");
    repo.add(&sibling, "proj-other");

    let listed = repo.world.workon(&repo.proj).arg("list").assert().success().get_output().stdout.clone();
    let listed = String::from_utf8_lossy(&listed);
    assert!(listed.contains("proj-feature"), "list must show the registered name: {listed}");

    let destroyed = repo
        .world
        .workon(&repo.proj)
        .args(["destroy", "--no-save", dir.to_str().unwrap()])
        .assert()
        .success()
        .get_output()
        .stderr
        .clone();
    let destroyed = String::from_utf8_lossy(&destroyed);
    assert!(destroyed.lines().any(|line| line == "Forgot jj workspace proj-feature"), "{destroyed}");
    assert!(
        destroyed.lines().all(|line| line != "Forgot jj workspace feature"),
        "forgot the stripped name instead of the registered one: {destroyed}"
    );
    assert_removed_soon(&dir);
    assert!(sibling.exists(), "destroyed the sibling");
    let names = jj_names(&repo.proj);
    assert!(!names.iter().any(|n| n == "proj-feature"), "jj still has proj-feature: {names:?}");
    assert!(names.iter().any(|n| n == "proj-other"), "jj lost the sibling: {names:?}");
}

/// Issue #5, case 3. Registrations jj has and workon does not manage — a root
/// outside `~/.worktrees`, and one whose directory is already gone — are
/// reported. `path` still resolves the one that has a root.
#[test]
fn list_reports_jj_registrations_workon_does_not_manage() {
    let Some(repo) = JjRepo::new() else { return };
    let outside = repo.world.path().join("outside").join("extra");
    repo.add(&outside, "extra");
    let ghost = repo.world.worktrees().join("ghostdir");
    repo.add(&ghost, "ghost");
    std::fs::remove_dir_all(&ghost).unwrap();

    let listed = repo.world.workon(&repo.proj).arg("list").assert().success().get_output().clone();
    let stderr = String::from_utf8_lossy(&listed.stderr);
    assert!(stderr.contains("jj workspace extra is registered and workon does not manage it"), "{stderr}");
    assert!(stderr.contains("jj workspace ghost is registered and workon does not manage it"), "{stderr}");

    let path = repo.world.workon(&repo.proj).args(["path", "extra"]).assert().success().get_output().stdout.clone();
    assert_eq!(String::from_utf8_lossy(&path).trim(), outside.to_str().unwrap());
    assert!(jj_names(&repo.proj).iter().any(|n| n == "extra"), "reporting extra must not forget it");
}

/// Every jj repo registers a workspace named `default`, so that line would show
/// on every list. When `default` is the only registration workon does not
/// manage, neither text nor JSON mode prints it. Any other name still does.
#[test]
fn list_does_not_report_the_default_jj_workspace() {
    let Some(repo) = JjRepo::new() else { return };
    assert_eq!(jj_names(&repo.proj), ["default".to_string()], "the repo should register only default");

    for args in [&["list"][..], &["list", "--json"]] {
        let listed = repo.world.workon(&repo.proj).args(args).assert().success().get_output().clone();
        let stdout = String::from_utf8_lossy(&listed.stdout);
        let stderr = String::from_utf8_lossy(&listed.stderr);
        assert!(
            !stdout.contains("is registered and workon does not manage it"),
            "{args:?} printed it on stdout: {stdout}"
        );
        assert!(
            !stderr.contains("is registered and workon does not manage it"),
            "{args:?} printed it on stderr: {stderr}"
        );
    }

    let outside = repo.world.path().join("outside").join("extra");
    repo.add(&outside, "extra");
    for args in [&["list"][..], &["list", "--json"]] {
        let listed = repo.world.workon(&repo.proj).args(args).assert().success().get_output().clone();
        let stderr = String::from_utf8_lossy(&listed.stderr);
        assert!(
            stderr.contains("jj workspace extra is registered and workon does not manage it"),
            "{args:?} dropped the non-default registration: {stderr}"
        );
        assert!(
            !stderr.contains("jj workspace default is registered and workon does not manage it"),
            "{args:?} still reported default: {stderr}"
        );
    }
}

/// A workspace whose `mise env` adds a variable and puts `tools` (holding `only-in-mise`) first on
/// PATH; returns the world, its ws_id and directory.
fn exec_world() -> (World, String, PathBuf) {
    let world = World::new();
    let proj = world.project(&[]);
    let tools = world.path().join("tools");
    std::fs::create_dir(&tools).expect("tools dir");
    write_script(&tools.join("only-in-mise"), "echo found on the mise PATH");
    let mise_env =
        serde_json::json!({ "FROM_MISE": "yes", "PATH": format!("{}:{}", tools.display(), world.stubs.path()) });
    world.stubs.set("mise", &format!("[ \"$1\" = env ] && printf '%s' '{mise_env}'; exit 0"));
    let created = create_json(&world, &proj, "exec");
    let ws_id = created["ws_id"].as_str().expect("ws_id").to_string();
    let ws_dir = PathBuf::from(created["path"].as_str().expect("path"));
    (world, ws_id, ws_dir)
}

fn write_script(path: &Path, body: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, format!("#!/bin/sh\n{body}\n")).expect("write script");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).expect("chmod script");
}

/// `exec` runs the command in the workspace's directory with mise's environment, looks the
/// program up on mise's PATH, and exits with the command's status (workon#7).
#[test]
fn exec_runs_the_command_in_the_workspace_with_its_env_and_status() {
    let (world, ws_id, ws_dir) = exec_world();
    let out = world
        .workon(&world.path())
        .args(["exec", &ws_id, "--", "sh", "-c", "pwd -P; echo \"$FROM_MISE\"; only-in-mise; exit 7"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(7), "{out:?}");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected = format!("{}\nyes\nfound on the mise PATH\n", std::fs::canonicalize(&ws_dir).unwrap().display());
    assert_eq!(stdout, expected);
}

/// A relative program such as `bin/rails` is the workspace's, not the caller's directory's.
#[test]
fn exec_resolves_a_relative_program_in_the_workspace() {
    let (world, ws_id, ws_dir) = exec_world();
    std::fs::create_dir(ws_dir.join("bin")).unwrap();
    write_script(&ws_dir.join("bin/hello"), "echo \"hello $1\"");
    world
        .workon(&world.path())
        .args(["exec", &ws_id, "--", "bin/hello", "there"])
        .assert()
        .success()
        .stdout("hello there\n");
}

/// With no reference, the workspace is the one the caller stands in, as for `path`.
#[test]
fn exec_without_a_reference_uses_the_enclosing_workspace() {
    let (world, _, ws_dir) = exec_world();
    let inside = ws_dir.join("sub");
    std::fs::create_dir(&inside).unwrap();
    let canonical = std::fs::canonicalize(&ws_dir).unwrap();
    world
        .workon(&inside)
        .args(["exec", "--", "pwd", "-P"])
        .assert()
        .success()
        .stdout(format!("{}\n", canonical.display()));
}

#[test]
fn exec_of_a_missing_command_exits_127() {
    let (world, ws_id, _) = exec_world();
    world
        .workon(&world.path())
        .args(["exec", &ws_id, "--", "no-such-command"])
        .assert()
        .code(127)
        .stderr(predicate::str::contains("workon exec: no-such-command: command not found"));
}

#[test]
fn exec_of_a_file_that_cannot_run_exits_126() {
    let (world, ws_id, ws_dir) = exec_world();
    std::fs::write(ws_dir.join("notes.txt"), "not a program\n").unwrap();
    world
        .workon(&world.path())
        .args(["exec", &ws_id, "--", "./notes.txt"])
        .assert()
        .code(126)
        .stderr(predicate::str::contains("workon exec: ./notes.txt: permission denied"));
}

#[test]
fn exec_needs_a_command_after_the_separator() {
    workon(&["exec", "some-ws"]).code(2).stderr(predicate::str::contains("COMMAND"));
}

#[test]
fn exec_of_a_workspace_path_that_is_gone_fails_cleanly() {
    let world = World::new();
    let gone = world.path().join("gone");
    world
        .workon(&world.path())
        .args(["exec", gone.to_str().unwrap(), "--", "true"])
        .assert()
        .code(1)
        .stderr(predicate::str::contains("does not exist"));
}
