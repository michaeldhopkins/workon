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
        .args(["--skip-copy-ignored", "some-project"])
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
fn missing_named_config_errors_cleanly() {
    let tmp = tempfile::tempdir().unwrap();
    cargo_bin_cmd!("workon")
        .env("XDG_CONFIG_HOME", tmp.path())
        .args(["--config", "no-such-config", "."])
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
        .args(["--config", "../etc/hosts", "."])
        .assert()
        .failure()
        .stderr(predicate::str::contains("invalid config name"));
}
