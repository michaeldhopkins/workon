use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::Result;
use rand::Rng;
use vcs_runner::{binary_available, run_git_utf8, Cmd};

use crate::claude_trust;
use crate::layout;
use crate::session;
use crate::vcs::Vcs;

/// The per-workspace test DB env file. Workon writes it on setup and excludes
/// it from the VCS — it must never be tracked or auto-saved as a real change.
const ENV_TEST_LOCAL: &str = ".env.test.local";

/// Files workon generates inside a workspace; ignored when deciding whether the
/// workspace has meaningful changes worth offering to save.
const GENERATED_FILES: &[&str] = &[ENV_TEST_LOCAL];

#[derive(Default)]
pub struct WorkspaceOptions<'a> {
    pub skip_copy_ignored: bool,
    pub label: Option<&'a str>,
    pub resume: Option<&'a str>,
    pub config: Option<&'a str>,
}

pub fn run_workspace(
    project_dir: &Path,
    project_name: &str,
    opts: WorkspaceOptions<'_>,
    vcs: &dyn Vcs,
) -> Result<()> {
    let WorkspaceOptions { skip_copy_ignored, label, resume, config } = opts;

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
    let base = vcs.create_workspace(project_dir, &ws_dir, &ws_id, &trunk)?;

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
        // setup_rails_db only writes .env.test.local when the DB was created.
        if created_db.is_some() {
            vcs.ignore_generated_file(project_dir, &ws_dir, ENV_TEST_LOCAL);
        }
    }

    let _ = claude_trust::approve_workspace(&ws_dir);

    let ws_layout;
    let claude_session_id;
    if let Some(prev_session_id) = resume {
        migrate_claude_session(prev_session_id, &ws_dir);
        ws_layout = layout::resolve_resume_layout(config, prev_session_id)?;
        claude_session_id = prev_session_id.to_string();
    } else {
        claude_session_id = generate_claude_session_id();
        ws_layout = layout::resolve_workspace_layout(config, &claude_session_id)?;
    }
    session::launch(&tab_name, ws_layout.path(), &ws_dir, &mise_vars)?;

    cleanup(&ws_id, &base, &claude_session_id, project_dir, &ws_dir, created_db.as_deref(), vcs)
}

/// Default-yes save prompt: empty input (bare Enter, or EOF from a closed
/// session) and any `y`/`yes` mean save; only an explicit `n`/`no`/other
/// declines.
fn is_affirmative(answer: &str) -> bool {
    let a = answer.trim();
    a.is_empty() || a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes")
}

fn cleanup(
    ws_id: &str,
    base: &str,
    claude_session_id: &str,
    project_dir: &Path,
    ws_dir: &Path,
    created_db: Option<&str>,
    vcs: &dyn Vcs,
) -> Result<()> {
    eprintln!();
    eprintln!("Cleaning up workspace {ws_id}...");
    eprintln!("Claude session: {claude_session_id}");
    eprintln!("  Resume with: workon -w --resume {claude_session_id}");

    // Two kinds of work would vanish into an anonymous head on teardown:
    // unsaved in-stack work (changed_files is bookmark-aware, so work already
    // bookmarked or pushed is excluded), and commits this workspace stranded
    // off its stack via `jj new` (attributed through the op log — concurrency-
    // proof, so a sibling workspace's orphans are never swept in here).
    let changed = vcs.changed_files(ws_id, base, project_dir, ws_dir);
    let meaningful: Vec<&String> = changed.iter().filter(|f| !GENERATED_FILES.contains(&f.as_str())).collect();
    let stranded = vcs.stranded_work(ws_id, base, project_dir, ws_dir);

    if !meaningful.is_empty() || !stranded.is_empty() {
        eprintln!("Workspace has unsaved work that won't survive teardown:");
        for f in &meaningful {
            eprintln!("    changed:  {f}");
        }
        for s in &stranded {
            eprintln!("    stranded: {s}");
        }
        // Default yes: the prompt only fires for work that would otherwise be
        // lost, so preserving is almost always what you want. Empty/EOF (you
        // closed the session without answering) counts as yes.
        eprint!("Save under workon/{ws_id}? [Y/n] ");
        std::io::stderr().flush()?;
        let mut answer = String::new();
        std::io::stdin().read_line(&mut answer)?;

        if is_affirmative(&answer) {
            if !meaningful.is_empty()
                && let Err(e) = vcs.save_work(ws_id, base, project_dir, ws_dir)
            {
                eprintln!("Warning: failed to save work: {e}");
            }
            for s in &stranded {
                if let Some(id) = s.split_whitespace().next()
                    && let Err(e) = vcs.save_stranded(project_dir, ws_id, id)
                {
                    eprintln!("Warning: failed to save stranded commit {id}: {e}");
                }
            }
        } else {
            eprintln!("Not saved. Recover later with: jj log");
        }
    }

    vcs.forget_workspace(ws_id, project_dir, ws_dir);

    if let Some(db) = created_db {
        let _ = Cmd::new("dropdb").arg(db).run();
        eprintln!("Dropped test database {db}");
    }

    // Spawn rm -rf in the background so the user gets their shell back
    // immediately. The OS will finish the deletion asynchronously.
    match Cmd::new("rm").args(["-rf", &path_str(ws_dir)]).spawn() {
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

    if Cmd::new("createdb").arg(&db_name).run().is_ok() {
        let env_content = format!("DATABASE_URL=postgresql://localhost/{db_name}");
        let _ = std::fs::write(ws_dir.join(ENV_TEST_LOCAL), env_content);

        eprintln!("Loading schema...");
        let mut cmd = Cmd::new("bundle")
            .args(["exec", "rails", "db:schema:load"])
            .env("RAILS_ENV", "test")
            .env("DATABASE_URL", format!("postgresql://localhost/{db_name}"))
            .in_dir(ws_dir);
        for (k, v) in mise_vars {
            cmd = cmd.env(k, v);
        }
        let _ = cmd.run();

        Some(db_name)
    } else {
        eprintln!("Warning: could not create test database {db_name}");
        None
    }
}

fn copy_gitignored_files(project_dir: &Path, ws_dir: &Path) -> Result<()> {
    let files = enumerate_gitignored_files(project_dir)?;
    if files.is_empty() {
        return Ok(());
    }
    let total = files.len();
    // On macOS/APFS, clonefile handles ~20k files/sec. On other platforms
    // the per-file reflink fallback is closer to ~3k files/sec.
    let rate = if cfg!(target_os = "macos") { 20_000 } else { 3_000 };
    let est_secs = total / rate;
    if est_secs >= 2 {
        eprintln!("Cloning {total} gitignored files (~{est_secs}s, skip with --skip-copy-ignored)...");
    } else {
        eprintln!("Cloning {total} gitignored files (skip with --skip-copy-ignored)...");
    }
    let cancelled = AtomicBool::new(false);
    let silent = AtomicBool::new(false);
    do_copy_files(project_dir, ws_dir, &files, &cancelled, &silent);
    Ok(())
}

fn enumerate_gitignored_files(project_dir: &Path) -> Result<Vec<String>> {
    let stdout = run_git_utf8(project_dir, &["ls-files", "--others", "--ignored", "--exclude-standard"])
        .map_err(|e| anyhow::anyhow!("failed to list gitignored files: {e}"))?;

    Ok(stdout.lines()
        .filter(|l| !l.is_empty() && !l.starts_with(".jj/"))
        .map(|l| l.strip_suffix('/').unwrap_or(l).to_string())
        .collect())
}

fn do_copy_files(
    project_dir: &Path,
    ws_dir: &Path,
    files: &[String],
    cancelled: &AtomicBool,
    silent: &AtomicBool,
) {
    let total = files.len();

    // Collect unique first-level path components (dirs and root files).
    let mut top_level: Vec<String> = files.iter()
        .map(|l| match l.find('/') {
            Some(i) => l[..i].to_string(),
            None => l.clone(),
        })
        .collect();
    top_level.sort_unstable();
    top_level.dedup();

    let opts = clonetree::Options::new();
    let mut cloned: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for name in &top_level {
        if cancelled.load(Ordering::Relaxed) { return; }

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
    let stragglers: Vec<&str> = files.iter()
        .filter(|l| {
            let top = match l.find('/') {
                Some(i) => &l[..i],
                None => l.as_str(),
            };
            !cloned.contains(top)
        })
        .map(String::as_str)
        .collect();

    let mut copied = 0usize;
    for rel_path in &stragglers {
        if cancelled.load(Ordering::Relaxed) { return; }

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
                    if !silent.load(Ordering::Relaxed) {
                        eprintln!("\rWarning: could not clone dir {rel_path}: {e}");
                    }
                }
            }
        } else if src.is_file() {
            if let Some(parent) = dst.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = std::fs::copy(&src, &dst) {
                if !silent.load(Ordering::Relaxed) {
                    eprintln!("\rWarning: could not copy {rel_path}: {e}");
                }
            } else {
                copied += 1;
            }
        }
    }

    if !silent.load(Ordering::Relaxed) {
        eprintln!(
            "Cloned {total} gitignored files ({} dirs cloned, {copied} copied individually)",
            cloned.len(),
        );
    }
}




fn mise_env(dir: &Path) -> HashMap<String, String> {
    match Cmd::new("mise").arg("env").in_dir(dir).run() {
        Ok(output) => parse_mise_env_output(&output.stdout_lossy()),
        Err(_) => HashMap::new(),
    }
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
    if !binary_available("mise") {
        return Ok(());
    }

    let configs = find_mise_configs(ws_dir);

    for config_path in &configs {
        let display = config_path.strip_prefix(ws_dir).unwrap_or(config_path);
        match Cmd::new("mise").args(["trust", &path_str(config_path)]).run() {
            Ok(_) => eprintln!("Trusted mise config: {}", display.display()),
            Err(_) => eprintln!("Warning: could not trust mise config: {}", display.display()),
        }
    }

    if !configs.is_empty() {
        warn_mise_shims();
    }

    Ok(())
}

/// Whether `mise activate` is live in the current environment. Activation exports
/// these markers; their presence means mise is already managing PATH (the install
/// bin dirs are injected directly), so the shims check below would be a false
/// alarm — `which ruby` resolves correctly without shims on PATH.
fn mise_activated() -> bool {
    ["MISE_SHELL", "__MISE_DIFF", "__MISE_SESSION"]
        .iter()
        .any(|k| std::env::var_os(k).is_some_and(|v| !v.is_empty()))
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
fn warn_mise_shims() {
    let shims_dir = match home_dir() {
        Ok(h) => h.join(".local/share/mise/shims"),
        Err(_) => return,
    };
    let shims_str = path_str(&shims_dir);
    let on_path = std::env::var("PATH").unwrap_or_default().split(':').any(|p| p == shims_str);
    let shims_would_help = shims_dir.is_dir() && !on_path;

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

/// Copy a Claude session file from wherever it was originally stored to the new
/// workspace's project directory so `claude --resume` can find it.
fn migrate_claude_session(session_id: &str, new_ws_dir: &Path) {
    let claude_dir = match home_dir() {
        Ok(h) => h.join(".claude").join("projects"),
        Err(_) => return,
    };
    let session_file = format!("{session_id}.jsonl");

    // Scan all project dirs for the session file
    let entries = match std::fs::read_dir(&claude_dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let src = entry.path().join(&session_file);
        if src.is_file() {
            let dest_dir_name = encode_claude_project_path(new_ws_dir);
            let dest_dir = claude_dir.join(dest_dir_name);
            let _ = std::fs::create_dir_all(&dest_dir);
            match std::fs::copy(&src, dest_dir.join(&session_file)) {
                Ok(_) => {
                    eprintln!("Migrated Claude session to new workspace");
                    return;
                }
                Err(e) => {
                    eprintln!("Warning: failed to copy Claude session: {e}");
                    return;
                }
            }
        }
    }
    eprintln!("Warning: could not find Claude session {session_id}");
}

/// Encode a path the way Claude Code does for its project directory names.
/// Non-alphanumeric characters are replaced with `-`.
/// `/Users/foo/.worktrees/bar` → `-Users-foo--worktrees-bar`
fn encode_claude_project_path(path: &Path) -> String {
    path_str(path)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

fn generate_ws_id() -> String {
    let bytes: [u8; 3] = rand::rng().random();
    format!("ws-{:02x}{:02x}{:02x}", bytes[0], bytes[1], bytes[2])
}

fn generate_claude_session_id() -> String {
    uuid::Uuid::new_v4().to_string()
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
    use std::process::{Command, Stdio};

    use super::*;

    #[test]
    fn is_affirmative_defaults_to_yes() {
        // Bare Enter and EOF (closed session) both arrive as empty -> save.
        assert!(is_affirmative(""));
        assert!(is_affirmative("\n"));
        assert!(is_affirmative("y"));
        assert!(is_affirmative("Y\n"));
        assert!(is_affirmative("yes"));
    }

    #[test]
    fn is_affirmative_explicit_no_declines() {
        assert!(!is_affirmative("n"));
        assert!(!is_affirmative("N\n"));
        assert!(!is_affirmative("no"));
        assert!(!is_affirmative("nope"));
    }

    #[test]
    fn ws_id_format() {
        let id = generate_ws_id();
        assert!(id.starts_with("ws-"));
        assert_eq!(id.len(), 9); // "ws-" + 6 hex chars
        assert!(id[3..].chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn encode_claude_project_path_matches_claude_convention() {
        let p = Path::new("/Users/foo/.worktrees/bar-ws-abc123");
        assert_eq!(encode_claude_project_path(p), "-Users-foo--worktrees-bar-ws-abc123");
    }

    #[test]
    fn claude_session_id_is_valid_uuid() {
        let id = generate_claude_session_id();
        assert!(uuid::Uuid::parse_str(&id).is_ok(), "should be a valid UUID: {id}");
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

    #[test]
    fn do_copy_files_stops_on_cancel() {
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
        std::fs::write(project.join(".gitignore"), "a.key\nb.key\nc.key\n").unwrap();
        std::fs::write(project.join("a.key"), "a").unwrap();
        std::fs::write(project.join("b.key"), "b").unwrap();
        std::fs::write(project.join("c.key"), "c").unwrap();

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

        // Cancel immediately before copying starts
        let cancelled = AtomicBool::new(true);
        let silent = AtomicBool::new(false);
        let files = enumerate_gitignored_files(&project).unwrap();
        do_copy_files(&project, &ws, &files, &cancelled, &silent);

        assert!(!ws.join("a.key").exists(), "no files should be copied when cancelled");
        assert!(!ws.join("b.key").exists());
        assert!(!ws.join("c.key").exists());
    }

    #[test]
    fn do_copy_files_respects_silent_flag() {
        // Verify do_copy_files completes successfully with silent=true
        // (no panics from suppressed output).
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
        std::fs::write(project.join(".gitignore"), "secret.key\n").unwrap();
        std::fs::write(project.join("secret.key"), "value").unwrap();

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

        let cancelled = AtomicBool::new(false);
        let silent = AtomicBool::new(true);
        let files = enumerate_gitignored_files(&project).unwrap();
        do_copy_files(&project, &ws, &files, &cancelled, &silent);

        assert_eq!(std::fs::read_to_string(ws.join("secret.key")).unwrap(), "value");
    }

    #[test]
    fn copy_gitignored_files_into_jj_workspace() {
        // Simulate copying into a workspace that was created by `jj workspace add`:
        // the workspace has tracked files and a .jj/ directory but no .git/.
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
        std::fs::write(project.join(".gitignore"), "target/\n.env\n").unwrap();
        std::fs::write(project.join("src.txt"), "code").unwrap();

        Command::new("git")
            .args(["-C", &path_str(&project), "add", "."])
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

        // Create gitignored files in the project
        std::fs::create_dir_all(project.join("target/debug")).unwrap();
        std::fs::write(project.join("target/debug/app"), "binary").unwrap();
        std::fs::write(project.join(".env"), "SECRET=123").unwrap();

        // Simulate a jj workspace: has tracked files and .jj/ but no .git/
        std::fs::write(ws.join("src.txt"), "code").unwrap();
        std::fs::create_dir_all(ws.join(".jj/working_copy")).unwrap();

        copy_gitignored_files(&project, &ws).unwrap();

        assert_eq!(
            std::fs::read_to_string(ws.join("target/debug/app")).unwrap(),
            "binary"
        );
        assert_eq!(
            std::fs::read_to_string(ws.join(".env")).unwrap(),
            "SECRET=123"
        );
    }
}
