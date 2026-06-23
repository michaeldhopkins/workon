use std::path::Path;

use anyhow::{Context, Result};
use vcs_runner::{run_git, run_git_utf8};

use super::{detect_git_remote, path_str, Vcs};

pub struct GitBackend;

impl GitBackend {
    /// Whether the worktree's HEAD has commits ahead of the pinned `base` that no
    /// branch or remote ref contains — i.e. committed work teardown would orphan.
    fn has_unnamed_commits(&self, base: &str, ws_dir: &Path) -> bool {
        let ahead = run_git_utf8(ws_dir, &["rev-list", &format!("{base}..HEAD")])
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false);
        if !ahead {
            return false;
        }
        // If any local branch or remote ref already contains HEAD, the work is
        // saved (named or pushed) and shouldn't prompt.
        let contained = run_git_utf8(
            ws_dir,
            &["for-each-ref", "--contains", "HEAD", "--format=%(refname)", "refs/heads", "refs/remotes"],
        )
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
        !contained
    }
}

impl Vcs for GitBackend {
    fn detect_trunk(&self, project_dir: &Path) -> Result<String> {
        let remote = detect_git_remote(project_dir);
        let ok = run_git(project_dir, &["rev-parse", "--verify", &format!("{remote}/master")]).is_ok();

        Ok(if ok { "master".into() } else { "main".into() })
    }

    fn create_workspace(&self, project_dir: &Path, ws_dir: &Path, ws_id: &str, trunk: &str) -> Result<String> {
        eprintln!("Creating git worktree {ws_id}...");
        let remote = detect_git_remote(project_dir);
        // Pin the branch point to a SHA so teardown diffs against exactly where
        // the worktree started, even if the remote ref advances mid-session.
        let base = run_git_utf8(project_dir, &["rev-parse", &format!("{remote}/{trunk}")])
            .map(|s| s.trim().to_string())
            .with_context(|| {
                format!("trunk ref `{remote}/{trunk}` doesn't resolve — the repo may have no commits on {trunk} yet")
            })?;
        run_git(
            project_dir,
            &["worktree", "add", "--detach", &path_str(ws_dir), &base],
        )
        .context("failed to create git worktree")?;
        Ok(base)
    }

    fn pre_copy_sync(&self, _project_dir: &Path) {
        // git worktrees have their own index; no sync needed.
    }

    fn changed_files(&self, _ws_id: &str, base: &str, _project_dir: &Path, ws_dir: &Path) -> Vec<String> {
        // Two sources of would-vanish work in a worktree (jj only has the
        // second, since it auto-snapshots): a dirty tree, and commits on the
        // detached HEAD that no branch or remote names.
        let mut files: Vec<String> = Vec::new();

        // 1. Uncommitted changes. Use run_git (not run_git_utf8) — we must NOT
        // trim: `git status --porcelain` emits " M path" for modified-unstaged
        // files; trimming the leading space corrupts line.get(3..). Generated
        // files are already dropped via .git/info/exclude.
        if let Ok(out) = run_git(ws_dir, &["status", "--porcelain"]) {
            files.extend(out.stdout_lossy().lines().filter_map(|line| line.get(3..).map(|p| p.to_string())));
        }

        // 2. Committed work not reachable from any branch or remote — lost when
        // the worktree is removed (reflog only). Diff against the pinned base.
        if self.has_unnamed_commits(base, ws_dir)
            && let Ok(out) = run_git(ws_dir, &["diff", "--name-only", &format!("{base}...HEAD")])
        {
            files.extend(out.stdout_lossy().lines().map(str::to_string));
        }

        files.sort();
        files.dedup();
        files
    }

    fn save_work(&self, ws_id: &str, _base: &str, project_dir: &Path, ws_dir: &Path) -> Result<()> {
        // Commit any uncommitted changes; a clean tree just means the work is
        // already committed on the detached HEAD (still unnamed until we branch).
        let _ = run_git(ws_dir, &["add", "-A"]);
        let _ = run_git(ws_dir, &["commit", "-m", &format!("wip: workon/{ws_id}")]);

        let hash = run_git_utf8(ws_dir, &["rev-parse", "HEAD"]).context("failed to get commit hash")?;

        run_git(project_dir, &["branch", &format!("workon/{ws_id}"), &hash])
            .context("failed to create branch")?;

        eprintln!("Saved as branch workon/{ws_id}");
        Ok(())
    }

    fn stranded_work(&self, _ws_id: &str, base: &str, _project_dir: &Path, ws_dir: &Path) -> Vec<String> {
        // git's analog of jj's stranded sibling: commit on the detached HEAD,
        // then move HEAD away (e.g. `git switch -d origin/master`). The commit is
        // now reflog-only — invisible to changed_files (HEAD is no longer ahead).
        // The per-worktree HEAD reflog is the attribution source (concurrency-
        // proof: it's this worktree's own pointer history).
        let Ok(reflog) = run_git_utf8(ws_dir, &["reflog", "show", "--format=%H"]) else {
            return Vec::new();
        };
        let mut seen = std::collections::HashSet::new();
        let mut stranded = Vec::new();
        for sha in reflog.lines().map(str::trim).filter(|s| !s.is_empty()) {
            if !seen.insert(sha) {
                continue;
            }
            // Still stranded? Ahead of base, not on the current stack, on no ref.
            let ahead = run_git_utf8(ws_dir, &["rev-list", "-n", "1", &format!("{base}..{sha}")])
                .is_ok_and(|s| !s.trim().is_empty());
            let on_stack = run_git(ws_dir, &["merge-base", "--is-ancestor", sha, "HEAD"]).is_ok();
            let on_ref = run_git_utf8(
                ws_dir,
                &["for-each-ref", "--contains", sha, "--format=%(refname)", "refs/heads", "refs/remotes"],
            )
            .is_ok_and(|s| !s.trim().is_empty());
            if ahead && !on_stack && !on_ref
                && let Ok(desc) = run_git_utf8(ws_dir, &["log", "-1", "--format=%h  %s", sha])
            {
                stranded.push(desc.trim().to_string());
            }
        }
        stranded
    }

    fn save_stranded(&self, project_dir: &Path, ws_id: &str, commit_id: &str) -> Result<()> {
        let name = format!("workon/{ws_id}-{commit_id}");
        run_git(project_dir, &["branch", &name, commit_id])
            .context("failed to branch stranded commit")?;
        eprintln!("Saved stranded commit {commit_id} as branch {name}");
        Ok(())
    }

    fn forget_workspace(&self, _ws_id: &str, project_dir: &Path, ws_dir: &Path) {
        let _ = run_git(project_dir, &["worktree", "remove", "--force", &path_str(ws_dir)]);
        eprintln!("Removed git worktree");
    }

    fn ignore_generated_file(&self, project_dir: &Path, _ws_dir: &Path, relpath: &str) {
        if let Err(e) = super::append_git_exclude(project_dir, relpath) {
            eprintln!("Warning: could not exclude {relpath} from git: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use std::process::{Command, Stdio};

    use super::*;

    /// Pinned base SHA the worktree branches from, as `create_workspace` returns.
    fn base_of(dir: &Path, reff: &str) -> String {
        run_git_utf8(dir, &["rev-parse", reff]).unwrap().trim().to_string()
    }

    fn init_repo_with_remote(tmp: &Path) -> (std::path::PathBuf, std::path::PathBuf) {
        let origin = tmp.join("origin.git");
        let repo = tmp.join("repo");

        Command::new("git").args(["init", "--bare", "--initial-branch=main", &path_str(&origin)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        Command::new("git").args(["clone", &path_str(&origin), &path_str(&repo)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        Command::new("git").args(["-C", &path_str(&repo), "config", "user.email", "test@test.com"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "config", "user.name", "Test"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        std::fs::write(repo.join("README.md"), "hello").unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "add", "."])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "commit", "-m", "init"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "push"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        (origin, repo)
    }

    fn init_repo_with_named_remote(tmp: &Path, remote_name: &str, branch: &str) -> (std::path::PathBuf, std::path::PathBuf) {
        let origin = tmp.join("origin.git");
        let repo = tmp.join("repo");

        Command::new("git").args(["init", "--bare", &format!("--initial-branch={branch}"), &path_str(&origin)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        Command::new("git").args(["clone", "-o", remote_name, &path_str(&origin), &path_str(&repo)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        Command::new("git").args(["-C", &path_str(&repo), "config", "user.email", "test@test.com"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "config", "user.name", "Test"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        std::fs::write(repo.join("README.md"), "hello").unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "add", "."])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "commit", "-m", "init"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "push"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        (origin, repo)
    }

    /// The heroku-remote bug: `git remote` lists alphabetically, so a deploy
    /// mirror like `heroku_test` sorts ahead of `origin`. detect_git_remote must
    /// still pick `origin` when it exists, not the first-listed remote.
    #[test]
    fn detect_git_remote_prefers_origin_over_alphabetically_first() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        Command::new("git").args(["init", &path_str(&repo)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "remote", "add", "heroku_test", "https://git.heroku.com/x.git"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "remote", "add", "origin", "git@github.com:o/r.git"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        assert_eq!(detect_git_remote(&repo), "origin");
    }

    #[test]
    fn detect_git_remote_falls_back_to_sole_remote_when_no_origin() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        Command::new("git").args(["init", &path_str(&repo)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
        Command::new("git").args(["-C", &path_str(&repo), "remote", "add", "heroku", "https://git.heroku.com/x.git"])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        assert_eq!(detect_git_remote(&repo), "heroku");
    }

    #[test]
    fn detect_trunk_with_non_origin_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_named_remote(tmp.path(), "heroku", "master");

        let backend = GitBackend;
        let trunk = backend.detect_trunk(&repo).unwrap();
        assert_eq!(trunk, "master");
    }

    #[test]
    fn create_worktree_with_non_origin_remote() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_named_remote(tmp.path(), "heroku", "master");
        let ws_dir = tmp.path().join("worktree");

        let backend = GitBackend;
        backend.create_workspace(&repo, &ws_dir, "ws-test", "master").unwrap();
        assert!(ws_dir.join("README.md").exists());

        backend.forget_workspace("ws-test", &repo, &ws_dir);
    }

    /// The git analog of the jj no-commits guard: when the trunk ref can't be
    /// resolved (here: no commits, no remote), create_workspace fails up front —
    /// before `git worktree add` — and leaves no worktree directory behind.
    #[test]
    fn create_workspace_errors_without_resolvable_trunk() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        Command::new("git")
            .args(["init", "--initial-branch=main", &path_str(&repo)])
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();

        let ws = tmp.path().join("ws");
        let err = GitBackend.create_workspace(&repo, &ws, "ws-x", "main").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("doesn't resolve"), "expected an unresolved-trunk error, got: {msg}");
        assert!(!ws.exists(), "no worktree dir should be left behind, got one at {ws:?}");
    }

    #[test]
    fn changed_files_clean_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());

        let backend = GitBackend;
        let base = base_of(&repo, "origin/main");
        assert!(backend.changed_files("ws-test", &base, &repo, &repo).is_empty());
    }

    #[test]
    fn changed_files_dirty_repo() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());

        std::fs::write(repo.join("new_file.txt"), "dirty").unwrap();

        let backend = GitBackend;
        let base = base_of(&repo, "origin/main");
        let files = backend.changed_files("ws-test", &base, &repo, &repo);
        assert_eq!(files, vec!["new_file.txt"]);
    }

    /// Regression: `git status --porcelain` emits " M path" (leading space) for
    /// modified-unstaged files. Earlier versions trimmed the whole stdout, eating
    /// the leading space and corrupting line.get(3..).
    #[test]
    fn changed_files_modified_unstaged_preserves_leading_space() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());

        // README.md was committed by init_repo_with_remote — modify it without staging.
        std::fs::write(repo.join("README.md"), "modified content").unwrap();

        let backend = GitBackend;
        let base = base_of(&repo, "origin/main");
        let files = backend.changed_files("ws-test", &base, &repo, &repo);
        assert_eq!(files, vec!["README.md"]);
    }

    #[test]
    fn create_and_forget_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());
        let ws_dir = tmp.path().join("worktree");

        let backend = GitBackend;
        backend.create_workspace(&repo, &ws_dir, "ws-test", "main").unwrap();

        assert!(ws_dir.join("README.md").exists());

        backend.forget_workspace("ws-test", &repo, &ws_dir);
        assert!(!ws_dir.exists());
    }

    #[test]
    fn ignore_generated_file_excludes_from_git_status() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());

        std::fs::write(repo.join(".env.test.local"), "DATABASE_URL=postgresql://localhost/x").unwrap();
        // Before excluding, the generated file shows up as a change.
        let backend = GitBackend;
        let base = base_of(&repo, "origin/main");
        assert_eq!(backend.changed_files("ws-test", &base, &repo, &repo), vec![".env.test.local"]);

        backend.ignore_generated_file(&repo, &repo, ".env.test.local");

        assert!(
            backend.changed_files("ws-test", &base, &repo, &repo).is_empty(),
            "excluded file should not surface in git status"
        );
    }

    #[test]
    fn ignore_generated_file_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());

        let backend = GitBackend;
        backend.ignore_generated_file(&repo, &repo, ".env.test.local");
        backend.ignore_generated_file(&repo, &repo, ".env.test.local");

        let exclude = std::fs::read_to_string(repo.join(".git/info/exclude")).unwrap();
        let occurrences = exclude.lines().filter(|l| l.trim() == ".env.test.local").count();
        assert_eq!(occurrences, 1, "pattern should be written exactly once");
    }

    #[test]
    fn save_work_creates_branch() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());
        let ws_dir = tmp.path().join("worktree");

        let backend = GitBackend;
        backend.create_workspace(&repo, &ws_dir, "ws-abc123", "main").unwrap();

        std::fs::write(ws_dir.join("work.txt"), "important work").unwrap();

        let base = base_of(&repo, "origin/main");
        backend.save_work("ws-abc123", &base, &repo, &ws_dir).unwrap();

        let output = Command::new("git")
            .args(["-C", &path_str(&repo), "branch", "--list", "workon/ws-abc123"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .output()
            .unwrap();
        let branches = String::from_utf8_lossy(&output.stdout);
        assert!(branches.contains("workon/ws-abc123"), "branch should exist in main repo");

        backend.forget_workspace("ws-abc123", &repo, &ws_dir);
    }

    fn git_c(dir: &Path, args: &[&str]) {
        let dir_path = path_str(dir);
        let mut full = vec!["-C", &*dir_path];
        full.extend_from_slice(args);
        Command::new("git").args(&full)
            .stdout(Stdio::null()).stderr(Stdio::null()).status().unwrap();
    }

    /// Regression for the git analog of the jj bug: an agent that commits in a
    /// worktree leaves a clean tree, so the old `git status` check saw nothing
    /// and `git worktree remove --force` would orphan the commit.
    #[test]
    fn changed_files_detects_unnamed_committed_work() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());
        let ws = tmp.path().join("wt");
        let backend = GitBackend;
        backend.create_workspace(&repo, &ws, "ws-x", "main").unwrap();

        let base = base_of(&repo, "origin/main");
        std::fs::write(ws.join("feature.txt"), "work").unwrap();
        git_c(&ws, &["add", "."]);
        git_c(&ws, &["commit", "-m", "feature work"]);

        let changed = backend.changed_files("ws-x", &base, &repo, &ws);
        assert!(changed.contains(&"feature.txt".to_string()), "unnamed committed work should be detected, got {changed:?}");

        backend.forget_workspace("ws-x", &repo, &ws);
    }

    /// Branch-aware: committed work that's been put on a branch is saved, so it
    /// must not prompt — the git analog of jj's bookmark-aware silence.
    #[test]
    fn changed_files_silent_for_branched_work() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());
        let ws = tmp.path().join("wt");
        let backend = GitBackend;
        backend.create_workspace(&repo, &ws, "ws-x", "main").unwrap();

        let base = base_of(&repo, "origin/main");
        std::fs::write(ws.join("feature.txt"), "work").unwrap();
        git_c(&ws, &["add", "."]);
        git_c(&ws, &["commit", "-m", "feature work"]);
        git_c(&ws, &["checkout", "-b", "feature"]);

        assert!(backend.changed_files("ws-x", &base, &repo, &ws).is_empty(), "work on a branch is saved");

        backend.forget_workspace("ws-x", &repo, &ws);
    }

    /// save_work must handle a clean tree (work already committed) without
    /// failing on "nothing to commit", and still branch the committed work.
    #[test]
    fn save_work_branches_already_committed_work() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());
        let ws = tmp.path().join("wt");
        let backend = GitBackend;
        backend.create_workspace(&repo, &ws, "ws-y", "main").unwrap();

        std::fs::write(ws.join("feature.txt"), "work").unwrap();
        git_c(&ws, &["add", "."]);
        git_c(&ws, &["commit", "-m", "feature work"]);
        let head = run_git_utf8(&ws, &["rev-parse", "HEAD"]).unwrap();

        let base = base_of(&repo, "origin/main");
        backend.save_work("ws-y", &base, &repo, &ws).unwrap();

        let branch_sha = run_git_utf8(&repo, &["rev-parse", "workon/ws-y"]).unwrap();
        assert_eq!(branch_sha.trim(), head.trim(), "branch should point at the committed work");

        backend.forget_workspace("ws-y", &repo, &ws);
    }

    /// Commit work on a worktree's detached HEAD, then move HEAD off it — the
    /// commit is now reflog-only. changed_files can't see it (HEAD isn't ahead);
    /// stranded_work recovers it from the per-worktree reflog.
    fn strand_via_reflog(ws: &Path, content: &str, msg: &str) {
        std::fs::write(ws.join("orphan.txt"), content).unwrap();
        git_c(ws, &["add", "."]);
        git_c(ws, &["commit", "-m", msg]);
        git_c(ws, &["checkout", "--detach", "origin/main"]); // HEAD no longer ahead -> orphan
    }

    #[test]
    fn stranded_work_detects_and_excludes_branched() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());
        let ws = tmp.path().join("wt");
        let backend = GitBackend;
        backend.create_workspace(&repo, &ws, "ws-r", "main").unwrap();

        let base = base_of(&repo, "origin/main");
        strand_via_reflog(&ws, "lost work", "stranded feature");
        let s = backend.stranded_work("ws-r", &base, &repo, &ws);
        assert_eq!(s.len(), 1, "the reflog orphan should be detected, got {s:?}");
        assert!(s[0].contains("stranded feature"), "got {s:?}");

        // Once it's on a branch, it's saved — no longer stranded.
        let id = s[0].split_whitespace().next().unwrap().to_string();
        git_c(&repo, &["branch", "rescued", &id]);
        assert!(
            backend.stranded_work("ws-r", &base, &repo, &ws).is_empty(),
            "a branched commit is saved, not stranded"
        );

        backend.forget_workspace("ws-r", &repo, &ws);
    }

    /// No double-counting: a commit still on the current HEAD stack (HEAD ahead,
    /// unbranched) is handled by `changed_files`, so `stranded_work` must leave
    /// it alone — otherwise teardown would branch the same work twice.
    #[test]
    fn stranded_work_excludes_current_stack() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());
        let ws = tmp.path().join("wt");
        let backend = GitBackend;
        backend.create_workspace(&repo, &ws, "ws-c", "main").unwrap();

        let base = base_of(&repo, "origin/main");
        // Commit and stay on it (HEAD ahead, unbranched) — do NOT switch away.
        std::fs::write(ws.join("feature.txt"), "work").unwrap();
        git_c(&ws, &["add", "."]);
        git_c(&ws, &["commit", "-m", "in-progress work"]);

        assert!(
            !backend.changed_files("ws-c", &base, &repo, &ws).is_empty(),
            "current-stack work is handled by changed_files"
        );
        assert!(
            backend.stranded_work("ws-c", &base, &repo, &ws).is_empty(),
            "stranded_work must not also claim the current stack (would double-branch)"
        );

        backend.forget_workspace("ws-c", &repo, &ws);
    }

    #[test]
    fn save_stranded_branches_the_commit() {
        let tmp = tempfile::tempdir().unwrap();
        let (_origin, repo) = init_repo_with_remote(tmp.path());
        let ws = tmp.path().join("wt");
        let backend = GitBackend;
        backend.create_workspace(&repo, &ws, "ws-s", "main").unwrap();

        let base = base_of(&repo, "origin/main");
        strand_via_reflog(&ws, "lost work", "stranded feature");
        let s = backend.stranded_work("ws-s", &base, &repo, &ws);
        let id = s[0].split_whitespace().next().unwrap().to_string();

        backend.save_stranded(&repo, "ws-s", &id).unwrap();
        let branches = run_git_utf8(&repo, &["branch", "--list", &format!("workon/ws-s-{id}")]).unwrap();
        assert!(branches.contains("workon/ws-s-"), "branch should exist, got {branches:?}");

        backend.forget_workspace("ws-s", &repo, &ws);
    }
}
