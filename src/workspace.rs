use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{bail, Context, Result};
use clap_complete::engine::CompletionCandidate;
use rand::Rng;
use serde::{Deserialize, Serialize};
use vcs_runner::{binary_available, run_git_utf8, Cmd};

use crate::claude_trust;
use crate::deps;
use crate::discover::{self, WsRef};
use crate::layout;
use crate::session;
use crate::vcs::Vcs;

/// The per-workspace test DB env file. Workon writes it on setup and excludes
/// it from the VCS — it must never be tracked or auto-saved as a real change.
const ENV_TEST_LOCAL: &str = ".env.test.local";

/// Per-workspace provenance file, written by `provision` inside the worktree.
/// Holds only what provision chose or did and can't be inferred later. Removed
/// with the worktree on teardown. See specs/non-interactive-workspaces.md.
const WORKON_JSON: &str = ".workon.json";

/// Files workon generates inside a workspace; ignored when deciding whether the
/// workspace has meaningful changes worth offering to save.
const GENERATED_FILES: &[&str] = &[ENV_TEST_LOCAL, WORKON_JSON];

#[derive(Default)]
pub struct WorkspaceOptions<'a> {
    pub skip_copy_ignored: bool,
    pub label: Option<&'a str>,
    pub resume: Option<&'a str>,
    pub config: Option<&'a str>,
}

/// A live handle to a provisioned workspace, threaded through the
/// provision → attach → teardown phases. Constructed by `provision`; in a
/// fresh process (headless `destroy`) it will be rebuilt by `load_workspace`
/// with the provenance fields sourced from `.workon.json`.
struct Workspace {
    ws_id: String,
    /// Original (un-slugified) label, if the workspace was named.
    name: Option<String>,
    project_dir: PathBuf,
    project_name: String,
    ws_dir: PathBuf,
    /// The commit `create_workspace` pinned as the branch point. Teardown diffs
    /// against this, never a re-resolved trunk (which may have moved). `None`
    /// only for a workspace reloaded from a `.workon.json` that predates the
    /// field or a partial `create` — teardown then skips unsaved-work detection.
    base: Option<String>,
    /// Layout the workspace was created with; `attach` falls back to it when no
    /// `-c` is passed.
    config: Option<String>,
    /// The test DB provision created, if any — so teardown drops only our own.
    created_db: Option<String>,
}

/// On-disk contents of `.workon.json`. Every field is written explicitly (as
/// `null` when absent) so the file is self-describing; missing fields on read
/// (an older or partial file) decode to `None`.
#[derive(Serialize, Deserialize, Default)]
struct WorkspaceMeta {
    #[serde(default)]
    base: Option<String>,
    #[serde(default)]
    config: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    created_db: Option<String>,
}

/// Atomically write `.workon.json` into the worktree (temp file + rename, the
/// same recipe `claude_trust` uses) so a crash can't leave a torn file.
fn write_meta(ws_dir: &Path, meta: &WorkspaceMeta) -> Result<()> {
    let path = ws_dir.join(WORKON_JSON);
    let json = serde_json::to_string_pretty(meta)?;
    let tmp = tempfile::NamedTempFile::new_in(ws_dir)?;
    std::fs::write(tmp.path(), json.as_bytes())?;
    tmp.persist(&path)?;
    Ok(())
}

/// Read `.workon.json` back. A missing file (older workspace, or none written)
/// or a malformed one decodes to all-`None` — teardown then skips save
/// detection rather than acting on bad provenance.
fn read_meta(ws_dir: &Path) -> WorkspaceMeta {
    let path = ws_dir.join(WORKON_JSON);
    let Ok(content) = std::fs::read_to_string(&path) else {
        return WorkspaceMeta::default();
    };
    serde_json::from_str(&content).unwrap_or_else(|e| {
        eprintln!("workon: ignoring malformed {}: {e}", path.display());
        WorkspaceMeta::default()
    })
}

/// Ephemeral workspace flow (`workon -w`): provision, attach, and tear down on
/// quit. Same observable behavior as before, now composed from the three phases
/// the headless subcommands also use.
pub fn run_workspace(
    project_dir: &Path,
    project_name: &str,
    opts: WorkspaceOptions<'_>,
    vcs: &dyn Vcs,
) -> Result<()> {
    let WorkspaceOptions { skip_copy_ignored, label, resume, config } = opts;
    let ws = provision(project_dir, project_name, skip_copy_ignored, label, config, vcs)?;
    let claude_session_id = attach(&ws, config, resume)?;
    teardown(&ws, Some(&claude_session_id), SaveMode::Prompt, vcs)?;
    Ok(())
}

/// How teardown decides whether to rescue unsaved work. `Prompt` is the
/// interactive `[Y/n]` (ephemeral quit); `Save`/`NoSave` are the non-interactive
/// `destroy` choices, since a headless run can't block on stdin.
enum SaveMode {
    Prompt,
    Save,
    NoSave,
}

/// What teardown did, for `destroy --json` and tests.
struct TeardownOutcome {
    /// Bookmark/branch names created to rescue work (`workon/<ws_id>[-<id>]`).
    saved: Vec<String>,
    /// The test DB that was dropped, if any.
    dropped_db: Option<String>,
}

/// Everything up to (but not including) the interactive session: create the
/// worktree, copy gitignored files, set up mise + the Rails test DB, approve
/// the workspace for Claude, and record provenance in `.workon.json`. The
/// returned `Workspace` is the handle the later phases operate on.
fn provision(
    project_dir: &Path,
    project_name: &str,
    skip_copy_ignored: bool,
    label: Option<&str>,
    config: Option<&str>,
    vcs: &dyn Vcs,
) -> Result<Workspace> {
    let ws = provision_in(
        &discover::worktrees_dir()?,
        project_dir,
        project_name,
        skip_copy_ignored,
        label,
        config,
        vcs,
    )?;
    // Trust the worktree for Claude Code. Kept out of `provision_in` (the
    // testable core) because it writes the real ~/.claude.json; covered on its
    // own by claude_trust's tests.
    let _ = claude_trust::approve_workspace(&ws.ws_dir);
    Ok(ws)
}

/// `provision` against an explicit worktrees root. Split out so tests can point
/// it at a tempdir instead of the real `~/.worktrees`.
fn provision_in(
    worktrees: &Path,
    project_dir: &Path,
    project_name: &str,
    skip_copy_ignored: bool,
    label: Option<&str>,
    config: Option<&str>,
    vcs: &dyn Vcs,
) -> Result<Workspace> {
    let ws_id = match label {
        Some(l) => format!("{}-{}", generate_ws_id(), slugify(l)),
        None => generate_ws_id(),
    };
    std::fs::create_dir_all(worktrees)?;
    let ws_dir = worktrees.join(format!("{project_name}-{ws_id}"));

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

    let meta = WorkspaceMeta {
        base: Some(base.clone()),
        config: config.map(String::from),
        name: label.map(String::from),
        created_db: created_db.clone(),
    };
    match write_meta(&ws_dir, &meta) {
        Ok(()) => vcs.ignore_generated_file(project_dir, &ws_dir, WORKON_JSON),
        Err(e) => eprintln!("Warning: failed to write {WORKON_JSON}: {e}"),
    }

    Ok(Workspace {
        ws_id,
        name: label.map(String::from),
        project_dir: project_dir.to_path_buf(),
        project_name: project_name.to_string(),
        ws_dir,
        base: Some(base),
        config: config.map(String::from),
        created_db,
    })
}

/// Resolve the layout (with the claude session id or resume args injected) and
/// hand off to zellij. Blocks until the session quits. Returns the claude
/// session id so the caller can surface the resume hint. Recomputes mise env
/// from the worktree so it works the same in a fresh process.
fn attach(ws: &Workspace, config: Option<&str>, resume: Option<&str>) -> Result<String> {
    let mise_vars = mise_env(&ws.ws_dir);
    let tab_name = match ws.name.as_deref() {
        Some(l) => capitalize(l),
        None => format!("{}-{}", ws.project_name, ws.ws_id),
    };

    let ws_layout;
    let claude_session_id;
    if let Some(prev_session_id) = resume {
        migrate_claude_session(prev_session_id, &ws.ws_dir);
        ws_layout = layout::resolve_resume_layout(config, prev_session_id)?;
        claude_session_id = prev_session_id.to_string();
    } else {
        claude_session_id = generate_claude_session_id();
        ws_layout = layout::resolve_workspace_layout(config, &claude_session_id)?;
    }
    session::launch(&tab_name, ws_layout.path(), &ws.ws_dir, &mise_vars)?;

    Ok(claude_session_id)
}

/// Default-yes save prompt: empty input (bare Enter, or EOF from a closed
/// session) and any `y`/`yes` mean save; only an explicit `n`/`no`/other
/// declines.
fn is_affirmative(answer: &str) -> bool {
    let a = answer.trim();
    a.is_empty() || a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes")
}

/// Decide whether teardown saves the unsaved work it found. `Save`/`NoSave` are
/// non-interactive; `Prompt` asks on stderr and reads stdin (default yes).
fn should_save(save: &SaveMode, ws_id: &str) -> Result<bool> {
    match save {
        SaveMode::Save => Ok(true),
        SaveMode::NoSave => Ok(false),
        SaveMode::Prompt => {
            // Default yes: the prompt only fires for work that would otherwise
            // be lost, so preserving is almost always what you want. Empty/EOF
            // (you closed the session without answering) counts as yes.
            eprint!("Save under workon/{ws_id}? [Y/n] ");
            std::io::stderr().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            Ok(is_affirmative(&answer))
        }
    }
}

/// Final phase: rescue unsaved work (per `save`), then forget the workspace,
/// drop its test DB, and remove the directory. `claude_session_id` is `Some`
/// only for the ephemeral flow, where it drives the resume hint.
fn teardown(
    ws: &Workspace,
    claude_session_id: Option<&str>,
    save: SaveMode,
    vcs: &dyn Vcs,
) -> Result<TeardownOutcome> {
    eprintln!();
    eprintln!("Cleaning up workspace {}...", ws.ws_id);
    if let Some(sid) = claude_session_id {
        eprintln!("Claude session: {sid}");
        eprintln!("  Resume with: workon -w --resume {sid}");
    }

    // Two kinds of work would vanish into an anonymous head on teardown:
    // unsaved in-stack work (changed_files is bookmark-aware, so work already
    // bookmarked or pushed is excluded), and commits this workspace stranded
    // off its stack via `jj new` (attributed through the op log — concurrency-
    // proof, so a sibling workspace's orphans are never swept in here). Both
    // diff against the pinned base; with no recorded base we can't tell unsaved
    // work from upstream, so we skip detection rather than risk a wrong answer.
    let (meaningful, stranded) = match ws.base.as_deref() {
        Some(base) => {
            let changed = vcs.changed_files(&ws.ws_id, base, &ws.project_dir, &ws.ws_dir);
            let meaningful: Vec<String> =
                changed.into_iter().filter(|f| !GENERATED_FILES.contains(&f.as_str())).collect();
            let stranded = vcs.stranded_work(&ws.ws_id, base, &ws.project_dir, &ws.ws_dir);
            (meaningful, stranded)
        }
        None => {
            eprintln!("Warning: no recorded base commit; skipping unsaved-work detection.");
            (Vec::new(), Vec::new())
        }
    };

    let mut saved: Vec<String> = Vec::new();
    if !meaningful.is_empty() || !stranded.is_empty() {
        eprintln!("Workspace has unsaved work that won't survive teardown:");
        for f in &meaningful {
            eprintln!("    changed:  {f}");
        }
        for s in &stranded {
            eprintln!("    stranded: {s}");
        }

        if should_save(&save, &ws.ws_id)? {
            if !meaningful.is_empty()
                && let Some(base) = ws.base.as_deref()
            {
                match vcs.save_work(&ws.ws_id, base, &ws.project_dir, &ws.ws_dir) {
                    Ok(()) => saved.push(format!("workon/{}", ws.ws_id)),
                    Err(e) => eprintln!("Warning: failed to save work: {e}"),
                }
            }
            for s in &stranded {
                if let Some(id) = s.split_whitespace().next() {
                    match vcs.save_stranded(&ws.project_dir, &ws.ws_id, id) {
                        Ok(()) => saved.push(format!("workon/{}-{}", ws.ws_id, id)),
                        Err(e) => eprintln!("Warning: failed to save stranded commit {id}: {e}"),
                    }
                }
            }
        } else {
            eprintln!("Not saved. Recover later with: jj log");
        }
    }

    vcs.forget_workspace(&ws.ws_id, &ws.project_dir, &ws.ws_dir);

    let mut dropped_db = None;
    if let Some(db) = ws.created_db.as_deref() {
        let _ = Cmd::new("dropdb").arg(db).run();
        eprintln!("Dropped test database {db}");
        dropped_db = Some(db.to_string());
    }

    // Spawn rm -rf in the background so the user gets their shell back
    // immediately. The OS will finish the deletion asynchronously.
    match Cmd::new("rm").args(["-rf", &path_str(&ws.ws_dir)]).spawn() {
        Ok(_) => eprintln!("Removing workspace directory in background"),
        Err(_) => {
            let _ = std::fs::remove_dir_all(&ws.ws_dir);
            eprintln!("Removed workspace directory");
        }
    }

    Ok(TeardownOutcome { saved, dropped_db })
}

/// Rebuild a `Workspace` handle in a fresh process by inferring structure from
/// the worktree and reading provenance from `.workon.json`.
fn load_workspace(reference: Option<&str>) -> Result<Workspace> {
    let ws_dir = resolve_ws_dir(reference)?;
    let project_dir = discover::project_dir_of(&ws_dir)?;
    let project_name = project_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .context("project directory has no name")?;
    let ws_id = discover::ws_id_of(&ws_dir, &project_name).with_context(|| {
        format!(
            "{} is not named <project>-<ws_id> for project {project_name}",
            ws_dir.display()
        )
    })?;
    let meta = read_meta(&ws_dir);

    Ok(Workspace {
        ws_id,
        name: meta.name,
        project_dir,
        project_name,
        ws_dir,
        base: meta.base,
        config: meta.config,
        created_db: meta.created_db,
    })
}

/// Turn a CLI reference into a concrete worktree path: cwd's enclosing
/// workspace, an explicit path, or a ws_id/nickname looked up under
/// `~/.worktrees`.
fn resolve_ws_dir(reference: Option<&str>) -> Result<PathBuf> {
    match discover::classify_ref(reference) {
        WsRef::Cwd => find_enclosing_workspace(),
        WsRef::Path(p) => Ok(p),
        WsRef::Token(t) => find_by_token(&t),
    }
}

/// Walk cwd's ancestors for the one that sits directly under `~/.worktrees` —
/// that's the workspace you're standing in.
fn find_enclosing_workspace() -> Result<PathBuf> {
    let cwd = std::env::current_dir()?;
    let root = std::fs::canonicalize(discover::worktrees_dir()?).ok();
    for ancestor in cwd.ancestors() {
        let parent = std::fs::canonicalize(ancestor).ok().and_then(|p| p.parent().map(Path::to_path_buf));
        if parent.is_some() && parent == root {
            return Ok(ancestor.to_path_buf());
        }
    }
    bail!("not inside a workspace — pass a ws_id or name (see: workon list)");
}

/// Whether `token` identifies a workspace with this `ws_id`/`name`. Matches the
/// ws_id exactly, or the `--name` nickname up to slugification — so the token
/// can be given either as stored (`"fix bug"`) or as it appears in the ws_id
/// (`"fix-bug"`).
fn token_matches(token: &str, ws_id: &str, name: Option<&str>) -> bool {
    ws_id == token || name.is_some_and(|n| slugify(n) == slugify(token))
}

/// Find the single worktree whose ws_id or `--name` nickname matches `token`.
fn find_by_token(token: &str) -> Result<PathBuf> {
    let mut hits: Vec<PathBuf> = Vec::new();
    for ws_dir in discover::list_worktree_dirs()? {
        let ws_id = discover::project_dir_of(&ws_dir)
            .ok()
            .and_then(|pd| pd.file_name().map(|n| n.to_string_lossy().into_owned()))
            .and_then(|pn| discover::ws_id_of(&ws_dir, &pn));
        let name = read_meta(&ws_dir).name;
        if token_matches(token, ws_id.as_deref().unwrap_or_default(), name.as_deref()) {
            hits.push(ws_dir);
        }
    }
    match hits.len() {
        0 => bail!("no workspace matches '{token}' (see: workon list)"),
        1 => Ok(hits.remove(0)),
        _ => {
            let names: Vec<String> = hits.iter().map(|p| path_str(p)).collect();
            bail!("'{token}' is ambiguous; matches:\n  {}", names.join("\n  "))
        }
    }
}

pub struct CreateArgs<'a> {
    pub skip_copy_ignored: bool,
    pub name: Option<&'a str>,
    pub config: Option<&'a str>,
    pub json: bool,
}

/// `workon create`: provision a persistent workspace and print its path to
/// stdout (so `WS=$(workon create)` works). No session, no teardown.
pub fn cmd_create(
    project_dir: &Path,
    project_name: &str,
    args: CreateArgs<'_>,
    vcs: &dyn Vcs,
) -> Result<()> {
    let ws = provision(project_dir, project_name, args.skip_copy_ignored, args.name, args.config, vcs)?;
    if args.json {
        let obj = serde_json::json!({
            "ws_id": ws.ws_id,
            "path": path_str(&ws.ws_dir),
            "db": ws.created_db,
        });
        println!("{obj}");
    } else {
        println!("{}", path_str(&ws.ws_dir));
        eprintln!("Created workspace {}", ws.ws_id);
        eprintln!("  Attach:  workon attach {}", ws.ws_id);
        eprintln!("  Destroy: workon destroy {}", ws.ws_id);
    }
    Ok(())
}

/// `workon attach [REF]`: open an existing workspace in a session and return
/// when it quits. No teardown — the workspace persists.
pub fn cmd_attach(reference: Option<&str>, config_override: Option<&str>) -> Result<()> {
    let ws = load_workspace(reference)?;
    let config = config_override.or(ws.config.as_deref());

    // Same up-front validation as the interactive path: a config that doesn't
    // resolve, is untrusted, or wants a missing binary should fail before we
    // hand control to zellij.
    let layout_content = layout::read_config(config)?;
    layout::validate_layout(&layout_content)?;
    deps::check_all(&layout_content)?;

    attach(&ws, config, None)?;
    Ok(())
}

/// `workon path [REF]`: print a workspace's directory to stdout so a shell can
/// enter it (`cd "$(workon path REF)"`). No session, no changes — a child
/// process can't change the parent shell's cwd, so this is the print half of
/// that idiom.
/// Shell-completion candidates for a workspace reference: every worktree's
/// ws_id and, where set, its `--name` nickname (slugified), each helped by its
/// project name. Feeds the `add = ArgValueCandidates::new(...)` on `reference`.
pub fn ref_candidates() -> Vec<CompletionCandidate> {
    match discover::worktrees_dir() {
        Ok(root) => ref_candidates_from(&root),
        Err(_) => Vec::new(),
    }
}

fn ref_candidates_from(worktrees: &Path) -> Vec<CompletionCandidate> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for ws_dir in discover::list_dirs_in(worktrees) {
        let project_name = discover::project_dir_of(&ws_dir)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()));
        let help = || project_name.clone().map(Into::into);

        if let Some(pn) = project_name.as_deref()
            && let Some(id) = discover::ws_id_of(&ws_dir, pn)
            && seen.insert(id.clone())
        {
            out.push(CompletionCandidate::new(id).help(help()));
        }
        if let Some(name) = read_meta(&ws_dir).name {
            let slug = slugify(&name);
            if !slug.is_empty() && seen.insert(slug.clone()) {
                out.push(CompletionCandidate::new(slug).help(help()));
            }
        }
    }
    out
}

pub fn cmd_path(reference: Option<&str>) -> Result<()> {
    // Just resolve the directory — no project inference or naming check. That
    // keeps `path` working for a stale workspace (project dir gone), which is
    // exactly when you'd want to cd in and look around.
    let ws_dir = resolve_ws_dir(reference)?;
    println!("{}", path_str(&ws_dir));
    Ok(())
}

/// `workon destroy [REF]`: tear down a workspace. Saves rescued work by default;
/// `--no-save` discards it.
pub fn cmd_destroy(reference: Option<&str>, no_save: bool, json: bool) -> Result<()> {
    let ws = load_workspace(reference)?;
    discover::assert_under_worktrees(&ws.ws_dir)?;
    let vcs = crate::vcs::detect(&ws.project_dir)?;
    let save = if no_save { SaveMode::NoSave } else { SaveMode::Save };
    let outcome = teardown(&ws, None, save, &*vcs)?;
    if json {
        let obj = serde_json::json!({
            "ws_id": ws.ws_id,
            "saved": outcome.saved,
            "dropped_db": outcome.dropped_db,
        });
        println!("{obj}");
    }
    Ok(())
}

struct WorkspaceRow {
    ws_id: String,
    name: Option<String>,
    age_seconds: u64,
    project: Option<String>,
    ws_dir: PathBuf,
    status: &'static str,
}

/// `workon list`: workspaces whose project is at or under cwd, plus any stale
/// (unresolvable) worktrees, which are surfaced from anywhere so leaks are
/// visible.
pub fn cmd_list(json: bool) -> Result<()> {
    let cwd = std::env::current_dir()?;
    let cwd_c = std::fs::canonicalize(&cwd).unwrap_or(cwd);

    let rows: Vec<WorkspaceRow> = discover::list_worktree_dirs()?
        .iter()
        .filter_map(|ws_dir| describe_workspace(ws_dir, &cwd_c))
        .collect();

    if json {
        let arr: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "ws_id": r.ws_id,
                    "name": r.name,
                    "age_seconds": r.age_seconds,
                    "project": r.project,
                    "ws_dir": path_str(&r.ws_dir),
                    "status": r.status,
                })
            })
            .collect();
        println!("{}", serde_json::Value::Array(arr));
        return Ok(());
    }

    if rows.is_empty() {
        eprintln!("No workspaces.");
        return Ok(());
    }
    println!("{}", list_row("WORKSPACE", "NAME", "AGE", "PROJECT", "STATUS"));
    for r in &rows {
        println!(
            "{}",
            list_row(
                &r.ws_id,
                r.name.as_deref().unwrap_or("-"),
                &humanize_age(r.age_seconds),
                r.project.as_deref().unwrap_or("-"),
                r.status,
            )
        );
    }
    Ok(())
}

fn list_row(ws_id: &str, name: &str, age: &str, project: &str, status: &str) -> String {
    format!("{ws_id:<26}  {name:<16}  {age:>5}  {project:<20}  {status}")
}

/// Build a `list` row for one worktree. Returns `None` for a resolvable
/// workspace whose project isn't under cwd (not ours to show here); stale
/// worktrees are always returned.
fn describe_workspace(ws_dir: &Path, cwd_c: &Path) -> Option<WorkspaceRow> {
    let age_seconds = workspace_age_seconds(ws_dir);
    let name = read_meta(ws_dir).name;
    let dir_basename = || ws_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();

    match discover::project_dir_of(ws_dir) {
        Ok(project_dir) => {
            let project_dir_c = std::fs::canonicalize(&project_dir).unwrap_or_else(|_| project_dir.clone());
            if !project_dir_c.starts_with(cwd_c) {
                return None;
            }
            let project_name = project_dir.file_name().map(|n| n.to_string_lossy().into_owned());
            let ws_id = project_name
                .as_deref()
                .and_then(|pn| discover::ws_id_of(ws_dir, pn))
                .unwrap_or_else(dir_basename);
            Some(WorkspaceRow {
                ws_id,
                name,
                age_seconds,
                project: project_name,
                ws_dir: ws_dir.to_path_buf(),
                status: "active",
            })
        }
        Err(_) => Some(WorkspaceRow {
            ws_id: dir_basename(),
            name,
            age_seconds,
            project: None,
            ws_dir: ws_dir.to_path_buf(),
            status: "stale",
        }),
    }
}

fn workspace_age_seconds(ws_dir: &Path) -> u64 {
    std::fs::metadata(ws_dir)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .map_or(0, |d| d.as_secs())
}

fn humanize_age(secs: u64) -> String {
    const MIN: u64 = 60;
    const HOUR: u64 = 60 * MIN;
    const DAY: u64 = 24 * HOUR;
    if secs < MIN {
        format!("{secs}s")
    } else if secs < HOUR {
        format!("{}m", secs / MIN)
    } else if secs < DAY {
        format!("{}h", secs / HOUR)
    } else {
        format!("{}d", secs / DAY)
    }
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
    fn workon_json_round_trips_through_write_meta() {
        let tmp = tempfile::tempdir().unwrap();
        let meta = WorkspaceMeta {
            base: Some("a1b2c3".to_string()),
            config: Some("opencode".to_string()),
            name: Some("fix bug".to_string()),
            created_db: Some("mbc_ws_abc_test".to_string()),
        };
        write_meta(tmp.path(), &meta).unwrap();

        let raw = std::fs::read_to_string(tmp.path().join(WORKON_JSON)).unwrap();
        let back: WorkspaceMeta = serde_json::from_str(&raw).unwrap();
        assert_eq!(back.base.as_deref(), Some("a1b2c3"));
        assert_eq!(back.config.as_deref(), Some("opencode"));
        assert_eq!(back.name.as_deref(), Some("fix bug"));
        assert_eq!(back.created_db.as_deref(), Some("mbc_ws_abc_test"));
    }

    #[test]
    fn workon_json_reads_nulls_and_missing_fields_as_none() {
        // A default (unnamed, no config, no db) workspace serializes every field
        // as null; an older/partial file may omit fields entirely.
        let all_null = serde_json::to_string(&WorkspaceMeta::default()).unwrap();
        let back: WorkspaceMeta = serde_json::from_str(&all_null).unwrap();
        assert!(back.base.is_none() && back.config.is_none() && back.created_db.is_none());

        let empty: WorkspaceMeta = serde_json::from_str("{}").unwrap();
        assert!(empty.base.is_none() && empty.name.is_none());
    }

    #[test]
    fn workon_json_is_a_generated_file() {
        // Teardown's changed-file filter must skip it, else the provenance file
        // would look like unsaved work worth prompting to save.
        assert!(GENERATED_FILES.contains(&WORKON_JSON));
    }

    #[test]
    fn token_matches_id_exactly_and_name_by_slug() {
        // Exact ws_id.
        assert!(token_matches("ws-abc123-fix-bug", "ws-abc123-fix-bug", Some("fix bug")));
        // Nickname given as stored (with spaces) or slugified — both match.
        assert!(token_matches("fix bug", "ws-abc123-fix-bug", Some("fix bug")));
        assert!(token_matches("fix-bug", "ws-abc123-fix-bug", Some("fix bug")));
        assert!(token_matches("Fix Bug", "ws-abc123-fix-bug", Some("fix bug")));
        // No match.
        assert!(!token_matches("other", "ws-abc123-fix-bug", Some("fix bug")));
        assert!(!token_matches("fix", "ws-abc123-fix-bug", Some("fix bug")));
        // No nickname: only the ws_id can match.
        assert!(token_matches("ws-abc123", "ws-abc123", None));
        assert!(!token_matches("anything", "ws-abc123", None));
    }

    #[test]
    fn humanize_age_scales_by_unit() {
        assert_eq!(humanize_age(5), "5s");
        assert_eq!(humanize_age(59), "59s");
        assert_eq!(humanize_age(60), "1m");
        assert_eq!(humanize_age(3599), "59m");
        assert_eq!(humanize_age(3600), "1h");
        assert_eq!(humanize_age(86_399), "23h");
        assert_eq!(humanize_age(86_400), "1d");
        assert_eq!(humanize_age(200_000), "2d");
    }

    fn git(dir: &Path, args: &[&str]) {
        let ok = Command::new("git")
            .args(args)
            .current_dir(dir)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        assert!(ok, "git {args:?} failed in {}", dir.display());
    }

    /// Build a real git repo `myproj` with one commit and a detached worktree
    /// named `myproj-<ws_id>`. Returns (project_dir, ws_dir).
    fn repo_with_worktree(root: &Path, ws_id: &str) -> (PathBuf, PathBuf) {
        let project = root.join("myproj");
        std::fs::create_dir(&project).unwrap();
        git(&project, &["init", "-q"]);
        git(&project, &["config", "user.email", "t@example.com"]);
        git(&project, &["config", "user.name", "Tester"]);
        git(&project, &["commit", "-q", "--allow-empty", "-m", "init"]);
        let ws_dir = root.join(format!("myproj-{ws_id}"));
        git(&project, &["worktree", "add", "-q", "--detach", ws_dir.to_str().unwrap(), "HEAD"]);
        (project, ws_dir)
    }

    fn git_out(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git").args(args).current_dir(dir).output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// A cloned git repo with an `origin/main` so `detect_trunk`/`create_workspace`
    /// resolve a base. Returns the working clone (the "project").
    fn cloned_repo(root: &Path) -> PathBuf {
        let origin = root.join("origin.git");
        let repo = root.join("proj");
        git(root, &["init", "-q", "--bare", "--initial-branch=main", origin.to_str().unwrap()]);
        git(root, &["clone", "-q", origin.to_str().unwrap(), repo.to_str().unwrap()]);
        git(&repo, &["config", "user.email", "t@example.com"]);
        git(&repo, &["config", "user.name", "Tester"]);
        git(&repo, &["commit", "-q", "--allow-empty", "-m", "init"]);
        git(&repo, &["push", "-q", "-u", "origin", "main"]);
        repo
    }

    #[test]
    fn provision_in_creates_worktree_and_records_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = cloned_repo(tmp.path());
        let worktrees = tmp.path().join("worktrees");

        let ws = provision_in(
            &worktrees,
            &repo,
            "proj",
            true, // skip gitignored-file copy: irrelevant here, keeps the test fast
            Some("fix bug"),
            Some("opencode"),
            &crate::vcs::GitBackend,
        )
        .unwrap();

        assert!(ws.ws_dir.starts_with(&worktrees), "worktree under the injected root");
        assert!(ws.ws_dir.is_dir());
        assert!(ws.base.is_some(), "base pinned from trunk");
        assert_eq!(ws.name.as_deref(), Some("fix bug"));
        assert_eq!(ws.config.as_deref(), Some("opencode"));
        assert!(ws.created_db.is_none());

        // .workon.json on disk carries the same provenance, so a fresh-process
        // load_workspace would recover it.
        let meta = read_meta(&ws.ws_dir);
        assert_eq!(meta.base, ws.base);
        assert_eq!(meta.config.as_deref(), Some("opencode"));
        assert_eq!(meta.name.as_deref(), Some("fix bug"));
    }

    #[test]
    fn teardown_no_save_forgets_worktree_and_reports_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = cloned_repo(tmp.path());
        let worktrees = tmp.path().join("worktrees");
        let ws = provision_in(&worktrees, &repo, "proj", true, None, None, &crate::vcs::GitBackend).unwrap();
        let ws_dir = ws.ws_dir.clone();

        let outcome = teardown(&ws, None, SaveMode::NoSave, &crate::vcs::GitBackend).unwrap();
        assert!(outcome.saved.is_empty());
        assert!(outcome.dropped_db.is_none());
        // forget_workspace removes the git worktree synchronously.
        assert!(!git_out(&repo, &["worktree", "list"]).contains(ws_dir.to_str().unwrap()));
    }

    #[test]
    fn teardown_save_rescues_committed_work_as_a_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = cloned_repo(tmp.path());
        let worktrees = tmp.path().join("worktrees");
        let ws = provision_in(&worktrees, &repo, "proj", true, None, None, &crate::vcs::GitBackend).unwrap();

        // Commit work on the detached HEAD that no branch names — exactly what
        // would be lost on teardown.
        std::fs::write(ws.ws_dir.join("new.txt"), "work").unwrap();
        git(&ws.ws_dir, &["add", "-A"]);
        git(&ws.ws_dir, &["commit", "-q", "-m", "wip"]);

        let outcome = teardown(&ws, None, SaveMode::Save, &crate::vcs::GitBackend).unwrap();

        let branch = format!("workon/{}", ws.ws_id);
        assert!(outcome.saved.contains(&branch), "reported saved: {:?}", outcome.saved);
        // The branch exists in the main repo, so the commit survives teardown.
        assert!(git_out(&repo, &["branch", "--list", &branch]).contains(&branch));
    }

    #[test]
    fn teardown_no_save_discards_committed_work() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = cloned_repo(tmp.path());
        let worktrees = tmp.path().join("worktrees");
        let ws = provision_in(&worktrees, &repo, "proj", true, None, None, &crate::vcs::GitBackend).unwrap();

        std::fs::write(ws.ws_dir.join("new.txt"), "work").unwrap();
        git(&ws.ws_dir, &["add", "-A"]);
        git(&ws.ws_dir, &["commit", "-q", "-m", "wip"]);

        let outcome = teardown(&ws, None, SaveMode::NoSave, &crate::vcs::GitBackend).unwrap();

        assert!(outcome.saved.is_empty());
        let branch = format!("workon/{}", ws.ws_id);
        assert!(!git_out(&repo, &["branch", "--list", &branch]).contains(&branch), "no branch created");
    }

    #[test]
    fn ref_candidates_offers_ws_id_and_nickname_slug() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = cloned_repo(tmp.path());
        let worktrees = tmp.path().join("worktrees");
        let ws = provision_in(&worktrees, &repo, "proj", true, Some("fix bug"), None, &crate::vcs::GitBackend).unwrap();

        let values: Vec<String> = ref_candidates_from(&worktrees)
            .iter()
            .map(|c| c.get_value().to_string_lossy().into_owned())
            .collect();

        assert!(values.contains(&ws.ws_id), "ws_id offered: {values:?}");
        assert!(values.iter().any(|v| v == "fix-bug"), "nickname slug offered: {values:?}");
    }

    #[test]
    fn ref_candidates_empty_for_missing_root() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(ref_candidates_from(&tmp.path().join("nope")).is_empty());
    }

    #[test]
    fn load_workspace_recovers_structure_and_meta_from_a_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let (_project, ws_dir) = repo_with_worktree(tmp.path(), "ws-abc123");
        write_meta(
            &ws_dir,
            &WorkspaceMeta {
                base: Some("deadbeef".into()),
                config: Some("opencode".into()),
                name: Some("Fix Bug".into()),
                created_db: Some("myproj_ws_abc123_test".into()),
            },
        )
        .unwrap();

        let ws = load_workspace(Some(ws_dir.to_str().unwrap())).unwrap();
        assert_eq!(ws.ws_id, "ws-abc123");
        assert_eq!(ws.project_name, "myproj");
        assert_eq!(ws.project_dir.file_name().unwrap(), "myproj");
        assert_eq!(ws.base.as_deref(), Some("deadbeef"));
        assert_eq!(ws.config.as_deref(), Some("opencode"));
        assert_eq!(ws.name.as_deref(), Some("Fix Bug"));
        assert_eq!(ws.created_db.as_deref(), Some("myproj_ws_abc123_test"));
    }

    #[test]
    fn load_workspace_tolerates_absent_meta() {
        // A worktree with no .workon.json (older or partial create) still loads;
        // provenance is None so teardown will skip unsaved-work detection.
        let tmp = tempfile::tempdir().unwrap();
        let (_project, ws_dir) = repo_with_worktree(tmp.path(), "ws-nometa");

        let ws = load_workspace(Some(ws_dir.to_str().unwrap())).unwrap();
        assert_eq!(ws.ws_id, "ws-nometa");
        assert!(ws.base.is_none());
        assert!(ws.config.is_none());
        assert!(ws.name.is_none());
    }

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
