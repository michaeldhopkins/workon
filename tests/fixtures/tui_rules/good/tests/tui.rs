use std::time::{Duration, Instant};

#[test]
fn launches() {
    let stubs = Stubs::new();
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_workon"));
    cmd.env_clear();
    cmd.env("PATH", stubs.path());
    let start = Instant::now();
    loop {
        if done() {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(20));
    }
}
