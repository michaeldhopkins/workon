use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use rand::Rng;

use crate::claude_trust;
use crate::session;

pub fn ensure_jj(project_dir: &Path) -> Result<()> {
    let has_git = project_dir.join(".git").is_dir();
    let has_jj = project_dir.join(".jj").is_dir();

    if has_git && !has_jj {
        eprintln!("Initializing jj colocated repo in {}...", project_dir.display());
        run_cmd("jj", &["git", "init", "--colocate", "-R", &path_str(project_dir)])?;

        let main_branch = detect_trunk(project_dir);

        run_cmd(
            "jj",
            &[
                "bookmark",
                "track",
                &format!("{main_branch}@origin"),
                "-R",
                &path_str(project_dir),
            ],
        )?;

        run_cmd(
            "jj",
            &[
                "config",
                "set",
                "--repo",
                "remotes.origin.auto-track-bookmarks",
                "glob:*",
                "-R",
                &path_str(project_dir),
            ],
        )?;

        eprintln!("jj initialized, tracking {main_branch}@origin");
    }
    Ok(())
}

pub fn run_workspace(project_dir: &Path, project_name: &str, layout: &Path, skip_copy_ignored: bool) -> Result<()> {
    let ws_id = generate_ws_id();
    let ws_dir = home_dir()?
        .join(".worktrees")
        .join(format!("{project_name}-{ws_id}"));
    let tab_name = format!("{project_name}-{ws_id}");

    std::fs::create_dir_all(home_dir()?.join(".worktrees"))?;

    let main_branch = detect_trunk_jj(project_dir)?;

    eprintln!("Creating jj workspace {ws_id}...");
    let status = Command::new("jj")
        .args([
            "-R",
            &path_str(project_dir),
            "workspace",
            "add",
            &path_str(&ws_dir),
            "--name",
            &ws_id,
            "-r",
            &main_branch,
        ])
        .status()
        .context("failed to create jj workspace")?;

    if !status.success() {
        bail!("failed to create jj workspace");
    }

    let claude_dir = project_dir.join(".claude");
    if claude_dir.is_dir() && !ws_dir.join(".claude").exists() {
        if let Err(e) = copy_dir_recursive(&claude_dir, &ws_dir.join(".claude")) {
            eprintln!("Warning: failed to copy .claude directory: {e}");
        }
    }

    if !skip_copy_ignored {
        // Snapshot ensures the git index is in sync with jj's working copy,
        // so git ls-files --ignored returns accurate results.
        let _ = Command::new("jj")
            .args(["-R", &path_str(project_dir), "snapshot"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        if let Err(e) = copy_gitignored_files(project_dir, &ws_dir) {
            eprintln!("Warning: failed to copy gitignored files: {e}");
        }
    }

    if let Err(e) = trust_mise_configs(&ws_dir) {
        eprintln!("Warning: failed to trust mise configs: {e}");
    }

    // Resolve mise environment for the worktree so child processes
    // (including Claude Code's subshells) get the correct tool versions.
    let mise_vars = mise_env(&ws_dir);

    let mut created_db = None;
    if ws_dir.join("config/database.yml").is_file() {
        created_db = setup_rails_db(project_name, &ws_id, &ws_dir, &mise_vars);
    }

    let _ = claude_trust::approve_workspace(&ws_dir);

    session::launch(&tab_name, layout, &ws_dir, &mise_vars)?;

    cleanup(&ws_id, project_dir, &ws_dir, created_db.as_deref())
}

fn cleanup(
    ws_id: &str,
    project_dir: &Path,
    ws_dir: &Path,
    created_db: Option<&str>,
) -> Result<()> {
    eprintln!();
    eprintln!("Cleaning up workspace {ws_id}...");

    if has_uncommitted_changes(ws_id, project_dir) {
        eprintln!("Workspace has uncommitted changes.");
        eprint!("Auto-bookmark as workon/{ws_id}? [y/N] ");
        std::io::stderr().flush()?;

        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim().eq_ignore_ascii_case("y") {
            let _ = run_cmd(
                "jj",
                &[
                    "-R",
                    &path_str(project_dir),
                    "bookmark",
                    "set",
                    &format!("workon/{ws_id}"),
                    "-r",
                    &format!("{ws_id}@"),
                ],
            );
            eprintln!("Bookmarked as workon/{ws_id}");
        }
    }

    let _ = Command::new("jj")
        .args(["-R", &path_str(project_dir), "workspace", "forget", ws_id])
        .stderr(Stdio::null())
        .status();
    eprintln!("Forgot jj workspace {ws_id}");

    if let Some(db) = created_db {
        let _ = Command::new("dropdb")
            .arg(db)
            .stderr(Stdio::null())
            .status();
        eprintln!("Dropped test database {db}");
    }

    let _ = std::fs::remove_dir_all(ws_dir);
    eprintln!("Removed workspace directory");

    Ok(())
}

fn has_uncommitted_changes(ws_id: &str, project_dir: &Path) -> bool {
    Command::new("jj")
        .args([
            "-R",
            &path_str(project_dir),
            "log",
            "--ignore-working-copy",
            "-r",
            &format!("{ws_id}@"),
            "--no-graph",
            "-T",
            r#"if(empty, "", "changes")"#,
            "--limit",
            "1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .map(|o| !o.stdout.is_empty() && String::from_utf8_lossy(&o.stdout).contains("changes"))
        .unwrap_or(false)
}

fn setup_rails_db(
    project_name: &str,
    ws_id: &str,
    ws_dir: &Path,
    mise_vars: &HashMap<String, String>,
) -> Option<String> {
    let db_name = format!("{}_{}_test", project_name, ws_id).replace('-', "_");
    eprintln!("Creating test database {db_name}...");

    let ok = Command::new("createdb")
        .arg(&db_name)
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());

    if ok {
        let env_content = format!("DATABASE_URL=postgresql://localhost/{db_name}");
        let _ = std::fs::write(ws_dir.join(".env.test.local"), env_content);

        eprintln!("Loading schema...");
        let _ = Command::new("bundle")
            .args(["exec", "rails", "db:schema:load"])
            .env("RAILS_ENV", "test")
            .env(
                "DATABASE_URL",
                format!("postgresql://localhost/{db_name}"),
            )
            .envs(mise_vars)
            .current_dir(ws_dir)
            .status();

        Some(db_name)
    } else {
        eprintln!("Warning: could not create test database {db_name}");
        None
    }
}

fn copy_gitignored_files(project_dir: &Path, ws_dir: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["-C", &path_str(project_dir), "ls-files", "--others", "--ignored", "--exclude-standard"])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .context("failed to list gitignored files")?;

    if !output.status.success() {
        bail!("git ls-files failed");
    }

    let file_list = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = file_list.lines()
        .filter(|l| !l.is_empty() && !l.starts_with(".jj/"))
        .collect();
    let total = lines.len();

    if total == 0 {
        return Ok(());
    }

    eprint!("Copying gitignored files... 0/{total}");
    let mut count = 0u32;

    for (i, rel_path) in lines.iter().enumerate() {
        let dst = ws_dir.join(rel_path);
        if dst.exists() {
            continue;
        }

        let src = project_dir.join(rel_path);
        if !src.is_file() {
            continue;
        }

        if let Some(parent) = dst.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!("\rWarning: could not create directory {}: {e}", parent.display());
                eprint!("Copying gitignored files... {}/{total}", i + 1);
                continue;
            }
        }

        if let Err(e) = std::fs::copy(&src, &dst) {
            eprintln!("\rWarning: could not copy {}: {e}", rel_path);
            eprint!("Copying gitignored files... {}/{total}", i + 1);
            continue;
        }

        count += 1;

        if (i + 1) % 500 == 0 || i + 1 == total {
            eprint!("\rCopying gitignored files... {}/{total}", i + 1);
        }
    }

    eprintln!("\rCopied {count} gitignored files into workspace    ");
    Ok(())
}

fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path)?;
        }
    }
    Ok(())
}

/// Run `mise env` in the given directory and parse the exported variables.
/// Returns a map of env var names to values that mise wants set for that directory.
fn mise_env(dir: &Path) -> HashMap<String, String> {
    let output = Command::new("mise")
        .arg("env")
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output();

    let output = match output {
        Ok(o) if o.status.success() => o,
        _ => return HashMap::new(),
    };

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_mise_env_output(&stdout)
}

fn parse_mise_env_output(output: &str) -> HashMap<String, String> {
    output
        .lines()
        .filter_map(|line| {
            let line = line.strip_prefix("export ")?;
            let (key, value) = line.split_once('=')?;
            let value = value.trim_matches('\'').trim_matches('"');
            Some((key.to_string(), value.to_string()))
        })
        .collect()
}

fn trust_mise_configs(ws_dir: &Path) -> Result<()> {
    let mise_available = Command::new("mise")
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());

    if !mise_available {
        return Ok(());
    }

    let configs = find_mise_configs(ws_dir);

    for config_path in &configs {
        let display = config_path.strip_prefix(ws_dir).unwrap_or(config_path);
        let status = Command::new("mise")
            .args(["trust", &path_str(config_path)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();

        match status {
            Ok(s) if s.success() => eprintln!("Trusted mise config: {}", display.display()),
            _ => eprintln!("Warning: could not trust mise config: {}", display.display()),
        }
    }

    if !configs.is_empty() {
        warn_mise_shims();
    }

    Ok(())
}

/// Warn if mise shims aren't on PATH. Without shims, non-interactive shells
/// (like those spawned by Claude Code) won't resolve the correct tool versions.
fn warn_mise_shims() {
    let shims_dir = match home_dir() {
        Ok(h) => h.join(".local/share/mise/shims"),
        Err(_) => return,
    };
    if !shims_dir.is_dir() {
        return;
    }

    let path = std::env::var("PATH").unwrap_or_default();
    let shims_str = path_str(&shims_dir);
    if !path.split(':').any(|p| p == shims_str) {
        eprintln!();
        eprintln!("Warning: mise shims directory is not on your PATH.");
        eprintln!("Non-interactive shells (e.g. Claude Code) may not pick up");
        eprintln!("the correct tool versions. Add this to your shell profile:");
        eprintln!();
        eprintln!("  export PATH=\"$HOME/.local/share/mise/shims:$PATH\"");
        eprintln!();
        eprintln!("workon will inject the correct env vars for this session,");
        eprintln!("but fixing your shell profile avoids the issue everywhere.");
        eprintln!();
    }
}

const MISE_CONFIG_NAMES: &[&str] = &[".mise.toml", ".mise.local.toml", "mise.toml", ".tool-versions"];

fn find_mise_configs(dir: &Path) -> Vec<PathBuf> {
    let mut configs = Vec::new();
    find_mise_configs_recursive(dir, &mut configs, 0);
    configs
}

fn find_mise_configs_recursive(dir: &Path, configs: &mut Vec<PathBuf>, depth: u32) {
    // Cap depth to avoid traversing into deep dependency trees
    if depth > 3 {
        return;
    }

    for name in MISE_CONFIG_NAMES {
        let path = dir.join(name);
        if path.exists() {
            configs.push(path);
        }
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // Skip hidden dirs, dependency dirs, and VCS dirs
        if name.starts_with('.')
            || name == "node_modules"
            || name == "vendor"
            || name == "target"
            || name == "build"
            || name == "dist"
        {
            continue;
        }
        find_mise_configs_recursive(&path, configs, depth + 1);
    }
}

fn generate_ws_id() -> String {
    let bytes: [u8; 3] = rand::rng().random();
    format!("ws-{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2])
}

fn detect_trunk(project_dir: &Path) -> String {
    let ok = Command::new("git")
        .args(["-C", &path_str(project_dir), "rev-parse", "--verify", "origin/master"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success());

    if ok { "master".into() } else { "main".into() }
}

fn detect_trunk_jj(project_dir: &Path) -> Result<String> {
    let output = Command::new("jj")
        .args([
            "-R",
            &path_str(project_dir),
            "log",
            "-r",
            "trunk()",
            "--no-graph",
            "-T",
            "bookmarks",
            "--limit",
            "1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .context("failed to detect trunk branch")?;

    let raw = String::from_utf8_lossy(&output.stdout);
    let branch = raw.split('@').next().unwrap_or("main").trim();
    if branch.is_empty() {
        Ok("main".into())
    } else {
        Ok(branch.into())
    }
}

fn run_cmd(program: &str, args: &[&str]) -> Result<()> {
    let status = Command::new(program)
        .args(args)
        .status()
        .with_context(|| format!("failed to run {program}"))?;
    if !status.success() {
        bail!("{program} exited with status {status}");
    }
    Ok(())
}

fn path_str(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

fn home_dir() -> Result<PathBuf> {
    crate::home::home_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ws_id_format() {
        let id = generate_ws_id();
        assert!(id.starts_with("ws-"));
        assert_eq!(id.len(), 9); // "ws-" + 6 hex chars
        assert!(id[3..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn ws_id_is_random() {
        let a = generate_ws_id();
        let b = generate_ws_id();
        assert_ne!(a, b);
    }

    #[test]
    fn copy_gitignored_files_copies_ignored_files() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&ws).unwrap();

        // Init a git repo with a .gitignore
        Command::new("git")
            .args(["init", &path_str(&project)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        std::fs::write(project.join(".gitignore"), "secret.key\nconfig/creds/\n").unwrap();
        std::fs::write(project.join("tracked.txt"), "hello").unwrap();

        // Create gitignored files
        std::fs::write(project.join("secret.key"), "supersecret").unwrap();
        std::fs::create_dir_all(project.join("config/creds")).unwrap();
        std::fs::write(project.join("config/creds/master.key"), "key123").unwrap();

        // Commit tracked files so git recognizes the repo
        Command::new("git")
            .args(["-C", &path_str(&project), "add", ".gitignore", "tracked.txt"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        Command::new("git")
            .args(["-C", &path_str(&project), "commit", "-m", "init"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();

        copy_gitignored_files(&project, &ws).unwrap();

        assert_eq!(std::fs::read_to_string(ws.join("secret.key")).unwrap(), "supersecret");
        assert_eq!(std::fs::read_to_string(ws.join("config/creds/master.key")).unwrap(), "key123");
    }

    #[test]
    fn copy_gitignored_files_skips_jj_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&ws).unwrap();

        Command::new("git")
            .args(["init", &path_str(&project)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        std::fs::write(project.join(".gitignore"), ".jj/\nsecret.key\n").unwrap();
        std::fs::write(project.join("secret.key"), "keep").unwrap();
        std::fs::create_dir_all(project.join(".jj/repo")).unwrap();
        std::fs::write(project.join(".jj/repo/store"), "corrupt").unwrap();

        Command::new("git")
            .args(["-C", &path_str(&project), "add", ".gitignore"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        Command::new("git")
            .args(["-C", &path_str(&project), "commit", "-m", "init"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();

        copy_gitignored_files(&project, &ws).unwrap();

        assert_eq!(std::fs::read_to_string(ws.join("secret.key")).unwrap(), "keep");
        assert!(!ws.join(".jj").exists(), ".jj/ should not be copied");
    }

    #[test]
    fn copy_gitignored_files_skips_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(&ws).unwrap();

        Command::new("git")
            .args(["init", &path_str(&project)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        std::fs::write(project.join(".gitignore"), "*.key\n").unwrap();
        std::fs::write(project.join("secret.key"), "from_project").unwrap();

        Command::new("git")
            .args(["-C", &path_str(&project), "add", ".gitignore"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        Command::new("git")
            .args(["-C", &path_str(&project), "commit", "-m", "init"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();

        // Pre-existing file in workspace should not be overwritten
        std::fs::write(ws.join("secret.key"), "already_here").unwrap();

        copy_gitignored_files(&project, &ws).unwrap();

        assert_eq!(std::fs::read_to_string(ws.join("secret.key")).unwrap(), "already_here");
    }

    #[test]
    fn copy_dir_recursive_copies_nested_files() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src_dir");
        let dst = tmp.path().join("dst_dir");

        std::fs::create_dir_all(src.join("sub/deep")).unwrap();
        std::fs::write(src.join("top.txt"), "top").unwrap();
        std::fs::write(src.join("sub/mid.txt"), "mid").unwrap();
        std::fs::write(src.join("sub/deep/bottom.txt"), "bottom").unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("top.txt")).unwrap(), "top");
        assert_eq!(std::fs::read_to_string(dst.join("sub/mid.txt")).unwrap(), "mid");
        assert_eq!(std::fs::read_to_string(dst.join("sub/deep/bottom.txt")).unwrap(), "bottom");
    }

    #[test]
    fn find_mise_configs_finds_root_and_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        std::fs::write(root.join(".mise.toml"), "").unwrap();
        std::fs::create_dir_all(root.join("services/api")).unwrap();
        std::fs::write(root.join("services/api/.mise.toml"), "").unwrap();
        std::fs::create_dir_all(root.join("services/web")).unwrap();
        std::fs::write(root.join("services/web/.tool-versions"), "").unwrap();

        let configs = find_mise_configs(root);
        let rel: Vec<_> = configs.iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().to_string())
            .collect();

        assert!(rel.contains(&".mise.toml".to_string()));
        assert!(rel.contains(&"services/api/.mise.toml".to_string()));
        assert!(rel.contains(&"services/web/.tool-versions".to_string()));
    }

    #[test]
    fn find_mise_configs_skips_hidden_and_dependency_dirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // These should be skipped
        std::fs::create_dir_all(root.join(".hidden")).unwrap();
        std::fs::write(root.join(".hidden/.mise.toml"), "").unwrap();
        std::fs::create_dir_all(root.join("node_modules/pkg")).unwrap();
        std::fs::write(root.join("node_modules/pkg/.tool-versions"), "").unwrap();
        std::fs::create_dir_all(root.join("vendor/lib")).unwrap();
        std::fs::write(root.join("vendor/lib/.mise.toml"), "").unwrap();

        let configs = find_mise_configs(root);
        assert!(configs.is_empty(), "should skip hidden/dependency dirs, got: {configs:?}");
    }

    #[test]
    fn find_mise_configs_respects_depth_limit() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();

        // depth 3 (within limit: root=0, a=1, b=2, c=3)
        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        std::fs::write(root.join("a/b/c/.mise.toml"), "").unwrap();
        // depth 4 (beyond limit)
        std::fs::create_dir_all(root.join("a/b/c/d")).unwrap();
        std::fs::write(root.join("a/b/c/d/.mise.toml"), "").unwrap();

        let configs = find_mise_configs(root);
        let rel: Vec<_> = configs.iter()
            .map(|p| p.strip_prefix(root).unwrap().to_string_lossy().to_string())
            .collect();

        assert!(rel.contains(&"a/b/c/.mise.toml".to_string()));
        assert!(!rel.contains(&"a/b/c/d/.mise.toml".to_string()), "should not scan beyond depth 3");
    }

    #[test]
    fn parse_mise_env_output_extracts_vars() {
        let output = "\
export PATH='/usr/local/bin:/usr/bin'
export RUBY_ROOT=/home/user/.mise/installs/ruby/4.0.1
export COMPOSER_HOME=\"/home/user/.composer\"
not an export line
";
        let vars = parse_mise_env_output(output);
        assert_eq!(vars.get("PATH").unwrap(), "/usr/local/bin:/usr/bin");
        assert_eq!(vars.get("RUBY_ROOT").unwrap(), "/home/user/.mise/installs/ruby/4.0.1");
        assert_eq!(vars.get("COMPOSER_HOME").unwrap(), "/home/user/.composer");
        assert!(!vars.contains_key("not"));
    }
}
