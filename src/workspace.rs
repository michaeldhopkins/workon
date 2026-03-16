use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{bail, Context, Result};
use rand::Rng;

use crate::claude_trust;
use crate::session;
use crate::vcs::Vcs;

pub fn run_workspace(
    project_dir: &Path,
    project_name: &str,
    layout: &Path,
    skip_copy_ignored: bool,
    label: Option<&str>,
    vcs: &dyn Vcs,
) -> Result<()> {
    let ws_id = match label {
        Some(l) => format!("{}-{}", generate_ws_id(), slugify(l)),
        None => generate_ws_id(),
    };
    let ws_dir = home_dir()?
        .join(".worktrees")
        .join(format!("{project_name}-{ws_id}"));
    let tab_name = match label {
        Some(l) => capitalize(l),
        None => format!("{project_name}-{ws_id}"),
    };

    std::fs::create_dir_all(home_dir()?.join(".worktrees"))?;

    let trunk = vcs.detect_trunk(project_dir)?;
    vcs.create_workspace(project_dir, &ws_dir, &ws_id, &trunk)?;

    if !skip_copy_ignored {
        vcs.pre_copy_sync(project_dir);

        if let Err(e) = copy_gitignored_files(project_dir, &ws_dir) {
            eprintln!("Warning: failed to copy gitignored files: {e}");
        }
    }

    if let Err(e) = trust_mise_configs(&ws_dir) {
        eprintln!("Warning: failed to trust mise configs: {e}");
    }

    let mise_vars = mise_env(&ws_dir);

    let mut created_db = None;
    if ws_dir.join("config/database.yml").is_file() {
        created_db = setup_rails_db(project_name, &ws_id, &ws_dir, &mise_vars);
    }

    let _ = claude_trust::approve_workspace(&ws_dir);

    session::launch(&tab_name, layout, &ws_dir, &mise_vars)?;

    cleanup(&ws_id, project_dir, &ws_dir, created_db.as_deref(), vcs)
}

fn cleanup(
    ws_id: &str,
    project_dir: &Path,
    ws_dir: &Path,
    created_db: Option<&str>,
    vcs: &dyn Vcs,
) -> Result<()> {
    eprintln!();
    eprintln!("Cleaning up workspace {ws_id}...");

    const GENERATED_FILES: &[&str] = &[".env.test.local"];
    let changed = vcs.changed_files(ws_id, project_dir, ws_dir);
    let has_meaningful_changes = changed.iter().any(|f| !GENERATED_FILES.contains(&f.as_str()));

    if has_meaningful_changes {
        eprintln!("Workspace has uncommitted changes.");
        eprint!("Auto-save as workon/{ws_id}? [y/N] ");
        std::io::stderr().flush()?;

        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;
        if answer.trim().eq_ignore_ascii_case("y")
            && let Err(e) = vcs.save_work(ws_id, project_dir, ws_dir)
        {
            eprintln!("Warning: failed to save work: {e}");
        }
    }

    vcs.forget_workspace(ws_id, project_dir, ws_dir);

    if let Some(db) = created_db {
        let _ = Command::new("dropdb")
            .arg(db)
            .stderr(Stdio::null())
            .status();
        eprintln!("Dropped test database {db}");
    }

    // Spawn rm -rf in the background so the user gets their shell back
    // immediately. The OS will finish the deletion asynchronously.
    match Command::new("rm")
        .args(["-rf", &path_str(ws_dir)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    {
        Ok(_) => eprintln!("Removing workspace directory in background"),
        Err(_) => {
            let _ = std::fs::remove_dir_all(ws_dir);
            eprintln!("Removed workspace directory");
        }
    }

    Ok(())
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
        .map(|l| l.strip_suffix('/').unwrap_or(l))
        .collect();
    let total = lines.len();

    if total == 0 {
        return Ok(());
    }

    // On macOS/APFS, clonefile handles ~20k files/sec. On other platforms
    // the per-file reflink fallback is closer to ~3k files/sec.
    let rate = if cfg!(target_os = "macos") { 20_000 } else { 3_000 };
    let est_secs = total / rate;
    if est_secs >= 2 {
        eprintln!("Cloning {total} gitignored files (~{est_secs}s, skip with --skip-copy-ignored)...");
    } else {
        eprintln!("Cloning {total} gitignored files (skip with --skip-copy-ignored)...");
    }

    // Collect unique first-level path components (dirs and root files).
    let mut top_level: Vec<String> = lines.iter()
        .map(|l| match l.find('/') {
            Some(i) => l[..i].to_string(),
            None => l.to_string(),
        })
        .collect();
    top_level.sort_unstable();
    top_level.dedup();

    // Clone each top-level entry. On macOS/APFS this uses clonefile(2) to
    // clone entire directory trees in a single syscall. On other platforms
    // it walks the tree and reflinks each file individually.
    let opts = clonetree::Options::new();
    let mut cloned: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for name in &top_level {
        let src = project_dir.join(name);
        let dst = ws_dir.join(name);

        if dst.exists() {
            continue;
        }

        if src.is_dir() {
            if clonetree::clone_tree(&src, &dst, &opts).is_ok() {
                cloned.insert(name);
            }
        } else if src.is_file()
            && std::fs::copy(&src, &dst).is_ok()
        {
            cloned.insert(name);
        }
    }

    // Copy any stragglers whose top-level clone failed.
    let stragglers: Vec<&str> = lines.iter()
        .filter(|l| {
            let top = match l.find('/') {
                Some(i) => &l[..i],
                None => *l,
            };
            !cloned.contains(top)
        })
        .copied()
        .collect();

    let mut copied = 0usize;
    if !stragglers.is_empty() {
        for rel_path in &stragglers {
            let dst = ws_dir.join(rel_path);
            if dst.exists() { continue; }
            let src = project_dir.join(rel_path);
            if src.is_dir() {
                // Nested git repos (e.g. bundler git gem checkouts) appear as
                // directory entries in `git ls-files --others`. Clone them whole.
                if let Some(parent) = dst.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                match clonetree::clone_tree(&src, &dst, &opts) {
                    Ok(()) => copied += 1,
                    Err(e) => {
                        eprintln!("\rWarning: could not clone dir {rel_path}: {e}");
                    }
                }
            } else if src.is_file() {
                if let Some(parent) = dst.parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                if let Err(e) = std::fs::copy(&src, &dst) {
                    eprintln!("\rWarning: could not copy {rel_path}: {e}");
                } else {
                    copied += 1;
                }
            }
        }
    }

    eprintln!(
        "Cloned {total} gitignored files ({} dirs cloned, {copied} copied individually)",
        cloned.len(),
    );

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

fn slugify(text: &str) -> String {
    text.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn capitalize(text: &str) -> String {
    let mut chars = text.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => c.to_uppercase().to_string() + chars.as_str(),
    }
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
    fn slugify_converts_text_to_slug() {
        assert_eq!(slugify("HD Ticket #12345"), "hd-ticket-12345");
        assert_eq!(slugify("Fix the BUG"), "fix-the-bug");
        assert_eq!(slugify("  leading/trailing  "), "leading-trailing");
        assert_eq!(slugify("a--b"), "a-b");
        assert_eq!(slugify("simple"), "simple");
    }

    #[test]
    fn capitalize_uppercases_first_char() {
        assert_eq!(capitalize("fix tests"), "Fix tests");
        assert_eq!(capitalize("HD Ticket"), "HD Ticket");
        assert_eq!(capitalize(""), "");
        assert_eq!(capitalize("a"), "A");
    }

    #[test]
    fn copy_gitignored_files_copies_ignored_files() {
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
        std::fs::write(project.join(".gitignore"), "secret.key\nconfig/creds/\n").unwrap();
        std::fs::write(project.join("tracked.txt"), "hello").unwrap();

        std::fs::write(project.join("secret.key"), "supersecret").unwrap();
        std::fs::create_dir_all(project.join("config/creds")).unwrap();
        std::fs::write(project.join("config/creds/master.key"), "key123").unwrap();

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

        std::fs::write(ws.join("secret.key"), "already_here").unwrap();

        copy_gitignored_files(&project, &ws).unwrap();

        assert_eq!(std::fs::read_to_string(ws.join("secret.key")).unwrap(), "already_here");
    }

    #[test]
    fn copy_gitignored_files_clones_directories() {
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
        std::fs::write(project.join(".gitignore"), "build/\nsecret.key\n").unwrap();
        std::fs::write(project.join("tracked.txt"), "hello").unwrap();
        std::fs::write(project.join("secret.key"), "root_file").unwrap();
        std::fs::create_dir_all(project.join("build/sub")).unwrap();
        std::fs::write(project.join("build/out.o"), "compiled").unwrap();
        std::fs::write(project.join("build/sub/lib.a"), "archive").unwrap();

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

        assert_eq!(std::fs::read_to_string(ws.join("secret.key")).unwrap(), "root_file");
        assert_eq!(std::fs::read_to_string(ws.join("build/out.o")).unwrap(), "compiled");
        assert_eq!(std::fs::read_to_string(ws.join("build/sub/lib.a")).unwrap(), "archive");
    }

    #[test]
    fn copy_gitignored_files_falls_back_for_partially_existing_dir() {
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
        std::fs::write(project.join(".gitignore"), "config/creds/\n").unwrap();
        std::fs::create_dir_all(project.join("config/creds")).unwrap();
        std::fs::write(project.join("config/settings.toml"), "tracked").unwrap();
        std::fs::write(project.join("config/creds/secret.key"), "hidden").unwrap();

        Command::new("git")
            .args(["-C", &path_str(&project), "add", ".gitignore", "config/settings.toml"])
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

        std::fs::create_dir_all(ws.join("config")).unwrap();
        std::fs::write(ws.join("config/settings.toml"), "tracked").unwrap();

        copy_gitignored_files(&project, &ws).unwrap();

        assert_eq!(
            std::fs::read_to_string(ws.join("config/creds/secret.key")).unwrap(),
            "hidden"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("config/settings.toml")).unwrap(),
            "tracked"
        );
    }

    #[test]
    fn copy_gitignored_files_clones_nested_git_repos_as_stragglers() {
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
        std::fs::write(project.join(".gitignore"), "vendor/bundle/\n").unwrap();
        std::fs::create_dir_all(project.join("vendor")).unwrap();
        std::fs::write(project.join("vendor/tracked.txt"), "tracked").unwrap();

        let gem_dir = project.join("vendor/bundle/gems/some_gem-abc123");
        std::fs::create_dir_all(&gem_dir).unwrap();
        std::fs::write(gem_dir.join("lib.rb"), "puts 'hello'").unwrap();
        Command::new("git")
            .args(["init", &path_str(&gem_dir)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        Command::new("git")
            .args(["-C", &path_str(&gem_dir), "add", "."])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        Command::new("git")
            .args(["-C", &path_str(&gem_dir), "commit", "-m", "init"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();

        Command::new("git")
            .args(["-C", &path_str(&project), "add", ".gitignore", "vendor/tracked.txt"])
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

        std::fs::create_dir_all(ws.join("vendor")).unwrap();
        std::fs::write(ws.join("vendor/tracked.txt"), "tracked").unwrap();

        copy_gitignored_files(&project, &ws).unwrap();

        assert!(
            ws.join("vendor/bundle/gems/some_gem-abc123/lib.rb").exists(),
            "nested git repo content should be copied to workspace"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join("vendor/bundle/gems/some_gem-abc123/lib.rb")).unwrap(),
            "puts 'hello'"
        );
        assert!(
            ws.join("vendor/bundle/gems/some_gem-abc123/.git").exists(),
            "nested .git directory should be cloned too"
        );
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

        std::fs::create_dir_all(root.join("a/b/c")).unwrap();
        std::fs::write(root.join("a/b/c/.mise.toml"), "").unwrap();
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
