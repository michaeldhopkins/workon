use std::path::Path;
use std::process::{Command, Stdio};

use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;

fn git(dir: &Path, args: &[&str]) {
    let ok = Command::new("git")
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    assert!(ok, "git {args:?} failed");
}

/// End-to-end shell-completion check: with HOME pointed at a tempdir holding one
/// worktree, the dynamic completer must surface that workspace's id when
/// completing `workon attach <TAB>`. Guards the `add = ArgValueCandidates`
/// wiring and the CompleteEnv shim, not just the candidate function.
#[test]
fn dynamic_completion_offers_workspace_ids() {
    let home = tempfile::tempdir().unwrap();
    let root = home.path();

    let proj = root.join("proj");
    std::fs::create_dir(&proj).unwrap();
    git(&proj, &["init", "-q"]);
    git(&proj, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"]);

    let worktrees = root.join(".worktrees");
    std::fs::create_dir_all(&worktrees).unwrap();
    let wt = worktrees.join("proj-ws-abc123");
    git(&proj, &["worktree", "add", "-q", "--detach", wt.to_str().unwrap(), "HEAD"]);

    // clap_complete's request protocol: `workon -- workon attach <cursor>` with
    // the cursor word index in _CLAP_COMPLETE_INDEX.
    cargo_bin_cmd!("workon")
        .env("HOME", root)
        .env("COMPLETE", "zsh")
        .env("_CLAP_COMPLETE_INDEX", "2")
        .args(["--", "workon", "attach", ""])
        .assert()
        .success()
        .stdout(predicate::str::contains("ws-abc123"));
}

#[test]
fn version_flag() {
    cargo_bin_cmd!("workon").arg("--version").assert().success().stdout(predicate::str::contains("workon 0."));
}

#[test]
fn help_flag() {
    cargo_bin_cmd!("workon")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("Development workspace launcher"));
}

#[test]
fn skip_copy_ignored_requires_workspace() {
    cargo_bin_cmd!("workon")
        .arg("--skip-copy-ignored")
        .assert()
        .failure()
        .stderr(predicate::str::contains("--skip-copy-ignored"));
}

#[test]
fn help_lists_config_flag() {
    cargo_bin_cmd!("workon").arg("--help").assert().success().stdout(predicate::str::contains("--config"));
}

#[test]
fn help_lists_subcommands() {
    cargo_bin_cmd!("workon")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("create"))
        .stdout(predicate::str::contains("attach"))
        .stdout(predicate::str::contains("destroy"))
        .stdout(predicate::str::contains("list"))
        .stdout(predicate::str::contains("path"));
}

#[test]
fn create_help_lists_its_flags() {
    cargo_bin_cmd!("workon")
        .args(["create", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--name"))
        .stdout(predicate::str::contains("--skip-copy-ignored"))
        .stdout(predicate::str::contains("--json"));
}

#[test]
fn path_unknown_reference_fails_cleanly() {
    cargo_bin_cmd!("workon")
        .args(["path", "definitely-no-such-ws"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("definitely-no-such-ws"));
}

#[test]
fn destroy_help_lists_no_save() {
    cargo_bin_cmd!("workon")
        .args(["destroy", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("--no-save"));
}

/// A bare token (no slash) is a ws_id/nickname; with no matching workspace it
/// should fail cleanly rather than be misread as a flag or path. Proves the
/// subcommand + positional parse and reaches lookup.
#[test]
fn destroy_unknown_reference_fails_cleanly() {
    cargo_bin_cmd!("workon")
        .args(["destroy", "definitely-no-such-ws"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("definitely-no-such-ws"));
}

/// `--resume` is workspace-only. Guards the `requires = "workspace"` constraint
/// across the change of `-w` from an optional-value arg to a plain bool flag.
#[test]
fn resume_requires_workspace() {
    cargo_bin_cmd!("workon")
        .args(["--resume", "some-session-id"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("--resume"));
}

#[test]
fn help_lists_name_flag() {
    cargo_bin_cmd!("workon").arg("--help").assert().success().stdout(predicate::str::contains("--name"));
}

#[test]
fn help_lists_new_session_long_flag() {
    cargo_bin_cmd!("workon").arg("--help").assert().success().stdout(predicate::str::contains("--new-session"));
}

/// `-n` used to force a new session; it's now an inert no-op. It must still
/// *parse* (no "unexpected argument") — proven by reaching the config error
/// rather than a clap parse error.
#[test]
fn reserved_n_short_flag_is_accepted_as_noop() {
    let tmp = tempfile::tempdir().unwrap();
    cargo_bin_cmd!("workon")
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["-n", "--config", "no-such-config"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no-such-config"));
}

/// `-w` is now a pure boolean flag — it must not swallow the following
/// `--config` token as a value. If it did, config resolution wouldn't run and
/// we'd never see the "no-such-config" error.
#[test]
fn workspace_flag_takes_no_value() {
    let tmp = tempfile::tempdir().unwrap();
    cargo_bin_cmd!("workon")
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["-w", "--config", "no-such-config"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no-such-config"));
}

#[test]
fn missing_named_config_errors_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    cargo_bin_cmd!("workon")
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["--config", "no-such-config"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no-such-config"))
        .stderr(predicate::str::contains("#creating-a-config"));
}

#[test]
fn invalid_config_name_with_path_traversal_is_rejected() {
    let tmp = tempfile::tempdir().unwrap();
    cargo_bin_cmd!("workon")
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["--config", "../etc/hosts"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid config name"));
}

/// `workon create --json` in a fresh repo, with HOME pointed at `root` so the
/// worktree and `~/.claude.json` land in the tempdir. Returns the JSON report.
fn create_json(root: &Path, name: &str) -> serde_json::Value {
    let proj = root.join("proj");
    if !proj.exists() {
        std::fs::create_dir(&proj).unwrap();
        git(&proj, &["init", "-q", "-b", "main"]);
        git(&proj, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "--allow-empty", "-m", "init"]);
        // The git backend (used when jj is not on PATH) branches from
        // `origin/<trunk>`, so the repo needs a remote carrying main.
        let origin = root.join("origin.git");
        git(root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
        git(&proj, &["remote", "add", "origin", origin.to_str().unwrap()]);
        git(&proj, &["push", "-q", "origin", "main"]);
    }
    let out = cargo_bin_cmd!("workon")
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join(".config"))
        .current_dir(&proj)
        .args(["create", "--name", name, "--json"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    serde_json::from_slice(&out).unwrap()
}

/// `--name` becomes the ws_id's label, and `--name ""` means no name: a bare
/// `ws-xxxxxx` rather than one ending in an empty `-` label.
#[test]
fn create_name_labels_the_ws_id_and_empty_name_is_no_name() {
    let home = tempfile::tempdir().unwrap();
    let labelled = create_json(home.path(), "Fix Bug")["ws_id"].as_str().unwrap().to_string();
    assert!(labelled.starts_with("ws-") && labelled.ends_with("-fix-bug"), "{labelled}");
    assert_eq!(labelled.len(), "ws-abcdef-fix-bug".len(), "{labelled}");

    let unnamed = create_json(home.path(), "")["ws_id"].as_str().unwrap().to_string();
    assert_eq!(unnamed.len(), "ws-abcdef".len(), "{unnamed}");
    assert!(unnamed.starts_with("ws-") && !unnamed.ends_with('-'), "{unnamed}");
}

/// Creating a workspace pre-accepts Claude Code's trust dialog for it, in the
/// `~/.claude.json` under `$HOME`.
#[test]
fn create_trusts_the_worktree_in_claude_json() {
    let home = tempfile::tempdir().unwrap();
    let report = create_json(home.path(), "");
    let path = report["path"].as_str().unwrap();

    let claude: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(home.path().join(".claude.json")).unwrap()).unwrap();
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

    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let proj = root.join("proj");
    std::fs::create_dir_all(proj.join("config")).unwrap();
    git(&proj, &["init", "-q", "-b", "main"]);
    std::fs::write(proj.join(".gitignore"), ".env\nconfig/*.key\nconfig/gem/\nconfig/bad/\n").unwrap();
    std::fs::write(proj.join("config/app.yml"), "tracked\n").unwrap();
    git(&proj, &["add", "."]);
    git(&proj, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"]);
    let origin = root.join("origin.git");
    git(root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(&proj, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&proj, &["push", "-q", "origin", "main"]);

    std::fs::write(proj.join(".env"), "SECRET=1\n").unwrap();
    std::fs::write(proj.join("config/master.key"), "key\n").unwrap();
    // Nested repos (a bundler git checkout) are listed as directories.
    for nested in ["config/gem", "config/bad"] {
        std::fs::create_dir_all(proj.join(nested)).unwrap();
        git(&proj.join(nested), &["init", "-q"]);
        std::fs::write(proj.join(nested).join("file"), "x\n").unwrap();
    }
    let unreadable = [proj.join("config/locked.key"), proj.join("config/bad/file")];
    for path in &unreadable {
        std::fs::write(path, "x\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    // Root reads anything, so it would see no failures to warn about.
    let can_fail = std::fs::File::open(&unreadable[0]).is_err();

    let output = cargo_bin_cmd!("workon")
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join(".config"))
        .current_dir(&proj)
        .args(["create", "--json"])
        .output()
        .unwrap();
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
/// is looked up under `$HOME`.
#[test]
fn create_warns_when_mise_shims_are_off_path() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let root = root.path();
    let proj = root.join("proj");
    std::fs::create_dir(&proj).unwrap();
    git(&proj, &["init", "-q", "-b", "main"]);
    std::fs::write(proj.join("mise.toml"), "[tools]\n").unwrap();
    git(&proj, &["add", "."]);
    git(&proj, &["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-q", "-m", "init"]);
    let origin = root.join("origin.git");
    git(root, &["init", "-q", "--bare", origin.to_str().unwrap()]);
    git(&proj, &["remote", "add", "origin", origin.to_str().unwrap()]);
    git(&proj, &["push", "-q", "origin", "main"]);

    std::fs::create_dir_all(root.join(".local/share/mise/shims")).unwrap();
    // A stand-in mise, so `mise trust` succeeds and the shims check runs.
    let bin = root.join("bin");
    std::fs::create_dir(&bin).unwrap();
    std::fs::write(bin.join("mise"), "#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(bin.join("mise"), std::fs::Permissions::from_mode(0o755)).unwrap();

    let output = cargo_bin_cmd!("workon")
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join(".config"))
        .env("PATH", format!("{}:/usr/bin:/bin", bin.display()))
        .env_remove("MISE_SHELL")
        .env_remove("__MISE_DIFF")
        .env_remove("__MISE_SESSION")
        .current_dir(&proj)
        .args(["create", "--json"])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("Warning: mise shims directory is not on your PATH"), "{stderr}");
}
