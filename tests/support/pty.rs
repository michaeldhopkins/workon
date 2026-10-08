//! `workon` in a pseudo-terminal, for what crosses the terminal: a prompt read from the
//! tty, and zellij being handed the screen. The binary is the one cargo just built, the
//! environment is cleared, and PATH is the stub directory (`super::World::env`).
//! Adapted from specdiff's `tests/support/pty.rs`.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, CommandBuilder, PtySize};

const ROWS: u16 = 30;
const COLS: u16 = 120;
/// Under cargo-mutants' per-mutant timeout, so a mutant that hangs a prompt reads as caught.
const TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(20);

pub struct Tui {
    parser: vt100::Parser,
    output: Receiver<Vec<u8>>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

impl Tui {
    pub fn spawn(world: &super::World, args: &[&str], cwd: &Path) -> Self {
        let pair = native_pty_system()
            .openpty(PtySize { rows: ROWS, cols: COLS, pixel_width: 0, pixel_height: 0 })
            .expect("a pty");
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_workon"));
        cmd.env_clear();
        for (key, value) in world.env() {
            cmd.env(key, value);
        }
        // Already in world.env(); repeated here so the rules test can see it at the spawn.
        cmd.env("PATH", world.stubs.path());
        cmd.args(args);
        cmd.cwd(cwd);
        let child = pair.slave.spawn_command(cmd).expect("workon starts");
        drop(pair.slave);

        let mut reader = pair.master.try_clone_reader().expect("a reader");
        let writer = pair.master.take_writer().expect("a writer");
        let (tx, output) = mpsc::channel();
        std::thread::spawn(move || {
            let _master = pair.master;
            let mut buf = [0u8; 4096];
            while let Ok(n) = reader.read(&mut buf) {
                if n == 0 || tx.send(buf[..n].to_vec()).is_err() {
                    break;
                }
            }
        });

        Self { parser: vt100::Parser::new(ROWS, COLS, 0), output, writer, child }
    }

    fn drain(&mut self) {
        while let Ok(bytes) = self.output.try_recv() {
            self.parser.process(&bytes);
        }
    }

    pub fn screen(&mut self) -> String {
        self.drain();
        self.parser.screen().contents()
    }

    /// Type `keys` as the user would (`"n\r"` answers a prompt).
    pub fn press(&mut self, keys: &str) {
        self.writer.write_all(keys.as_bytes()).expect("a keypress");
        self.writer.flush().expect("a flush");
    }

    pub fn wait_for_text(&mut self, text: &str) {
        let start = Instant::now();
        loop {
            let screen = self.screen();
            if screen.contains(text) {
                return;
            }
            assert!(start.elapsed() < TIMEOUT, "timed out waiting for {text:?}. Screen:\n{screen}");
            std::thread::sleep(POLL);
        }
    }

    /// Wait for workon to exit and return whether it succeeded.
    pub fn wait_for_exit(&mut self) -> bool {
        self.wait_for_exit_code() == 0
    }

    /// The exit code, once workon has exited (bounded by the same deadline).
    pub fn wait_for_exit_code(&mut self) -> u32 {
        let start = Instant::now();
        loop {
            if let Some(status) = self.child.try_wait().expect("a status") {
                self.drain();
                return status.exit_code();
            }
            let screen = self.screen();
            assert!(start.elapsed() < TIMEOUT, "workon did not exit. Screen:\n{screen}");
            std::thread::sleep(POLL);
        }
    }
}

impl Drop for Tui {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}
