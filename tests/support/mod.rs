//! A fake world for running the built `workon`: a temp HOME, a project repo with an
//! `origin`, and a stub directory that is the whole of PATH (plus `/usr/bin:/bin` for the
//! stubs' own `cat` and `mv`). Every program `workon` can run (`tests/tui.toml` `programs`)
//! has a stub that records its argv and answers like the real one would on a quiet machine.
//! `git` and `rm` record and then run the real `/usr/bin/git` and `/bin/rm`: workon's
//! worktree handling is the thing under test, and a fake git would only test the fake.
//! The method is the `tui-testing` skill's.

pub mod pty;

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// Every program the stub directory holds, from `tests/tui.toml`.
pub fn programs() -> Vec<String> {
    let text = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/tui.toml")).expect("tui.toml");
    let manifest: toml::Value = toml::from_str(&text).expect("tui.toml parses");
    let mut names: Vec<String> = ["programs", "layout_commands"]
        .iter()
        .flat_map(|key| manifest[key].as_array().expect("a list").clone())
        .map(|p| p.as_str().expect("a program name").to_string())
        .collect();
    names.sort();
    names.dedup();
    names
}

/// What each stub does after recording its call. Anything not listed exits 0 silently.
fn default_body(name: &str) -> &'static str {
    match name {
        "git" => "exec /usr/bin/git \"$@\"",
        "rm" => "exec /bin/rm \"$@\"",
        // Failing `jj --version` reads as "jj is not installed", so workon takes its git
        // backend and no fake jj ever answers a real jj question.
        "jj" => "exit 1",
        "zellij" => {
            "case \"$1\" in\n\
             --version) echo 'zellij 0.43.1' ;;\n\
             list-sessions) echo 'No active zellij sessions found.' >&2; exit 1 ;;\n\
             esac"
        }
        "branchdiff" => "[ \"$1\" = --version ] && echo 'branchdiff 0.60.0'; exit 0",
        "claude" => "[ \"$1\" = --version ] && echo '2.1.0 (Claude Code)'; exit 0",
        "mise" => "[ \"$1\" = env ] && echo '{}'; exit 0",
        "id" => "echo 501",
        // No match, as on a machine with no zellij server running.
        "pgrep" | "pkill" => "exit 1",
        _ => "exit 0",
    }
}

pub struct Stubs {
    dir: tempfile::TempDir,
}

impl Stubs {
    fn new() -> Self {
        let stubs = Self { dir: tempfile::tempdir().expect("a stub dir") };
        for name in programs() {
            stubs.set(&name, default_body(&name));
        }
        stubs
    }

    /// Replace what `name` does after recording its call (a shell snippet; `$@` is its argv).
    pub fn set(&self, name: &str, body: &str) {
        use std::os::unix::fs::PermissionsExt;
        // One line per call: the working directory, then each argument, separated by US
        // (0x1f). Built in a temp file and appended in one go, so a reader never sees half a call.
        let log = self.dir.path().join(format!("{name}.calls"));
        let script = format!(
            "#!/bin/sh\nt=\"{log}.$$\"\nprintf '%s\\037' \"$(pwd -P)\" > \"$t\"\n\
             for a in \"$@\"; do printf '%s\\037' \"$a\"; done >> \"$t\"\n\
             printf '\\n' >> \"$t\"\ncat \"$t\" >> \"{log}\"\n/bin/rm -f \"$t\"\n{body}\n",
            log = log.display(),
        );
        let path = self.dir.path().join(name);
        // Written by a short-lived `sh`, not this process: a stub open for writing here can be
        // inherited by a sibling test thread's fork, and the built binary execs it without
        // retry, so Linux would answer ETXTBSY ("Text file busy").
        let status = Command::new("/bin/sh")
            .args(["-c", "printf %s \"$WORKON_STUB\" > \"$1\"", "workon stub writer"])
            .arg(&path)
            .env("WORKON_STUB", script)
            .status()
            .expect("a stub writer");
        assert!(status.success(), "writing the {name} stub: {status}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("executable");
    }

    pub fn path(&self) -> String {
        format!("{}:/usr/bin:/bin", self.dir.path().display())
    }

    /// Each call `name` received, in order.
    pub fn calls(&self, name: &str) -> Vec<Call> {
        std::fs::read_to_string(self.dir.path().join(format!("{name}.calls")))
            .unwrap_or_default()
            .lines()
            .map(|line| {
                let mut fields: Vec<String> = line.split('\u{1f}').map(String::from).collect();
                fields.pop(); // the empty field after the last separator
                let cwd = PathBuf::from(fields.remove(0));
                Call { cwd, args: fields }
            })
            .collect()
    }

    /// The one call to `name` whose first argument is `first`.
    pub fn call(&self, name: &str, first: &str) -> Call {
        let calls = self.calls(name);
        let matching: Vec<&Call> = calls.iter().filter(|c| c.args.first().is_some_and(|a| a == first)).collect();
        assert_eq!(matching.len(), 1, "one `{name} {first}` expected; calls: {calls:?}");
        matching[0].clone()
    }
}

#[derive(Clone, Debug)]
pub struct Call {
    pub cwd: PathBuf,
    pub args: Vec<String>,
}

pub struct World {
    root: tempfile::TempDir,
    pub stubs: Stubs,
}

impl World {
    pub fn new() -> Self {
        let root = tempfile::tempdir().expect("a world");
        std::fs::create_dir_all(root.path().join("home")).expect("a home");
        // workon commits rescued work, so the identity it commits under lives in the fake HOME.
        std::fs::write(
            root.path().join("home/.gitconfig"),
            "[user]\n\tname = Test\n\temail = test@example.com\n[init]\n\tdefaultBranch = main\n\
             [commit]\n\tgpgsign = false\n",
        )
        .expect("a gitconfig");
        Self { root, stubs: Stubs::new() }
    }

    /// The canonical root: macOS's temp dir is a symlink (`/var` -> `/private/var`), and
    /// workon reports canonical paths.
    pub fn path(&self) -> PathBuf {
        self.root.path().canonicalize().expect("canonical")
    }

    pub fn home(&self) -> PathBuf {
        self.path().join("home")
    }

    pub fn worktrees(&self) -> PathBuf {
        self.home().join(".worktrees")
    }

    /// The only environment `workon` sees.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![
            ("TERM".into(), "xterm-256color".into()),
            ("HOME".into(), self.home().display().to_string()),
            ("XDG_CONFIG_HOME".into(), self.home().join(".config").display().to_string()),
            ("ZELLIJ_SOCKET_DIR".into(), self.path().join("zellij-sockets").display().to_string()),
            ("GIT_CONFIG_NOSYSTEM".into(), "1".into()),
            ("PATH".into(), self.stubs.path()),
        ]
    }

    /// The built binary, run piped in `cwd` with only [`World::env`].
    pub fn workon(&self, cwd: &Path) -> assert_cmd::Command {
        let mut cmd = assert_cmd::Command::new(env!("CARGO_BIN_EXE_workon"));
        cmd.env_clear();
        cmd.envs(self.env());
        cmd.current_dir(cwd);
        cmd
    }

    /// The built binary on a pseudo-terminal in `cwd`.
    pub fn tui(&self, cwd: &Path, args: &[&str]) -> pty::Tui {
        pty::Tui::spawn(self, args, cwd)
    }

    /// The real git, run by the test (not by workon), with the world's HOME.
    pub fn git(&self, dir: &Path, args: &[&str]) -> String {
        let out = Command::new("/usr/bin/git")
            .args(args)
            .current_dir(dir)
            .env("HOME", self.home())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .stdin(Stdio::null())
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A project repo `proj` holding `files`, committed on `main` and pushed to a bare
    /// `origin`: workon's git backend branches from `origin/<trunk>`.
    pub fn project(&self, files: &[(&str, &str)]) -> PathBuf {
        let proj = self.path().join("proj");
        std::fs::create_dir_all(&proj).expect("a project");
        self.git(&proj, &["init", "-q", "-b", "main"]);
        for (rel, content) in files {
            let path = proj.join(rel);
            std::fs::create_dir_all(path.parent().expect("a parent")).expect("dirs");
            std::fs::write(path, content).expect("a file");
        }
        self.git(&proj, &["add", "-A"]);
        self.git(&proj, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let origin = self.path().join("origin.git");
        self.git(&self.path(), &["init", "-q", "--bare", origin.to_str().expect("utf-8")]);
        self.git(&proj, &["remote", "add", "origin", origin.to_str().expect("utf-8")]);
        self.git(&proj, &["push", "-q", "origin", "main"]);
        proj
    }
}
