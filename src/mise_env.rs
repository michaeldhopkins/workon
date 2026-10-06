//! The environment `mise env` computes for a directory, handed to the session and
//! to provisioners.

use std::collections::HashMap;
use std::path::Path;

use vcs_runner::Cmd;

/// `<program> env --json` for `dir`, where `program` is `mise` (a test passes a
/// stand-in). Empty when mise is absent or fails, since a project without mise is
/// normal; a warning when mise ran but printed something that is not the JSON
/// object it documents, so a lost env is never silent.
///
/// `--json` exists in every release named `mise` (2024.1.0 onward; checked
/// against `src/cli/env.rs` at that tag), so no version fallback is needed.
pub fn mise_env(program: &str, dir: &Path) -> HashMap<String, String> {
    let shims = shims_dirs(&|var| std::env::var(var).ok());
    mise_env_from(program, dir, std::env::var("PATH").ok().as_deref(), &shims)
}

/// [`mise_env`] with the PATH it was started with and mise's shims directories given, so a test
/// can supply them.
///
/// mise puts its directories (`[env] _.path` entries, then each tool's) in front of PATH, except
/// when one of its shims directories is on PATH: then it inserts them just before the shims.
/// With the shims behind `/usr/bin` (`path_helper` puts the system directories first), `bundle`
/// and `ruby` ran from `/usr/bin` on the system Ruby (issue #2). So mise is run without its shims
/// on PATH, and they go back where they stood among the other entries, after mise's own.
/// Measured on mise 2026.2.21 and 2026.10.3; the second also drops duplicate entries and, in an
/// activated shell, the directories it activated before, so its PATH is used as it comes rather
/// than compared with the one given.
///
/// The cost: an `[env] _.source` script no longer finds mise's tools through the shims while
/// mise computes the env. mise's way for one to see them is `_.source = { path = "…", tools =
/// true }`. `exec()` templates are unaffected: mise already leaves its shims off their PATH.
fn mise_env_from(program: &str, dir: &Path, path: Option<&str>, shims: &[String]) -> HashMap<String, String> {
    let mut cmd = Cmd::new(program).args(["env", "--json"]).in_dir(dir);
    let given = path.map(|p| without_shims(p, shims));
    if let Some(given) = &given {
        cmd = cmd.env("PATH", given);
    }
    let Ok(output) = cmd.run() else {
        return HashMap::new();
    };
    let mut vars = parse(&output.stdout_lossy()).unwrap_or_else(|e| {
        eprintln!("Warning: could not read `mise env --json` output ({e}); the session starts without mise's env");
        HashMap::new()
    });
    if let (Some(from_mise), Some(path), Some(given)) = (vars.get_mut("PATH"), path, given) {
        // Given an empty PATH, mise ends its own with `:`, an empty entry that means the current
        // directory.
        if given.is_empty() {
            *from_mise = from_mise.trim_end_matches(':').to_string();
        }
        *from_mise = restore_shims(from_mise, path, shims);
    }
    vars
}

/// `from_mise` with each shims entry of `path` put back where it stood: before the first entry
/// that followed it in `path` and is still in `from_mise`, or at the end when none is. Shims a
/// user put ahead of Homebrew stay ahead of it; shims behind `/usr/bin` stay behind it.
///
/// The anchor is the entry that followed in `path`. If mise moved that entry forward
/// (deduplicating, or undoing an activation), the shims follow it, though still behind mise's
/// `_.path` entries. Harmless: a shim runs the version mise picked anyway.
fn restore_shims(from_mise: &str, path: &str, shims: &[String]) -> String {
    let given: Vec<&str> = path.split(':').collect();
    let mut out: Vec<&str> = if from_mise.is_empty() { Vec::new() } else { from_mise.split(':').collect() };
    let mut back = shims_on(path, shims);
    back.sort_by_key(|entry| given.iter().position(|e| e == entry));
    for entry in back {
        let at = given.iter().position(|e| *e == entry).unwrap_or(given.len());
        let next = given[at..]
            .iter()
            .filter(|e| !shims.iter().any(|s| is_shims(e, s)))
            .find_map(|e| out.iter().position(|o| o == e));
        match next {
            Some(i) => out.insert(i, entry),
            None => out.push(entry),
        }
    }
    out.join(":")
}

/// The shims directories mise anchors its PATH on, as `mise doctor` lists them: `shims` is
/// `$MISE_SHIMS_DIR`, else `shims` under `$MISE_DATA_DIR`, `$XDG_DATA_HOME/mise` or
/// `$HOME/.local/share/mise`, in that order; `system_shims` is `shims` under `$MISE_SYSTEM_DATA_DIR`,
/// else `/usr/local/share/mise`.
fn shims_dirs(env: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let system = env("MISE_SYSTEM_DATA_DIR").filter(|v| !v.is_empty());
    let system = shims_under(system.as_deref().unwrap_or("/usr/local/share/mise"));
    user_shims_dir(env).into_iter().chain([system]).collect()
}

/// The `shims` directory of [`shims_dirs`], the one a user's own tools go through. A leading `~`
/// in the variables is expanded, as mise does.
fn user_shims_dir(env: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    let non_empty = |var: &str| env(var).filter(|v| !v.is_empty());
    let home = non_empty("HOME");
    let dir = non_empty("MISE_SHIMS_DIR").map(|d| d.trim_end_matches('/').to_string()).or_else(|| {
        non_empty("MISE_DATA_DIR")
            .or_else(|| non_empty("XDG_DATA_HOME").map(|d| format!("{d}/mise")))
            .or_else(|| home.as_ref().map(|h| format!("{h}/.local/share/mise")))
            .map(|d| shims_under(&d))
    })?;
    Some(expand_tilde(&dir, home.as_deref()))
}

fn shims_under(data_dir: &str) -> String {
    format!("{}/shims", data_dir.trim_end_matches('/'))
}

/// A leading `~`, alone or before a `/`, with `home` put in its place; anything else, `~user` included, as it is.
fn expand_tilde(path: &str, home: Option<&str>) -> String {
    match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with('/') => format!("{home}{rest}"),
        _ => path.to_string(),
    }
}

/// Whether a PATH entry names the directory `shims`. mise compares directories, not spellings:
/// measured on 2026.10.3, it anchors on a tilde-prefixed spelling, repeated slashes, `.`
/// components and symlinked spellings of its shims, and expands `~` in `MISE_SHIMS_DIR` and `MISE_DATA_DIR`.
fn is_shims(entry: &str, shims: &str) -> bool {
    let home = std::env::var("HOME").ok();
    !entry.is_empty() && dir_key(entry, home.as_deref()) == dir_key(shims, home.as_deref())
}

/// A directory's identity: `~` expanded, then the real path when it exists, else the path with
/// repeated slashes, `.` components and a trailing slash removed. A relative path resolves
/// against workon's working directory, not the workspace mise runs in; mise's own shims are
/// absolute unless a variable makes them otherwise.
fn dir_key(path: &str, home: Option<&str>) -> String {
    let expanded = expand_tilde(path, home);
    if let Ok(real) = std::fs::canonicalize(&expanded) {
        return real.to_string_lossy().into_owned();
    }
    let parts: Vec<&str> = expanded.split('/').filter(|p| !p.is_empty() && *p != ".").collect();
    let lead = if expanded.starts_with('/') { "/" } else { "" };
    format!("{lead}{}", parts.join("/"))
}

/// `path` without mise's shims directories, the other entries untouched.
fn without_shims(path: &str, shims: &[String]) -> String {
    path.split(':').filter(|e| !shims.iter().any(|s| is_shims(e, s))).collect::<Vec<_>>().join(":")
}

/// The first entry of `path` naming each shims directory that is on it, spelled as `path` spells
/// it, in the order of `shims`, each entry once (the user and system shims can be one directory).
fn shims_on<'a>(path: &'a str, shims: &[String]) -> Vec<&'a str> {
    let mut found: Vec<&str> = Vec::new();
    for entry in shims.iter().filter_map(|s| path.split(':').find(|e| is_shims(e, s))) {
        if !found.contains(&entry) {
            found.push(entry);
        }
    }
    found
}

/// JSON, not the shell form: that quotes each value for a shell (`'it'\''s'`) and
/// prints a multi-line value over several lines, so reading it back needs a shell.
/// Non-string values are skipped; mise only emits strings.
fn parse(output: &str) -> Result<HashMap<String, String>, serde_json::Error> {
    let map: HashMap<String, serde_json::Value> = serde_json::from_str(output)?;
    Ok(map.into_iter().filter_map(|(k, v)| v.as_str().map(|s| (k, s.to_string()))).collect())
}

/// Whether `mise activate` is live in the current environment. Activation exports
/// these markers; their presence means mise is already managing PATH (the install
/// bin dirs are injected directly), so the shims check below would be a false
/// alarm — `which ruby` resolves correctly without shims on PATH.
fn mise_activated() -> bool {
    ["MISE_SHELL", "__MISE_DIFF", "__MISE_SESSION"].iter().any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
}

/// Warn only when tool versions could actually go unresolved: adding shims to
/// PATH would help (the dir exists and isn't already on PATH) *and* `mise
/// activate` isn't already handling it. Pure so the (otherwise I/O-bound)
/// decision can be tested directly.
fn should_warn_mise_shims(activated: bool, shims_would_help: bool) -> bool {
    !activated && shims_would_help
}

/// Warn if mise shims aren't on PATH *and* `mise activate` isn't handling it.
/// Without either, non-interactive shells (like those spawned by Claude Code)
/// won't resolve the correct tool versions.
pub(crate) fn warn_mise_shims() {
    let Some(shims_dir) = user_shims_dir(&|var| std::env::var(var).ok()) else {
        return;
    };
    let on_path = std::env::var("PATH").unwrap_or_default().split(':').any(|p| is_shims(p, &shims_dir));
    let shims_would_help = Path::new(&shims_dir).is_dir() && !on_path;

    if should_warn_mise_shims(mise_activated(), shims_would_help) {
        eprintln!();
        eprintln!("Warning: mise shims directory is not on your PATH, and");
        eprintln!("`mise activate` isn't set up either. Non-interactive shells");
        eprintln!("(e.g. Claude Code) may not pick up the correct tool versions.");
        eprintln!();
        // .zshenv, not .zshrc: only .zshenv is sourced by non-interactive zsh,
        // which is exactly the context this warning is about.
        eprintln!("Add this to ~/.zshenv (sourced by non-interactive shells too):");
        eprintln!();
        let home = std::env::var("HOME").unwrap_or_default();
        let shown = match shims_dir.strip_prefix(&home) {
            Some(rest) if !home.is_empty() && rest.starts_with('/') => format!("$HOME{rest}"),
            _ => shims_dir.clone(),
        };
        eprintln!("  export PATH=\"{shown}:$PATH\"");
        eprintln!();
        eprintln!("workon will inject the correct env vars for this session,");
        eprintln!("but fixing your shell profile avoids the issue everywhere.");
        eprintln!();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn should_warn_mise_shims_only_when_genuinely_missing() {
        // The fix: `mise activate` being live suppresses the warning even when
        // shims would otherwise help — that was the false alarm being reported.
        assert!(!should_warn_mise_shims(true, true), "activated => never warn");
        assert!(!should_warn_mise_shims(true, false), "activated => never warn");
        // Without activation, warn only when adding shims to PATH would help.
        assert!(should_warn_mise_shims(false, true), "shims would help => warn");
        assert!(!should_warn_mise_shims(false, false), "shims wouldn't help => no warn");
    }

    #[test]
    fn keeps_values_verbatim() {
        // Captured from `mise env --json` (mise 2026.2.21). The shell form of the same
        // env printed `'it'\''s`, `'say "hi"'` and a value split over two lines,
        // which the old line parser read back as `it'\''s`, `say "hi` and `line1`.
        let output = r#"{
  "PATH": "/usr/local/bin:/usr/bin",
  "W_DQUOTE": "say \"hi\"",
  "W_EMPTY": "",
  "W_NL": "line1\nline2",
  "W_SQUOTE": "it's",
  "W_TRAILQ": "x'"
}"#;
        let vars = parse(output).unwrap();
        assert_eq!(vars.get("PATH").unwrap(), "/usr/local/bin:/usr/bin");
        assert_eq!(vars.get("W_DQUOTE").unwrap(), "say \"hi\"");
        assert_eq!(vars.get("W_EMPTY").unwrap(), "");
        assert_eq!(vars.get("W_NL").unwrap(), "line1\nline2");
        assert_eq!(vars.get("W_SQUOTE").unwrap(), "it's");
        assert_eq!(vars.get("W_TRAILQ").unwrap(), "x'");
        assert_eq!(vars.len(), 6);
    }

    #[test]
    fn skips_non_strings() {
        let vars = parse(r#"{"A": "a", "B": 1, "C": null}"#).unwrap();
        assert_eq!(vars, HashMap::from([("A".to_string(), "a".to_string())]));
    }

    /// A stand-in for `mise` that prints its arguments and working directory as the
    /// JSON `mise env --json` prints, so the test sees what was run and where.
    fn stand_in(dir: &Path, body: &str) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("mise");
        std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn runs_env_json_in_the_directory_and_returns_its_vars() {
        let bin = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        let mise = stand_in(bin.path(), r#"printf '{"ARGS": "%s", "WHERE": "%s", "N": 1}' "$*" "$(pwd -P)""#);

        let vars = mise_env(mise.to_str().unwrap(), project.path());

        let here = std::fs::canonicalize(project.path()).unwrap();
        let expected = HashMap::from([
            ("ARGS".to_string(), "env --json".to_string()),
            ("WHERE".to_string(), here.to_string_lossy().into_owned()),
        ]);
        assert_eq!(vars, expected);
    }

    fn executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Issue #2. With the shims directory on PATH behind `/usr/bin` (`path_helper` moves the
    /// system directories to the front), mise inserts its directories where the shims are, so
    /// `bundle` ran from `/usr/bin` on the system Ruby and the schema load crashed. Reproduced
    /// 2026-10-05 (mise 2026.2.21) and 2026-10-06 (2026.10.3) with `[env] _.path = ["./bin"]`:
    /// `PATH=/usr/bin:/bin:<shims> mise env --json` printed
    /// `/usr/bin:/bin:<project>/bin:$HOME/.local/share/mise/installs/ruby/4.0.7/bin:<shims>`, and
    /// `PATH=/usr/bin:/bin` printed `<project>/bin:<ruby>/bin:/usr/bin:/bin`. The stand-in
    /// prepends its directories, as mise does without the shims, and fails if it sees them.
    #[test]
    fn mise_directories_come_before_the_system_ones_in_mise_order() {
        let root = tempfile::tempdir().unwrap();
        let system = root.path().join("usr/bin");
        let project_bin = root.path().join("project/bin");
        let ruby = root.path().join("mise/installs/ruby/4.0.7/bin");
        let shims = root.path().join("mise/shims");
        executable(&system.join("bundle"));
        executable(&ruby.join("bundle"));
        let (system, project_bin, ruby, shims) =
            (system.display(), project_bin.display(), ruby.display(), shims.display());
        let bin = tempfile::tempdir().unwrap();
        let mise = stand_in(
            bin.path(),
            &format!(
                r#"case ":$PATH:" in *":{shims}:"*) exit 1 ;; esac; printf '{{"PATH": "%s"}}' "{project_bin}:{ruby}:$PATH""#
            ),
        );

        let given = format!("{system}:/bin:{shims}");
        let vars = mise_env_from(mise.to_str().unwrap(), root.path(), Some(&given), &[shims.to_string()]);

        assert_eq!(vars["PATH"], format!("{project_bin}:{ruby}:{system}:/bin:{shims}"));
        let found = std::process::Command::new("/bin/sh")
            .args(["-c", "command -v bundle"])
            .env("PATH", &vars["PATH"])
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&found.stdout).trim(), format!("{ruby}/bundle"));
    }

    #[test]
    fn every_shims_directory_comes_off_and_goes_back_where_it_stood() {
        let bin = tempfile::tempdir().unwrap();
        // Fails if mise is shown any shims directory, as the real one would anchor on it.
        let mise = stand_in(
            bin.path(),
            r#"case ":$PATH:" in *:/m/shims:*|*:/m/shims/:*|*:/sys/shims:*|*:/sys/shims/:*) exit 1 ;; esac; printf '{"PATH": "/tools/bin:%s"}' "$PATH""#,
        );
        let shims = ["/m/shims".to_string(), "/sys/shims".to_string()];
        let run = |path: Option<&str>| mise_env_from(mise.to_str().unwrap(), bin.path(), path, &shims)["PATH"].clone();
        assert_eq!(run(Some("/usr/bin:/bin")), "/tools/bin:/usr/bin:/bin");
        assert_eq!(run(Some("/usr/bin:/sys/shims:/bin")), "/tools/bin:/usr/bin:/sys/shims:/bin");
        assert_eq!(
            run(Some("/m/shims:/opt/homebrew/bin:/usr/bin")),
            "/tools/bin:/m/shims:/opt/homebrew/bin:/usr/bin",
            "shims a user put first stay ahead of Homebrew, behind mise's own"
        );
        assert_eq!(
            run(Some("/sys/shims:/m//shims/:/usr/bin")),
            "/tools/bin:/sys/shims:/m//shims/:/usr/bin",
            "another spelling is still the shims, and goes back on as it was spelled"
        );
        assert_eq!(run(Some("/m/shimsx:/usr/bin")), "/tools/bin:/m/shimsx:/usr/bin", "a near miss is not the shims");
        let same = ["/m/shims".to_string(), "/m/shims".to_string()];
        assert_eq!(
            mise_env_from(mise.to_str().unwrap(), bin.path(), Some("/usr/bin:/m/shims"), &same)["PATH"],
            "/tools/bin:/usr/bin:/m/shims",
            "user and system shims that are one directory go back once"
        );
        // Only the shims, or nothing: mise ends its PATH with an empty entry, the current
        // directory, which is dropped.
        assert_eq!(run(Some("/m/shims")), "/tools/bin:/m/shims");
        assert_eq!(run(Some("")), "/tools/bin");
    }

    #[test]
    fn dir_key_names_the_directory_not_the_spelling() {
        let home = Some("/h");
        for spelling in ["/h/m/shims", "/h/m/shims/", "/h//m/shims", "/h/m/./shims", "~/m/shims", "~//m/shims/"] {
            assert_eq!(dir_key(spelling, home), "/h/m/shims", "{spelling}");
        }
        assert_eq!(dir_key("~", home), "/h");
        assert_eq!(dir_key("~user/m", home), "~user/m", "another user's home is not expanded");
        assert_eq!(dir_key("~/m", None), "~/m");
        assert_ne!(dir_key("/h/m/shimsx", home), "/h/m/shims");

        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real/shims");
        std::fs::create_dir_all(&real).unwrap();
        let link = root.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        assert_eq!(dir_key(&link.display().to_string(), home), dir_key(&real.display().to_string(), home), "a symlink");
    }

    #[test]
    fn is_shims_expands_a_tilde_on_either_side() {
        let home = std::env::var("HOME").unwrap_or_default();
        if home.is_empty() {
            return eprintln!("skipped: no HOME");
        }
        assert!(is_shims(&format!("{home}/workon-test-xyz/shims"), "~/workon-test-xyz/shims"), "MISE_SHIMS_DIR='~/…'");
        assert!(is_shims("~/workon-test-xyz/shims", &format!("{home}/workon-test-xyz/shims")), "a literal ~ on PATH");
        assert!(!is_shims("", "/m/shims"));
    }

    #[test]
    fn without_a_path_mise_inherits_the_process_one() {
        let bin = tempfile::tempdir().unwrap();
        let mise = stand_in(bin.path(), r#"printf '{"PATH": "/tools/bin:%s"}' "$PATH""#);
        let vars = mise_env_from(mise.to_str().unwrap(), bin.path(), None, &["/m/shims".to_string()]);
        assert_eq!(vars["PATH"], format!("/tools/bin:{}", std::env::var("PATH").unwrap_or_default()));
    }

    #[test]
    fn shims_dirs_follow_mises_order() {
        let env = |vars: &'static [(&'static str, &'static str)]| {
            move |name: &str| vars.iter().find(|(k, _)| *k == name).map(|(_, v)| v.to_string())
        };
        const ALL: &[(&str, &str)] = &[
            ("MISE_SHIMS_DIR", "/s/"),
            ("MISE_DATA_DIR", "/d/"),
            ("XDG_DATA_HOME", "/x"),
            ("HOME", "/h"),
            ("MISE_SYSTEM_DATA_DIR", "/sys"),
        ];
        assert_eq!(shims_dirs(&env(ALL)), ["/s", "/sys/shims"]);
        assert_eq!(shims_dirs(&env(&ALL[1..])), ["/d/shims", "/sys/shims"]);
        assert_eq!(shims_dirs(&env(&ALL[2..4])), ["/x/mise/shims", "/usr/local/share/mise/shims"]);
        assert_eq!(shims_dirs(&env(&ALL[3..4])), ["/h/.local/share/mise/shims", "/usr/local/share/mise/shims"]);
        assert_eq!(
            shims_dirs(&env(&[("MISE_SHIMS_DIR", ""), ("MISE_DATA_DIR", ""), ("HOME", "/h")])),
            ["/h/.local/share/mise/shims", "/usr/local/share/mise/shims"],
            "an empty variable is unset"
        );
        assert_eq!(
            shims_dirs(&env(&[("MISE_DATA_DIR", "~/d"), ("HOME", "/h")])),
            ["/h/d/shims", "/usr/local/share/mise/shims"]
        );
        assert_eq!(
            shims_dirs(&env(&[("MISE_SHIMS_DIR", "~/s"), ("HOME", "/h")])),
            ["/h/s", "/usr/local/share/mise/shims"]
        );
        assert_eq!(shims_dirs(&env(&[])), ["/usr/local/share/mise/shims"]);
        assert_eq!(user_shims_dir(&env(&[])), None);
    }

    proptest::proptest! {
        /// Removing the shims removes every copy of each and nothing else, in order: not a near
        /// miss (`/sa`, `/s/a`) and not an empty entry.
        #[test]
        fn without_shims_removes_only_the_shims(
            entries in proptest::collection::vec("(/s|/s/|/t|/t/|/s[a-c]|/s/[a-c]|/[a-c]{0,2}|)", 1..8),
        ) {
            let path = entries.join(":");
            let shims = ["/s".to_string(), "/t".to_string()];
            let out = without_shims(&path, &shims);
            let kept: Vec<&str> = entries.iter().map(String::as_str).filter(|e| !["/s", "/s/", "/t", "/t/"].contains(e)).collect();
            proptest::prop_assert_eq!(&out, &kept.join(":"));
            proptest::prop_assert_eq!(without_shims(&path, &[]), path);
        }

        /// Putting the shims back changes nothing but where they are: without them the result is
        /// mise's PATH exactly, each shims directory on the given PATH comes back once, and one
        /// that preceded an entry still precedes it.
        #[test]
        fn restore_shims_only_adds_the_shims_in_their_places(
            given in proptest::collection::vec("(/s|/t|/a|/b|/c)", 0..7),
            prepended in proptest::collection::vec("(/m|/n)", 0..3),
        ) {
            let shims = ["/s".to_string(), "/t".to_string()];
            let path = given.join(":");
            let rest: Vec<&str> = given.iter().map(String::as_str).filter(|e| *e != "/s" && *e != "/t").collect();
            let from_mise = prepended.iter().map(String::as_str).chain(rest.iter().copied()).collect::<Vec<_>>().join(":");
            let out = restore_shims(&from_mise, &path, &shims);
            proptest::prop_assert_eq!(without_shims(&out, &shims), from_mise.clone());
            let entries: Vec<&str> = out.split(':').filter(|e| !e.is_empty()).collect();
            for s in ["/s", "/t"] {
                let expected = usize::from(given.iter().any(|e| e == s));
                proptest::prop_assert_eq!(entries.iter().filter(|e| **e == s).count(), expected);
                let Some(at) = given.iter().position(|e| e == s) else { continue };
                if let Some(next) = given[at..].iter().find(|e| *e != "/s" && *e != "/t") {
                    let (si, ni) = (entries.iter().position(|e| *e == s), entries.iter().position(|e| e == next));
                    proptest::prop_assert!(si < ni, "{s} before {next} in {out}");
                }
            }
        }
    }

    /// The same against the real mise, from PATHs shaped like issue #2's: the shims behind
    /// `/usr/bin` and a duplicate entry, with the shell's mise activation markers cleared and
    /// kept. The project pins the Ruby the repo's `mise.toml` pins and has `_.path = ["./bin"]`,
    /// which must stay ahead of the tools. Skips without mise or a system `bundle` (Linux CI has
    /// neither), or when that Ruby is not installed; the reason is printed, and shown with
    /// `--nocapture`.
    #[test]
    fn real_mise_puts_its_directories_ahead_of_the_system_ones() {
        let sh = |script: &str, path: &str| {
            let out = std::process::Command::new("/bin/sh").args(["-c", script]).env("PATH", path).output().unwrap();
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        let mise = sh("command -v mise", &std::env::var("PATH").unwrap_or_default());
        if mise.is_empty() {
            return eprintln!("skipped: no mise on PATH");
        }
        if !Path::new("/usr/bin/bundle").exists() {
            return eprintln!("skipped: no /usr/bin/bundle to shadow mise's");
        }
        let repo_pins = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/mise.toml")).unwrap();
        let ruby_pin = repo_pins.lines().find(|l| l.starts_with("ruby = ")).expect("the repo pins Ruby");
        let project = tempfile::tempdir().unwrap();
        let project = std::fs::canonicalize(project.path()).unwrap();
        std::fs::create_dir(project.join("bin")).unwrap();
        std::fs::write(project.join("mise.toml"), format!("[tools]\n{ruby_pin}\n[env]\n_.path = [\"./bin\"]\n"))
            .unwrap();
        let shims = shims_dirs(&|var| std::env::var(var).ok());
        let given = format!("/usr/bin:/bin:/usr/bin:/usr/sbin:/sbin:{}", shims.join(":"));
        let bin = tempfile::tempdir().unwrap();
        let trust = format!("export MISE_TRUSTED_CONFIG_PATHS='{}'", project.display());
        let kept = stand_in(bin.path(), &format!("{trust}; exec '{mise}' \"$@\""));
        let cleared_dir = tempfile::tempdir().unwrap();
        let cleared = stand_in(
            cleared_dir.path(),
            &format!("{trust}; unset MISE_SHELL __MISE_DIFF __MISE_SESSION __MISE_ORIG_PATH; exec '{mise}' \"$@\""),
        );

        for (run, program) in [&kept, &cleared].into_iter().enumerate() {
            let vars = mise_env_from(program.to_str().unwrap(), &project, Some(&given), &shims);
            let Some(path) = vars.get("PATH").filter(|p| p.contains("/installs/ruby/")) else {
                // Only the first run may skip: the second differs in activation markers alone.
                assert_eq!(run, 0, "{}: no Ruby, though the run with the markers kept had one", program.display());
                return eprintln!("skipped: mise gave no Ruby for {ruby_pin} (not installed?)");
            };
            let entries: Vec<&str> = path.split(':').collect();
            let at = |want: &dyn Fn(&str) -> bool| entries.iter().position(|e| want(e)).unwrap_or(usize::MAX);
            let project_bin = project.join("bin");
            let (own, ruby, system) = (
                at(&|e| Path::new(e) == project_bin),
                at(&|e| e.contains("/installs/ruby/")),
                at(&|e| e == "/usr/bin"),
            );
            assert!(own < ruby && ruby < system, "{}: _.path, then Ruby, then /usr/bin: {path}", program.display());
            let bundle = sh("command -v bundle", path);
            assert!(
                bundle.contains("/installs/ruby/"),
                "{}: bundle resolved to {bundle} from {path}",
                program.display()
            );
        }
    }

    #[test]
    fn a_missing_or_failing_or_garbled_mise_gives_no_vars() {
        let bin = tempfile::tempdir().unwrap();
        let project = tempfile::tempdir().unwrap();
        assert!(mise_env(bin.path().join("no-such-mise").to_str().unwrap(), project.path()).is_empty());

        let failing = stand_in(bin.path(), r#"printf '{"A": "a"}'; exit 1"#);
        assert!(mise_env(failing.to_str().unwrap(), project.path()).is_empty());

        let garbled = stand_in(bin.path(), "echo 'export A=a'");
        assert!(mise_env(garbled.to_str().unwrap(), project.path()).is_empty());
    }

    #[test]
    fn output_that_is_not_a_json_object_is_an_error() {
        assert!(parse("export A=a\n").is_err(), "shell-form output is not JSON");
        assert!(parse("").is_err());
        assert!(parse("[1]").is_err());
    }
}
