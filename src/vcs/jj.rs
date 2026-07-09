use std::path::Path;

use anyhow::{bail, Context, Result};
use vcs_runner::{
    parse_diff_summary, run_git, run_git_utf8, run_jj, run_jj_utf8, run_jj_utf8_ignore_wc, RunError,
};

use super::{detect_git_remote, path_str, Vcs};

pub struct JjBackend;

impl JjBackend {
    /// Resolve a revset to a single immutable commit id. Read-only
    /// (`--ignore-working-copy`), so it works even when the working copy is
    /// stale and returns nothing when the revset matches no commit — which is
    /// how we distinguish "no trunk yet" from other failures.
    fn resolve_commit(&self, project_dir: &Path, revset: &str) -> Result<String> {
        let out = run_jj_utf8(
            project_dir,
            &["log", "--ignore-working-copy", "--no-graph", "-r", revset,
              "-T", "commit_id ++ \"\\n\"", "--limit", "1"],
        )?;
        let id = out.lines().next().unwrap_or("").trim().to_string();
        if id.is_empty() {
            bail!("revision `{revset}` resolved to no commit");
        }
        Ok(id)
    }

    /// `jj workspace add`, recovering once from a stale main working copy.
    ///
    /// `jj workspace add` snapshots the source working copy, so it aborts with
    /// "working copy is stale" when the main repo hasn't been refreshed since an
    /// operation elsewhere. It also creates the workspace entry *before* hitting
    /// that error, leaving an orphan behind — so on any failure we forget the
    /// partial workspace, and on the stale case we run `update-stale` and retry.
    ///
    /// There is deliberately no guard against a *concurrent op-log reconcile*
    /// here. `jj workspace add` rewrites no history — the corruption class that
    /// a reconcile enables is a bad rebase onto a reconciled state, which
    /// `workspace add` cannot produce — and jj preserves both sides of a fork
    /// regardless. Concurrency is safe; workon only needs to *surface* a
    /// divergence, which `workon list`'s status column does (see `active_status`
    /// in `workspace.rs`, reading `divergent()` via `jj_divergent_change_ids`).
    fn add_workspace(&self, project_dir: &Path, ws_dir: &Path, ws_id: &str, rev: &str) -> Result<()> {
        let add = || self.run_workspace_add(project_dir, ws_dir, ws_id, rev);

        match add() {
            Ok(()) => Ok(()),
            Err(e) if is_stale_working_copy(&e) => {
                eprintln!("Main repo working copy is stale; recovering with `jj workspace update-stale`...");
                self.cleanup_partial_workspace(ws_id, project_dir, ws_dir);
                run_jj(project_dir, &["workspace", "update-stale"])
                    .context("failed to refresh stale working copy")?;
                add().map_err(|e2| {
                    self.cleanup_partial_workspace(ws_id, project_dir, ws_dir);
                    anyhow::Error::new(e2).context("failed to create jj workspace after update-stale")
                })
            }
            Err(e) => {
                self.cleanup_partial_workspace(ws_id, project_dir, ws_dir);
                Err(anyhow::Error::new(e).context("failed to create jj workspace"))
            }
        }
    }

    fn run_workspace_add(&self, project_dir: &Path, ws_dir: &Path, ws_id: &str, rev: &str) -> Result<(), RunError> {
        run_jj(
            project_dir,
            &["workspace", "add", &path_str(ws_dir), "--name", ws_id, "-r", rev],
        )
        .map(|_| ())
    }

    /// Remove a workspace `jj workspace add` left half-created on failure: forget
    /// the entry, drop the git worktree plumbing, and delete the directory so a
    /// retry (or the next run) starts clean. Every step is best-effort.
    fn cleanup_partial_workspace(&self, ws_id: &str, project_dir: &Path, ws_dir: &Path) {
        let _ = run_jj(project_dir, &["workspace", "forget", ws_id]);
        if let Some(git_dir) = absolute_git_dir(project_dir) {
            let _ = std::fs::remove_dir_all(format!("{git_dir}/worktrees/{ws_id}"));
        }
        let _ = std::fs::remove_dir_all(ws_dir);
    }

    /// Whether `revset` matches at least one commit. Read-only.
    fn revset_nonempty(&self, project_dir: &Path, revset: &str) -> bool {
        run_jj_utf8(
            project_dir,
            &["log", "--ignore-working-copy", "--no-graph", "-r", revset, "-T", r#""x""#],
        )
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
    }

    /// Commit ids this workspace's `@` moved away from, parsed from
    /// `jj op log --op-diff`. Each operation prints a `Changed working copy
    /// <ws>@:` block whose `- <change_id> <commit_id> …` lines are the commits
    /// `@` left behind. Scoped to this workspace's blocks only — the basis for
    /// concurrency-proof attribution. Callers filter to those still stranded.
    fn abandoned_working_copies(&self, ws_id: &str, project_dir: &Path) -> Vec<String> {
        let header = format!("Changed working copy {ws_id}@:");
        // Working-copy-agnostic: teardown reads the op log to attribute orphans;
        // it must not snapshot the (possibly stale) working copy while doing so.
        let Ok(out) = run_jj_utf8_ignore_wc(project_dir, &["op", "log", "--op-diff", "--no-graph"]) else {
            return Vec::new();
        };
        let mut ids = Vec::new();
        let mut in_block = false;
        for line in out.lines() {
            let t = line.trim();
            if t == header {
                in_block = true;
            } else if !in_block {
                continue;
            } else if let Some(rest) = t.strip_prefix("- ") {
                if let Some(commit_id) = rest.split_whitespace().nth(1) {
                    ids.push(commit_id.to_string());
                }
            } else if !t.starts_with("+ ") {
                in_block = false;
            }
        }
        ids.sort();
        ids.dedup();
        ids
    }
}

/// One-time jj initialization for a git repo that doesn't have .jj yet.
pub(crate) fn init_jj(project_dir: &Path) -> Result<()> {
    eprintln!("Initializing jj colocated repo in {}...", project_dir.display());
    run_jj(project_dir, &["git", "init", "--colocate"])?;

    let (main_branch, remote) = detect_trunk_git(project_dir);

    run_jj(project_dir, &["bookmark", "track", &format!("{main_branch}@{remote}")])?;

    let auto_track_key = format!("remotes.{remote}.auto-track-bookmarks");
    run_jj(project_dir, &["config", "set", "--repo", &auto_track_key, "glob:*"])?;

    eprintln!("jj initialized, tracking {main_branch}@{remote}");
    Ok(())
}

/// Extract the first non-@git bookmark from jj's `bookmarks` template output.
/// Returns the full form (e.g. "master@heroku") so it resolves as a jj revision
/// even when the bookmark isn't tracked locally.
///
/// Strips trailing `*` (out-of-sync with tracked remote) and `?` (conflict)
/// markers that jj's `bookmarks` template appends to the name.
fn first_real_bookmark(raw: &str) -> &str {
    raw.split_whitespace()
        .map(|b| b.trim_end_matches(['*', '?']))
        .find(|b| !b.is_empty() && !b.ends_with("@git"))
        .unwrap_or("")
}

fn detect_trunk_git(project_dir: &Path) -> (String, String) {
    let remote = detect_git_remote(project_dir);
    let has_master = run_git(project_dir, &["rev-parse", "--verify", &format!("{remote}/master")]).is_ok();

    let branch = if has_master { "master" } else { "main" };
    (branch.into(), remote)
}

/// Revset matching a bookmark by exact name on the `origin` remote only, so a
/// stale deploy-mirror remote can't shadow the canonical trunk.
fn remote_scoped(name: &str) -> String {
    format!(r#"remote_bookmarks(exact:"{name}", remote=exact:"origin")"#)
}

/// Resolve a single trunk revset to its first real bookmark, or `None` when the
/// revset is empty / errors. Kept separate so `detect_trunk` reads as a plain
/// priority list of candidates.
fn jj_trunk_bookmark(project_dir: &Path, revset: &str) -> Option<String> {
    let output = run_jj_utf8(
        project_dir,
        &["log", "-r", revset, "--no-graph", "-T", "bookmarks", "--limit", "1"],
    )
    .ok()?;
    let bookmark = first_real_bookmark(&output);
    (!bookmark.is_empty()).then(|| bookmark.to_string())
}

impl Vcs for JjBackend {
    fn detect_trunk(&self, project_dir: &Path) -> Result<String> {
        // Candidates in priority order. Every jj step is origin-scoped so a
        // deploy-mirror remote can never win: the old fallback ranked
        // `remote_bookmarks("master") | remote_bookmarks("main")` across *all*
        // remotes with `latest()`, i.e. by commit timestamp, so a deploy mirror
        // (e.g. a heroku remote) whose `main` was pushed after origin's `master`
        // froze every workspace on the wrong trunk. `trunk()` covers the common
        // case; the explicit origin/main|master|trunk lookups rescue repos where
        // jj's `trunk()` alias doesn't resolve but the bookmarks are known.
        let origin_revsets = [
            "trunk()".to_string(),
            remote_scoped("main"),
            remote_scoped("master"),
            remote_scoped("trunk"),
        ];
        for revset in &origin_revsets {
            if let Some(bookmark) = jj_trunk_bookmark(project_dir, revset) {
                return Ok(bookmark);
            }
        }

        // jj's bookmark view can lag git's: a repo first initialized against a
        // deploy mirror never learned origin's bookmark, so none of the revsets
        // above resolve. Consult git's remote-tracking refs directly
        // (origin-preferring, via detect_trunk_git) and pin to the commit id,
        // which resolves in jj even when the bookmark was never tracked.
        let (branch, remote) = detect_trunk_git(project_dir);
        if let Ok(cid) = run_git_utf8(project_dir, &["rev-parse", "--verify", &format!("{remote}/{branch}")]) {
            let cid = cid.trim();
            if !cid.is_empty() {
                return Ok(cid.to_string());
            }
        }

        // Last resort — any remote's master, then main. Only reached when
        // neither jj nor git can point at an origin/deploy trunk; a
        // single-non-origin-remote repo whose git refs weren't consulted above.
        for revset in [r#"latest(remote_bookmarks(exact:"master"))"#, r#"latest(remote_bookmarks(exact:"main"))"#] {
            if let Some(bookmark) = jj_trunk_bookmark(project_dir, revset) {
                return Ok(bookmark);
            }
        }

        Ok("main".into())
    }

    fn create_workspace(&self, project_dir: &Path, ws_dir: &Path, ws_id: &str, trunk: &str) -> Result<String> {
        eprintln!("Creating jj workspace {ws_id}...");

        // Pin the base before creating the workspace. Resolving here also gives a
        // far better error than jj's bare "Revision doesn't exist" when the repo
        // has no commits yet — the only way `trunk` fails to resolve.
        let base = self.resolve_commit(project_dir, trunk).with_context(|| {
            format!(
                "trunk revision `{trunk}` doesn't resolve to a commit — the repo \
                 likely has no commits yet. Create one, e.g.\n    \
                 jj describe -m \"Initial commit\" && jj bookmark create {trunk} -r @ && jj new\n\
                 then re-run."
            )
        })?;

        // Branch from the pinned commit, not the `trunk` name — so the workspace's
        // actual branch point is provably the same commit teardown diffs against,
        // with no window where a moving trunk could desync the two.
        self.add_workspace(project_dir, ws_dir, ws_id, &base)?;

        // jj workspaces don't have a .git directory, so git commands
        // (branchdiff, git log, etc.) fail inside the workspace. Set up a
        // git worktree reference, detached at the same pinned base, so git works
        // alongside jj and the two can't disagree on where the workspace started.
        if let Err(e) = setup_git_worktree(project_dir, ws_dir, ws_id, &base) {
            eprintln!("Warning: could not set up git worktree for workspace: {e}");
        }

        Ok(base)
    }

    fn pre_copy_sync(&self, project_dir: &Path) {
        // Running any jj command triggers an automatic snapshot in modern jj,
        // which ensures the git index is in sync with jj's working copy so
        // that git ls-files --ignored returns accurate results.
        let _ = run_jj(project_dir, &["status"]);
    }

    fn changed_files(&self, ws_id: &str, base: &str, project_dir: &Path, _ws_dir: &Path) -> Vec<String> {
        let ws_head = format!("{ws_id}@");
        // Is there non-empty work in the stack that no bookmark or remote names?
        // Excluding ancestors(bookmarks | remote_bookmarks) means a stack you've
        // already bookmarked or pushed reports nothing — no spurious prompt.
        // (The whole-stack range also catches work parked on an ancestor of ws@
        // by `jj commit`, which a point query at ws@ would miss.)
        //
        // `base` is the pinned branch point, not a re-resolved trunk bookmark, so
        // a fetch that advanced trunk mid-session can't leak upstream commits in.
        let unsaved = format!(
            "({base}..{ws_head}) & ~empty() & ~ancestors(bookmarks() | remote_bookmarks())"
        );
        if !self.revset_nonempty(project_dir, &unsaved) {
            return Vec::new();
        }
        run_jj_utf8(
            project_dir,
            &["diff", "--ignore-working-copy", "--from", base, "--to", &ws_head, "--summary"],
        )
        .map(|stdout| {
            parse_diff_summary(&stdout)
                .into_iter()
                .map(|c| c.path.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
    }

    fn save_work(&self, ws_id: &str, base: &str, project_dir: &Path, _ws_dir: &Path) -> Result<()> {
        // Bookmark the tip of the non-empty stack, not ws@ — ws@ is empty
        // whenever the agent committed its work, so bookmarking it would save
        // nothing. Fall back to ws@ if (defensively) the stack is all-empty.
        let ws_head = format!("{ws_id}@");
        let target = run_jj_utf8(
            project_dir,
            &["log", "--ignore-working-copy", "--no-graph", "-T", "commit_id ++ \"\\n\"",
              "-r", &format!("heads(({base}..{ws_head}) & ~empty())")],
        )
        .ok()
        .and_then(|s| s.lines().next().map(str::to_string))
        .filter(|s| !s.is_empty())
        .unwrap_or(ws_head);

        run_jj(
            project_dir,
            &["bookmark", "set", &format!("workon/{ws_id}"), "-r", &target],
        )?;
        eprintln!("Bookmarked as workon/{ws_id}");
        Ok(())
    }

    fn stranded_work(&self, ws_id: &str, _base: &str, project_dir: &Path, _ws_dir: &Path) -> Vec<String> {
        // Candidates: commits this workspace's @ moved away from, recovered from
        // the operation log. `jj op log --op-diff` prints, per operation, a
        // "Changed working copy <ws>@:" block whose `-` line is the commit @ left
        // behind. Scoping to *this* workspace's blocks is what makes attribution
        // concurrency-proof (a repo-wide revset would also catch other
        // workspaces' orphans).
        let candidates = self.abandoned_working_copies(ws_id, project_dir);
        if candidates.is_empty() {
            return Vec::new();
        }
        // Keep only those still genuinely stranded: non-empty, unsaved, and
        // unreachable from any workspace.
        let revset = format!(
            "({}) & ~empty() & ~ancestors(bookmarks() | remote_bookmarks()) \
             & ~working_copies() & ~ancestors(working_copies())",
            candidates.join("|")
        );
        run_jj_utf8(
            project_dir,
            &["log", "--ignore-working-copy", "--no-graph", "-r", &revset,
              "-T", r#"commit_id.shortest(8) ++ "  " ++ if(description, description.first_line(), "(no description set)") ++ "\n""#],
        )
        .map(|s| s.lines().filter(|l| !l.trim().is_empty()).map(String::from).collect())
        .unwrap_or_default()
    }

    fn save_stranded(&self, project_dir: &Path, ws_id: &str, commit_id: &str) -> Result<()> {
        let name = format!("workon/{ws_id}-{commit_id}");
        run_jj(project_dir, &["bookmark", "set", &name, "-r", commit_id])?;
        eprintln!("Saved stranded commit {commit_id} as {name}");
        Ok(())
    }

    fn forget_workspace(&self, ws_id: &str, project_dir: &Path, _ws_dir: &Path) {
        let _ = run_jj(project_dir, &["workspace", "forget", ws_id]);

        // Clean up the git worktree reference we created alongside the jj workspace.
        if let Some(git_dir) = absolute_git_dir(project_dir) {
            let wt_dir = format!("{git_dir}/worktrees/{ws_id}");
            let _ = std::fs::remove_dir_all(wt_dir);
        }

        eprintln!("Forgot jj workspace {ws_id}");
    }

    fn ignore_generated_file(&self, project_dir: &Path, ws_dir: &Path, relpath: &str) {
        // The exclude has to land first: `jj file untrack` refuses to drop a
        // path unless it's already ignored, otherwise jj would re-snapshot it
        // on the next working-copy refresh.
        if let Err(e) = super::append_git_exclude(project_dir, relpath) {
            eprintln!("Warning: could not exclude {relpath} from git: {e}");
            return;
        }
        if let Err(e) = run_jj(ws_dir, &["file", "untrack", relpath]) {
            eprintln!("Warning: could not untrack {relpath} in jj: {e}");
        }
    }
}

/// Whether a failed jj invocation aborted because the working copy is stale —
/// recoverable with `jj workspace update-stale`. (Newer jj auto-recovers on
/// access; older versions, like the one that surfaced this bug, hard-error.)
fn is_stale_working_copy(err: &RunError) -> bool {
    err.stderr().is_some_and(stderr_reports_stale)
}

/// jj's stale-working-copy abort always carries this phrase, e.g.
/// "Error: The working copy is stale (not updated since operation abc123)."
fn stderr_reports_stale(stderr: &str) -> bool {
    stderr.contains("working copy is stale")
}

fn absolute_git_dir(project_dir: &Path) -> Option<String> {
    run_git_utf8(project_dir, &["rev-parse", "--absolute-git-dir"])
        .ok()
        .filter(|s| !s.is_empty())
}

/// Set up a git worktree reference in a jj workspace so that git commands work.
///
/// jj workspaces don't create a `.git` entry, which means git commands,
/// branchdiff, and tools that expect a git repo all fail inside the workspace.
/// This creates the minimal git worktree plumbing: a `.git` file pointing to a
/// worktree entry under the main repo's `.git/worktrees/` directory.
///
/// `base` is the pinned commit jj branched the workspace from. We detach git's
/// HEAD at exactly that commit — *not* a re-resolved `<remote>/<trunk>` — so the
/// git and jj views can't diverge. Guessing a remote was an active bug: on a repo
/// whose first remote is a stale deploy mirror (e.g. a heroku remote ordered
/// before origin), `<remote>/master` resolved to a ref ~1000 commits behind, and
/// branchdiff/git tooling showed that stale state instead of jj's real `@`.
fn setup_git_worktree(project_dir: &Path, ws_dir: &Path, ws_id: &str, base: &str) -> Result<()> {
    let git_dir = absolute_git_dir(project_dir)
        .context("could not determine .git directory")?;
    let wt_git_dir = format!("{git_dir}/worktrees/{ws_id}");

    std::fs::create_dir_all(&wt_git_dir)?;
    std::fs::write(format!("{wt_git_dir}/gitdir"), format!("{}/.git\n", path_str(ws_dir)))?;
    std::fs::write(format!("{wt_git_dir}/commondir"), "../..\n")?;
    std::fs::write(format!("{wt_git_dir}/HEAD"), format!("{base}\n"))?;

    // Point the workspace at this worktree so git commands work.
    std::fs::write(ws_dir.join(".git"), format!("gitdir: {wt_git_dir}\n"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use super::*;

    #[test]
    fn first_real_bookmark_picks_non_git_entry() {
        assert_eq!(first_real_bookmark("master@heroku master@git"), "master@heroku");
    }

    #[test]
    fn first_real_bookmark_returns_bare_name() {
        assert_eq!(first_real_bookmark("main"), "main");
    }

    #[test]
    fn first_real_bookmark_skips_git_only() {
        assert_eq!(first_real_bookmark("main@git"), "");
    }

    #[test]
    fn first_real_bookmark_empty_input() {
        assert_eq!(first_real_bookmark(""), "");
        assert_eq!(first_real_bookmark("   "), "");
    }

    #[test]
    fn first_real_bookmark_strips_sync_markers() {
        // jj's `bookmarks` template appends `*` when a local bookmark is
        // out of sync with its tracked remote, and `?` for conflicts.
        // These suffixes are display indicators, not part of the revision name.
        assert_eq!(first_real_bookmark("master*"), "master");
        assert_eq!(first_real_bookmark("master?"), "master");
        assert_eq!(first_real_bookmark("master* master@heroku_test master@git"), "master");
        assert_eq!(first_real_bookmark("master*?"), "master");
    }

    fn git(repo: &Path, args: &[&str]) {
        let repo_path = path_str(repo);
        let mut full = vec!["-C", &*repo_path];
        full.extend_from_slice(args);
        Command::new("git").args(&full)
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
    }

    /// Regression for the issue: `.env.test.local` written into a fresh jj
    /// *workspace* (not the main repo) must not surface as a phantom `A` change.
    /// Exercises the real topology — exclude lands in the main repo's git dir,
    /// untrack runs inside the separate workspace working copy.
    #[test]
    fn ignore_generated_file_drops_phantom_add_in_jj_workspace() {
        // Requires jj on PATH; CI doesn't install it, so skip there.
        if !vcs_runner::jj_available() {
            return;
        }

        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&repo).unwrap();

        git(&repo, &["init", "--initial-branch=main"]);
        git(&repo, &["config", "user.email", "t@t.com"]);
        git(&repo, &["config", "user.name", "T"]);
        std::fs::write(repo.join("README"), "hi").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "init"]);

        run_jj(&repo, &["git", "init", "--colocate"]).unwrap();
        run_jj(&repo, &["workspace", "add", &path_str(&ws), "--name", "testws", "-r", "main"]).unwrap();

        std::fs::write(ws.join(".env.test.local"), "DATABASE_URL=postgresql://localhost/x").unwrap();

        // Sanity: jj sees the generated file as a phantom add before we exclude it.
        let before = run_jj_utf8(&ws, &["status"]).unwrap();
        assert!(before.contains(".env.test.local"), "jj should track the file initially");

        JjBackend.ignore_generated_file(&repo, &ws, ".env.test.local");

        let after = run_jj_utf8(&ws, &["status"]).unwrap();
        assert!(
            !after.contains(".env.test.local"),
            "excluded + untracked file should not surface in jj status, got:\n{after}"
        );
    }

    #[test]
    fn setup_git_worktree_enables_git_commands() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        Command::new("git")
            .args(["init", "--initial-branch=main", &path_str(&project)])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        Command::new("git")
            .args(["-C", &path_str(&project), "config", "user.email", "t@t.com"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git")
            .args(["-C", &path_str(&project), "config", "user.name", "T"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        std::fs::write(project.join("README"), "hi").unwrap();
        Command::new("git")
            .args(["-C", &path_str(&project), "add", "."])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git")
            .args(["-C", &path_str(&project), "commit", "-m", "init"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        let base = run_git_utf8(&project, &["rev-parse", "HEAD"]).unwrap().trim().to_string();
        setup_git_worktree(&project, &ws, "test-ws", &base).unwrap();

        assert!(ws.join(".git").is_file(), ".git file should exist in workspace");

        let log = Command::new("git")
            .args(["-C", &path_str(&ws), "log", "--oneline", "-1"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .unwrap();
        assert!(log.status.success(), "git log should work in workspace");
        let output = String::from_utf8_lossy(&log.stdout);
        assert!(output.contains("init"), "should see the commit");
    }

    /// Regression for the heroku-remote bug: the git worktree HEAD must be the
    /// exact pinned base commit, never a re-resolved `<remote>/<trunk>`. A repo
    /// whose first-listed remote is a stale deploy mirror froze every worktree's
    /// git HEAD ~1000 commits behind jj's real `@`. setup_git_worktree no longer
    /// consults remotes at all; this proves the written HEAD equals `base`.
    #[test]
    fn setup_git_worktree_pins_head_to_base_not_a_remote_ref() {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("project");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();

        Command::new("git")
            .args(["init", "--initial-branch=main", &path_str(&project)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        git(&project, &["config", "user.email", "t@t.com"]);
        git(&project, &["config", "user.name", "T"]);
        std::fs::write(project.join("README"), "hi").unwrap();
        git(&project, &["add", "."]);
        git(&project, &["commit", "-m", "init"]);

        // A stale "deploy mirror" ref that a buggy remote-guess could pick up.
        git(&project, &["update-ref", "refs/remotes/heroku/master", "HEAD"]);
        std::fs::write(project.join("README"), "real master moved on").unwrap();
        git(&project, &["commit", "-am", "advance real master"]);
        let base = run_git_utf8(&project, &["rev-parse", "HEAD"]).unwrap().trim().to_string();

        setup_git_worktree(&project, &ws, "ws-pin", &base).unwrap();

        let git_dir = run_git_utf8(&project, &["rev-parse", "--absolute-git-dir"]).unwrap();
        let head = std::fs::read_to_string(format!("{}/worktrees/ws-pin/HEAD", git_dir.trim())).unwrap();
        assert_eq!(head.trim(), base, "worktree HEAD must equal the pinned base commit");
    }

    /// Colocated jj repo with an origin remote (so `trunk()` resolves) and a
    /// named workspace branched from master. Returns (repo, workspace) dirs.
    fn setup_ws(tmp: &std::path::Path, ws_id: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let repo = tmp.join("repo");
        let origin = tmp.join("origin.git");
        let ws = tmp.join(format!("ws-{ws_id}"));
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--initial-branch=master"]);
        git(&repo, &["config", "user.email", "t@t.com"]);
        git(&repo, &["config", "user.name", "T"]);
        std::fs::write(repo.join("app.rb"), "v1").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "init"]);
        Command::new("git")
            .args(["clone", "--bare", &path_str(&repo), &path_str(&origin)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        git(&repo, &["remote", "add", "origin", &path_str(&origin)]);
        git(&repo, &["push", "origin", "master"]);
        run_jj(&repo, &["git", "init", "--colocate"]).unwrap();
        run_jj(&repo, &["bookmark", "track", "master@origin"]).unwrap();
        run_jj(&repo, &["workspace", "add", &path_str(&ws), "--name", ws_id, "-r", "master"]).unwrap();
        (repo, ws)
    }

    /// Regression for the deploy-mirror bug: a mirror remote (heroku) whose
    /// `main` was pushed *after* origin's `master` must not be chosen as trunk.
    /// The old fallback ranked `remote_bookmarks("master") | ("main")` across
    /// all remotes by commit timestamp, so the newer mirror `main` won. Here we
    /// disable jj's `trunk()` alias to force the fallback path (a repo where jj
    /// can't derive trunk from origin) and assert origin still wins over the
    /// newer mirror.
    #[test]
    fn detect_trunk_prefers_origin_over_newer_deploy_mirror() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (repo, _ws) = setup_ws(tmp.path(), "wtrunk");

        // A deploy mirror with a `main` bookmark advanced past origin's master.
        let heroku = tmp.path().join("heroku.git");
        Command::new("git")
            .args(["init", "--bare", "--initial-branch=main", &path_str(&heroku)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        git(&repo, &["checkout", "-b", "main"]);
        std::fs::write(repo.join("app.rb"), "mirror is newer").unwrap();
        git(&repo, &["commit", "-am", "newer on mirror main"]);
        git(&repo, &["remote", "add", "heroku", &path_str(&heroku)]);
        git(&repo, &["push", "heroku", "main"]);
        git(&repo, &["checkout", "master"]);
        // Make jj learn the mirror bookmark, then force the fallback path by
        // stubbing out `trunk()` (origin's default branch is undiscoverable).
        run_jj(&repo, &["git", "fetch", "--remote", "heroku"]).unwrap();
        run_jj(&repo, &["config", "set", "--repo", r#"revset-aliases."trunk()""#, "none()"]).unwrap();

        let trunk = JjBackend.detect_trunk(&repo).unwrap();
        assert_eq!(trunk, "master", "must pick origin's master, not the newer heroku main, got {trunk:?}");
    }

    /// The pinned base commit id (master) the workspaces in these tests branch
    /// from — the value `create_workspace` returns in production.
    fn jj_base(repo: &Path) -> String {
        run_jj_utf8(
            repo,
            &["log", "--ignore-working-copy", "--no-graph", "-r", "master", "-T", "commit_id", "--limit", "1"],
        )
        .unwrap()
        .trim()
        .to_string()
    }

    /// Regression: an agent that commits its work leaves ws@ empty, so the old
    /// point query `-r ws@` reported nothing and the work was silently dropped.
    /// The range query must see it.
    #[test]
    fn changed_files_detects_committed_work() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (repo, ws) = setup_ws(tmp.path(), "wsa");
        let base = jj_base(&repo);
        std::fs::write(ws.join("app.rb"), "agent change").unwrap();
        run_jj(&ws, &["commit", "-m", "implement feature"]).unwrap();

        let changed = JjBackend.changed_files("wsa", &base, &repo, &ws);
        assert!(changed.contains(&"app.rb".to_string()), "should detect committed work, got {changed:?}");
    }

    /// save_work must bookmark the non-empty stack tip, not the empty ws@ that
    /// a commit leaves behind — otherwise the bookmark would capture nothing.
    #[test]
    fn save_work_bookmarks_committed_stack_tip() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (repo, ws) = setup_ws(tmp.path(), "wsb");
        let base = jj_base(&repo);
        std::fs::write(ws.join("app.rb"), "agent change").unwrap();
        run_jj(&ws, &["commit", "-m", "implement feature"]).unwrap();

        JjBackend.save_work("wsb", &base, &repo, &ws).unwrap();

        let out = run_jj_utf8(
            &repo,
            &["log", "--ignore-working-copy", "--no-graph", "-r", "workon/wsb",
              "-T", r#"description.first_line() ++ "|" ++ if(empty, "empty", "nonempty")"#],
        )
        .unwrap();
        assert!(out.contains("implement feature"), "bookmark should sit on the work commit, got {out}");
        assert!(out.contains("nonempty"), "bookmark target must be non-empty, got {out}");
    }

    /// The "finish, bookmark, leave" flow must stay silent — bookmarked work is
    /// findable, so it should not trigger the save prompt.
    #[test]
    fn changed_files_silent_for_bookmarked_work() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (repo, ws) = setup_ws(tmp.path(), "wse");
        let base = jj_base(&repo);
        std::fs::write(ws.join("app.rb"), "agent change").unwrap();
        run_jj(&ws, &["commit", "-m", "finish the PR"]).unwrap();
        run_jj(&ws, &["bookmark", "set", "my-feature", "-r", "@-"]).unwrap();

        assert!(
            JjBackend.changed_files("wse", &base, &repo, &ws).is_empty(),
            "bookmarked work is already saved and must not prompt"
        );
    }

    /// The bug op-log attribution fixes: two workspaces sharing the repo each
    /// strand an orphan. A repo-wide scan would let either teardown claim both;
    /// per-workspace attribution must return only that workspace's own.
    #[test]
    fn stranded_work_attributes_per_workspace() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (repo, ws1) = setup_ws(tmp.path(), "wsone");
        let ws2 = tmp.path().join("ws-two");
        run_jj(&repo, &["workspace", "add", &path_str(&ws2), "--name", "wstwo", "-r", "master"]).unwrap();

        std::fs::write(ws1.join("app.rb"), "ws1 edit").unwrap();
        run_jj(&ws1, &["new", "master", "-m", "ws1 fresh"]).unwrap();
        std::fs::write(ws2.join("app.rb"), "ws2 edit").unwrap();
        run_jj(&ws2, &["new", "master", "-m", "ws2 fresh"]).unwrap();

        let base = jj_base(&repo);
        let s1 = JjBackend.stranded_work("wsone", &base, &repo, &ws1);
        let s2 = JjBackend.stranded_work("wstwo", &base, &repo, &ws2);
        assert_eq!(s1.len(), 1, "wsone should see only its own orphan, got {s1:?}");
        assert_eq!(s2.len(), 1, "wstwo should see only its own orphan, got {s2:?}");
        assert_ne!(
            s1[0].split_whitespace().next(),
            s2[0].split_whitespace().next(),
            "the two workspaces must not be attributed the same commit"
        );
    }

    #[test]
    fn stranded_work_empty_for_clean_and_excludes_saved() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (repo, ws) = setup_ws(tmp.path(), "wsx");
        let base = jj_base(&repo);
        assert!(JjBackend.stranded_work("wsx", &base, &repo, &ws).is_empty(), "clean workspace strands nothing");

        std::fs::write(ws.join("app.rb"), "edit").unwrap();
        run_jj(&ws, &["new", "master", "-m", "fresh"]).unwrap();
        let s = JjBackend.stranded_work("wsx", &base, &repo, &ws);
        assert_eq!(s.len(), 1, "the jj-new orphan should be attributed, got {s:?}");

        let id = s[0].split_whitespace().next().unwrap().to_string();
        JjBackend.save_stranded(&repo, "wsx", &id).unwrap();
        assert!(
            JjBackend.stranded_work("wsx", &base, &repo, &ws).is_empty(),
            "a saved (bookmarked) stranded commit is no longer stranded"
        );
    }

    /// The base-pinning fix: a long session can outlive a fetch that advances
    /// trunk. Teardown must diff against the pinned branch point, not the moved
    /// trunk — otherwise unrelated upstream commits surface as the workspace's
    /// own "changed" files (the "14 phantom files, only test.txt changed" bug).
    #[test]
    fn changed_files_uses_pinned_base_not_moved_trunk() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (repo, _other) = setup_ws(tmp.path(), "wsother");
        let ws = tmp.path().join("ws-pin");
        let base = JjBackend.create_workspace(&repo, &ws, "wspin", "master").unwrap();

        // Trunk advances in the main repo, as a mid-session fetch would.
        std::fs::write(repo.join("upstream.rb"), "upstream change").unwrap();
        run_jj(&repo, &["commit", "-m", "upstream work"]).unwrap();
        run_jj(&repo, &["bookmark", "set", "master", "-r", "@-"]).unwrap();
        let moved_trunk = jj_base(&repo);
        assert_ne!(base, moved_trunk, "precondition: trunk moved off the branch point");

        // The workspace does its own, unrelated work. The repo op above left this
        // workspace's copy stale — refresh it, just as the teardown path does.
        run_jj(&ws, &["workspace", "update-stale"]).unwrap();
        std::fs::write(ws.join("app.rb"), "agent change").unwrap();
        run_jj(&ws, &["commit", "-m", "agent work"]).unwrap();

        let pinned = JjBackend.changed_files("wspin", &base, &repo, &ws);
        assert!(pinned.contains(&"app.rb".to_string()), "own work must be reported, got {pinned:?}");
        assert!(
            !pinned.contains(&"upstream.rb".to_string()),
            "pinned base must not leak upstream changes, got {pinned:?}"
        );

        // Regression guard: re-resolving the now-moved trunk is exactly the old bug.
        let leaked = JjBackend.changed_files("wspin", &moved_trunk, &repo, &ws);
        assert!(
            leaked.contains(&"upstream.rb".to_string()),
            "sanity: a moved-trunk base leaks upstream.rb (the bug), got {leaked:?}"
        );
    }

    /// A repo with no commits has no `main` to branch from. create_workspace must
    /// fail with an actionable hint (not jj's bare "Revision doesn't exist") and
    /// leave no half-created workspace behind.
    #[test]
    fn create_workspace_errors_helpfully_without_commits() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("empty");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--initial-branch=main"]);
        run_jj(&repo, &["git", "init", "--colocate"]).unwrap();

        let ws = tmp.path().join("ws-empty");
        let err = JjBackend.create_workspace(&repo, &ws, "wsempty", "main").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no commits"), "expected a no-commits hint, got: {msg}");
        assert!(!ws.exists(), "no workspace dir should be left behind, got one at {ws:?}");
    }

    /// The stale-recovery trigger: detection keys off jj's stderr phrase. Newer
    /// jj auto-recovers so the error can't be provoked end-to-end on every
    /// machine; this pins the exact message we match against (and rejects others).
    #[test]
    fn stderr_reports_stale_matches_jj_message() {
        assert!(stderr_reports_stale(
            "Error: The working copy is stale (not updated since operation cd3e17046956).\n\
             Hint: Run `jj workspace update-stale` to update it."
        ));
        assert!(!stderr_reports_stale("Error: Revision `main` doesn't exist"));
        assert!(!stderr_reports_stale(""));
    }

    /// Colocated jj repo with a few operations, so there's an older op to fork
    /// the log at when simulating a concurrent writer.
    fn setup_colocated(tmp: &Path) -> std::path::PathBuf {
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "--initial-branch=main"]);
        git(&repo, &["config", "user.email", "t@t.com"]);
        git(&repo, &["config", "user.name", "T"]);
        std::fs::write(repo.join("f.txt"), "a").unwrap();
        git(&repo, &["add", "."]);
        git(&repo, &["commit", "-m", "init"]);
        run_jj(&repo, &["git", "init", "--colocate"]).unwrap();
        run_jj(&repo, &["describe", "-m", "A"]).unwrap();
        run_jj(&repo, &["new", "-m", "B"]).unwrap();
        repo
    }

    /// Fork the operation log at an older op and let jj auto-reconcile the two
    /// heads — the exact sequence a second jj process racing us produces, and
    /// what leaves a persistent divergent change behind.
    fn force_op_divergence(repo: &Path) {
        let ops = run_jj_utf8(repo, &["op", "log", "--no-graph", "-T", "id ++ \"\\n\""]).unwrap();
        let earlier = ops.lines().nth(2).expect("an older op to fork at").to_string();
        run_jj(repo, &["--at-operation", &earlier, "describe", "-m", "FORKED"]).unwrap();
        let _ = run_jj(repo, &["status"]); // triggers the auto-reconcile
    }

    /// workon reads divergence via `vcs_runner::jj_divergent_change_ids` (in
    /// `active_status`, during `workon list`). Its load-bearing property is that
    /// the read is working-copy-agnostic: it must detect a reconciled op-log
    /// fork AND never snapshot the user's in-progress edits (which would move
    /// `@` under them). Pinning both against real jj means a dependency
    /// regression — e.g. the pre-0.15 helper that shelled out without
    /// `--ignore-working-copy` — can't silently reintroduce the snapshot.
    #[test]
    fn divergence_read_detects_fork_without_snapshotting_wip() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = setup_colocated(tmp.path());

        assert!(
            vcs_runner::jj_divergent_change_ids(&repo).unwrap().is_empty(),
            "a clean repo reports no divergence"
        );

        force_op_divergence(&repo);
        assert!(
            !vcs_runner::jj_divergent_change_ids(&repo).unwrap().is_empty(),
            "a reconciled op-log fork must surface as divergent"
        );

        // Dirty the working copy, then read `@` (itself working-copy-agnostic so
        // the read doesn't snapshot) before and after the divergence check.
        std::fs::write(repo.join("f.txt"), "uncommitted WIP").unwrap();
        let at = || {
            run_jj_utf8(&repo, &["log", "--ignore-working-copy", "--no-graph", "-r", "@", "-T", "commit_id", "--limit", "1"])
                .unwrap()
                .trim()
                .to_string()
        };
        let before = at();
        let _ = vcs_runner::jj_divergent_change_ids(&repo);
        assert_eq!(before, at(), "the divergence read must not snapshot the working copy (@ must not move)");
    }
}
