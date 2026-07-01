use assert_cmd::cargo::cargo_bin_cmd;
use predicates::prelude::*;

#[test]
fn version_flag() {
    cargo_bin_cmd!("workon")
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains("workon 0."));
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
    cargo_bin_cmd!("workon")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--config"));
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
        .stdout(predicate::str::contains("list"));
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
    cargo_bin_cmd!("workon")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--name"));
}

#[test]
fn help_lists_new_session_long_flag() {
    cargo_bin_cmd!("workon")
        .arg("--help")
        .assert()
        .success()
        .stdout(predicate::str::contains("--new-session"));
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
