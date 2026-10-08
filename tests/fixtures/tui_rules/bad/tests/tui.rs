use std::time::Duration;

#[test]
fn quits() {
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_workon"));
    cmd.envs([("PATH", std::env::var("PATH").unwrap())]);
    std::thread::sleep(Duration::from_millis(50));
    assert_cmd::Command::cargo_bin("workon").unwrap();
    assert_cmd::cargo::cargo_bin_cmd!("workon").assert();
}

fn env_inside_a_macro() -> Vec<(&'static str, String)> {
    vec![("PATH", "/usr/bin".to_string())]
}
