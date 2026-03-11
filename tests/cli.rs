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
