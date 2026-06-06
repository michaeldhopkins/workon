use std::path::Path;

use anyhow::{Context, Result};
use vcs_runner::{parse_diff_summary, run_git, run_git_utf8, run_jj, run_jj_utf8};

use super::{detect_git_remote, path_str, Vcs};

pub struct JjBackend;

impl JjBackend {
    /// Trunk revision for range queries, falling back to the `trunk()` revset
    /// if detection somehow fails. Mirrors the trunk the workspace was branched
    /// from, so `trunk()..ws@` is exactly the work done in the workspace.
    fn trunk_or_default(&self, project_dir: &Path) -> String {
        self.detect_trunk(project_dir).unwrap_or_else(|_| "trunk()".into())
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
        let Ok(out) = run_jj_utf8(project_dir, &["op", "log", "--op-diff", "--no-graph"]) else {
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

impl Vcs for JjBackend {
    fn detect_trunk(&self, project_dir: &Path) -> Result<String> {
        // trunk() works when the remote is named "origin"; fall back to
        // searching all remotes for repos with non-standard remote names.
        let revsets = [
            "trunk()",
            r#"latest(remote_bookmarks("master") | remote_bookmarks("main"))"#,
        ];
        for revset in &revsets {
            if let Ok(output) = run_jj_utf8(
                project_dir,
                &["log", "-r", revset, "--no-graph", "-T", "bookmarks", "--limit", "1"],
            ) {
                let bookmark = first_real_bookmark(&output);
                if !bookmark.is_empty() {
                    return Ok(bookmark.to_string());
                }
            }
        }

        Ok("main".into())
    }

    fn create_workspace(&self, project_dir: &Path, ws_dir: &Path, ws_id: &str, trunk: &str) -> Result<()> {
        eprintln!("Creating jj workspace {ws_id}...");
        run_jj(
            project_dir,
            &["workspace", "add", &path_str(ws_dir), "--name", ws_id, "-r", trunk],
        )
        .context("failed to create jj workspace")?;

        // jj workspaces don't have a .git directory, so git commands
        // (branchdiff, git log, etc.) fail inside the workspace. Set up a
        // git worktree reference so git works alongside jj.
        if let Err(e) = setup_git_worktree(project_dir, ws_dir, ws_id, trunk) {
            eprintln!("Warning: could not set up git worktree for workspace: {e}");
        }

        Ok(())
    }

    fn pre_copy_sync(&self, project_dir: &Path) {
        // Running any jj command triggers an automatic snapshot in modern jj,
        // which ensures the git index is in sync with jj's working copy so
        // that git ls-files --ignored returns accurate results.
        let _ = run_jj(project_dir, &["status"]);
    }

    fn changed_files(&self, ws_id: &str, project_dir: &Path, _ws_dir: &Path) -> Vec<String> {
        let trunk = self.trunk_or_default(project_dir);
        let ws_head = format!("{ws_id}@");
        // Is there non-empty work in the stack that no bookmark or remote names?
        // Excluding ancestors(bookmarks | remote_bookmarks) means a stack you've
        // already bookmarked or pushed reports nothing — no spurious prompt.
        // (The whole-stack range also catches work parked on an ancestor of ws@
        // by `jj commit`, which a point query at ws@ would miss.)
        let unsaved = format!(
            "({trunk}..{ws_head}) & ~empty() & ~ancestors(bookmarks() | remote_bookmarks())"
        );
        if !self.revset_nonempty(project_dir, &unsaved) {
            return Vec::new();
        }
        run_jj_utf8(
            project_dir,
            &["diff", "--ignore-working-copy", "--from", &trunk, "--to", &ws_head, "--summary"],
        )
        .map(|stdout| {
            parse_diff_summary(&stdout)
                .into_iter()
                .map(|c| c.path.to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default()
    }

    fn save_work(&self, ws_id: &str, project_dir: &Path, _ws_dir: &Path) -> Result<()> {
        let trunk = self.trunk_or_default(project_dir);
        // Bookmark the tip of the non-empty stack, not ws@ — ws@ is empty
        // whenever the agent committed its work, so bookmarking it would save
        // nothing. Fall back to ws@ if (defensively) the stack is all-empty.
        let ws_head = format!("{ws_id}@");
        let target = run_jj_utf8(
            project_dir,
            &["log", "--ignore-working-copy", "--no-graph", "-T", "commit_id ++ \"\\n\"",
              "-r", &format!("heads(({trunk}..{ws_head}) & ~empty())")],
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

    fn stranded_work(&self, ws_id: &str, project_dir: &Path, _ws_dir: &Path) -> Vec<String> {
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
fn setup_git_worktree(project_dir: &Path, ws_dir: &Path, ws_id: &str, trunk: &str) -> Result<()> {
    let git_dir = absolute_git_dir(project_dir)
        .context("could not determine .git directory")?;
    let wt_git_dir = format!("{git_dir}/worktrees/{ws_id}");

    std::fs::create_dir_all(&wt_git_dir)?;
    std::fs::write(format!("{wt_git_dir}/gitdir"), format!("{}/.git\n", path_str(ws_dir)))?;
    std::fs::write(format!("{wt_git_dir}/commondir"), "../..\n")?;

    let trunk_branch = trunk.split('@').next().unwrap_or(trunk);
    let remote = detect_git_remote(project_dir);

    let head_output = run_git_utf8(project_dir, &["rev-parse", &format!("{remote}/{trunk_branch}")])
        .ok()
        .filter(|s| !s.is_empty());

    let head = head_output.unwrap_or_else(|| {
        run_git_utf8(project_dir, &["rev-parse", "HEAD"]).unwrap_or_default()
    });

    std::fs::write(format!("{wt_git_dir}/HEAD"), format!("{head}\n"))?;

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

        setup_git_worktree(&project, &ws, "test-ws", "main").unwrap();

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
        std::fs::write(ws.join("app.rb"), "agent change").unwrap();
        run_jj(&ws, &["commit", "-m", "implement feature"]).unwrap();

        let changed = JjBackend.changed_files("wsa", &repo, &ws);
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
        std::fs::write(ws.join("app.rb"), "agent change").unwrap();
        run_jj(&ws, &["commit", "-m", "implement feature"]).unwrap();

        JjBackend.save_work("wsb", &repo, &ws).unwrap();

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
        std::fs::write(ws.join("app.rb"), "agent change").unwrap();
        run_jj(&ws, &["commit", "-m", "finish the PR"]).unwrap();
        run_jj(&ws, &["bookmark", "set", "my-feature", "-r", "@-"]).unwrap();

        assert!(
            JjBackend.changed_files("wse", &repo, &ws).is_empty(),
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

        let s1 = JjBackend.stranded_work("wsone", &repo, &ws1);
        let s2 = JjBackend.stranded_work("wstwo", &repo, &ws2);
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
        assert!(JjBackend.stranded_work("wsx", &repo, &ws).is_empty(), "clean workspace strands nothing");

        std::fs::write(ws.join("app.rb"), "edit").unwrap();
        run_jj(&ws, &["new", "master", "-m", "fresh"]).unwrap();
        let s = JjBackend.stranded_work("wsx", &repo, &ws);
        assert_eq!(s.len(), 1, "the jj-new orphan should be attributed, got {s:?}");

        let id = s[0].split_whitespace().next().unwrap().to_string();
        JjBackend.save_stranded(&repo, "wsx", &id).unwrap();
        assert!(
            JjBackend.stranded_work("wsx", &repo, &ws).is_empty(),
            "a saved (bookmarked) stranded commit is no longer stranded"
        );
    }
}
