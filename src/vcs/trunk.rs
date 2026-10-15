//! Repos with no remote: the local-bookmark trunk fallback, the `--base` override, and the
//! error for a trunk that does not resolve.

use std::path::Path;

use anyhow::{bail, Context, Result};
use vcs_runner::run_jj_utf8;

use super::jj::jj_trunk_bookmark;
use super::Vcs;

/// The error for a trunk revision that does not resolve. A repo that has commits is not
/// told it has none: it is told what was tried and how to name a base.
pub(crate) fn unresolved_trunk(trunk: &str, repo_has_commits: bool) -> String {
    if repo_has_commits {
        return format!(
            "trunk revision `{trunk}` doesn't resolve to a commit. Tried trunk(), main, master and \
             trunk on origin, master or main on the detected git remote and on any jj remote, and \
             local main and master bookmarks.\n\
             Name the revision to branch from instead: workon create --base REV \
             (for example --base 'default@-')."
        );
    }
    format!(
        "trunk revision `{trunk}` doesn't resolve to a commit — the repo \
         likely has no commits yet. Create one, e.g.\n    \
         jj describe -m \"Initial commit\" && jj bookmark create {trunk} -r @ && jj new\n\
         then re-run."
    )
}

/// Does the repo hold a commit beyond the root and the untouched working copy?
pub(crate) fn repo_has_commits(project_dir: &Path) -> bool {
    let revset = r#"~root() ~ working_copies() ~ (empty() & description(exact:""))"#;
    run_jj_utf8(
        project_dir,
        &["log", "--ignore-working-copy", "--no-graph", "-r", revset, "-T", "\"x\"", "--limit", "1"],
    )
    .is_ok_and(|out| !out.trim().is_empty())
}

/// A repo with no remote can still name its trunk with a local `main` or `master` bookmark.
pub(crate) fn local_trunk_bookmark(project_dir: &Path) -> Option<String> {
    ["main", "master"]
        .iter()
        .find(|name| jj_trunk_bookmark(project_dir, &format!(r#"bookmarks(exact:"{name}")"#)).is_some())
        .map(|name| (*name).to_string())
}

/// Resolve `--base REV` to the one commit it names. A working-copy commit is refused: the
/// workspace would sit on a commit that the owner's next snapshot rewrites, and teardown
/// would count their edits as the workspace's work.
pub(crate) fn resolve_single(project_dir: &Path, rev: &str) -> Result<String> {
    let is_working_copy = run_jj_utf8(
        project_dir,
        &["log", "--no-graph", "-r", &format!("({rev}) & working_copies()"), "-T", "\"x\"", "--limit", "1"],
    )
    .is_ok_and(|out| !out.trim().is_empty());
    if is_working_copy {
        bail!(
            "--base `{rev}` is a working copy, which keeps changing; name its parent instead (for example `({rev})-`)"
        );
    }
    let out = run_jj_utf8(project_dir, &["log", "--no-graph", "-r", rev, "-T", "commit_id ++ \"\\n\"", "--limit", "2"])
        .with_context(|| format!("--base `{rev}` doesn't resolve to a commit"))?;
    let mut ids = out.lines().map(str::trim).filter(|l| !l.is_empty());
    match (ids.next(), ids.next()) {
        (Some(id), None) => Ok(id.to_string()),
        (None, _) => bail!("--base `{rev}` matches no commit"),
        _ => bail!("--base `{rev}` matches more than one commit; name exactly one"),
    }
}

/// The backend with the branch point fixed to a commit the owner named, so
/// `detect_trunk` has nothing left to guess.
pub struct PinnedBase {
    inner: Box<dyn Vcs>,
    commit: String,
}

impl PinnedBase {
    pub fn new(inner: Box<dyn Vcs>, project_dir: &Path, rev: &str) -> Result<Self> {
        let commit = inner.resolve_base(project_dir, rev)?;
        Ok(Self { inner, commit })
    }
}

impl Vcs for PinnedBase {
    fn detect_trunk(&self, _project_dir: &Path) -> Result<String> {
        Ok(self.commit.clone())
    }
    fn resolve_base(&self, project_dir: &Path, rev: &str) -> Result<String> {
        self.inner.resolve_base(project_dir, rev)
    }
    fn create_workspace(&self, project_dir: &Path, ws_dir: &Path, ws_id: &str, trunk: &str) -> Result<String> {
        self.inner.create_workspace(project_dir, ws_dir, ws_id, trunk)
    }
    fn pre_copy_sync(&self, project_dir: &Path) {
        self.inner.pre_copy_sync(project_dir);
    }
    fn changed_files(&self, ws_id: &str, base: &str, project_dir: &Path, ws_dir: &Path) -> Vec<String> {
        self.inner.changed_files(ws_id, base, project_dir, ws_dir)
    }
    fn save_work(&self, ws_id: &str, base: &str, project_dir: &Path, ws_dir: &Path) -> Result<()> {
        self.inner.save_work(ws_id, base, project_dir, ws_dir)
    }
    fn forget_workspace(&self, ws_id: &str, project_dir: &Path, ws_dir: &Path) {
        self.inner.forget_workspace(ws_id, project_dir, ws_dir);
    }
    fn stranded_work(&self, ws_id: &str, base: &str, project_dir: &Path, ws_dir: &Path) -> Vec<String> {
        self.inner.stranded_work(ws_id, base, project_dir, ws_dir)
    }
    fn save_stranded(&self, project_dir: &Path, ws_id: &str, commit_id: &str) -> Result<()> {
        self.inner.save_stranded(project_dir, ws_id, commit_id)
    }
    fn ignore_generated_file(&self, project_dir: &Path, ws_dir: &Path, relpath: &str) {
        self.inner.ignore_generated_file(project_dir, ws_dir, relpath);
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    use proptest::prelude::*;

    use super::*;
    use crate::vcs::JjBackend;

    fn jj(repo: &Path, args: &[&str]) {
        let status = Command::new("jj")
            .args(args)
            .current_dir(repo)
            .env("JJ_USER", "T")
            .env("JJ_EMAIL", "t@t.com")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "jj {args:?}");
    }

    /// A colocated jj repo with two commits and neither a remote nor a bookmark.
    fn remoteless_repo(tmp: &Path) -> PathBuf {
        let repo = tmp.join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        jj(&repo, &["git", "init", "--colocate"]);
        std::fs::write(repo.join("app.rb"), "v1").unwrap();
        jj(&repo, &["commit", "-m", "first"]);
        std::fs::write(repo.join("app.rb"), "v2").unwrap();
        jj(&repo, &["commit", "-m", "second"]);
        repo
    }

    fn commit_id(repo: &Path, rev: &str) -> String {
        JjBackend.resolve_base(repo, rev).unwrap()
    }

    #[test]
    fn a_repo_with_commits_but_no_remote_or_bookmark_does_not_claim_to_be_empty() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = remoteless_repo(tmp.path());
        let trunk = JjBackend.detect_trunk(&repo).unwrap();
        let err = JjBackend.create_workspace(&repo, &tmp.path().join("ws"), "ws-a", &trunk).unwrap_err();
        let msg = format!("{err:#}");
        assert!(!msg.contains("no commits yet"), "{msg}");
        assert!(msg.contains("--base"), "{msg}");
    }

    #[test]
    fn an_empty_repo_still_gets_the_no_commits_hint() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir_all(&repo).unwrap();
        jj(&repo, &["git", "init", "--colocate"]);
        let err = JjBackend.create_workspace(&repo, &tmp.path().join("ws"), "ws-a", "main").unwrap_err();
        assert!(format!("{err:#}").contains("no commits yet"));
    }

    #[test]
    fn a_pinned_base_branches_from_the_named_commit() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = remoteless_repo(tmp.path());
        let parent = commit_id(&repo, "@-");
        let vcs = PinnedBase::new(Box::new(JjBackend), &repo, "@-").unwrap();
        let trunk = vcs.detect_trunk(&repo).unwrap();
        assert_eq!(trunk, parent);
        let ws = tmp.path().join("ws");
        let base = vcs.create_workspace(&repo, &ws, "ws-a", &trunk).unwrap();
        assert_eq!(base, parent);
        assert_eq!(std::fs::read_to_string(ws.join("app.rb")).unwrap(), "v2");
    }

    #[test]
    fn a_working_copy_is_refused_as_a_base_but_its_parent_is_not() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = remoteless_repo(tmp.path());
        for rev in ["@", "default@", "(@)"] {
            let err = JjBackend.resolve_base(&repo, rev).unwrap_err();
            assert!(err.to_string().contains("is a working copy"), "{rev}: {err:#}");
        }
        assert!(JjBackend.resolve_base(&repo, "default@-").is_ok());
    }

    #[test]
    fn the_local_trunk_is_the_name_asked_for_not_a_sibling_bookmark() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = remoteless_repo(tmp.path());
        jj(&repo, &["bookmark", "create", "alpha", "main", "-r", "@-"]);
        assert_eq!(local_trunk_bookmark(&repo), Some("main".to_string()));
    }

    #[test]
    fn the_git_backend_refuses_a_base_and_says_why() {
        let err = crate::vcs::GitBackend.resolve_base(Path::new("."), "main").unwrap_err();
        assert!(err.to_string().contains("--base needs jj installed"));
    }

    #[test]
    fn a_base_that_names_nothing_or_many_is_refused() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = remoteless_repo(tmp.path());
        let missing = JjBackend.resolve_base(&repo, "nope").unwrap_err();
        assert!(format!("{missing:#}").contains("--base `nope`"), "{missing:#}");
        let none = JjBackend.resolve_base(&repo, "none()").unwrap_err();
        assert!(format!("{none:#}").contains("matches no commit"), "{none:#}");
        let many = JjBackend.resolve_base(&repo, "@- | @--").unwrap_err();
        assert!(format!("{many:#}").contains("more than one"), "{many:#}");
    }

    #[test]
    fn a_local_main_or_master_bookmark_is_the_trunk_without_a_remote() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = remoteless_repo(tmp.path());
        assert_eq!(local_trunk_bookmark(&repo), None);
        jj(&repo, &["bookmark", "create", "master", "-r", "@-"]);
        assert_eq!(JjBackend.detect_trunk(&repo).unwrap(), "master");
        jj(&repo, &["bookmark", "create", "main", "-r", "@--"]);
        assert_eq!(JjBackend.detect_trunk(&repo).unwrap(), "main", "main outranks master");
    }

    #[test]
    fn has_commits_ignores_the_root_and_an_untouched_working_copy() {
        if !vcs_runner::jj_available() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let repo = remoteless_repo(tmp.path());
        assert!(repo_has_commits(&repo));
        let blank = tmp.path().join("blank");
        std::fs::create_dir_all(&blank).unwrap();
        jj(&blank, &["git", "init", "--colocate"]);
        assert!(!repo_has_commits(&blank));
    }

    proptest! {
        /// The error always names the trunk it tried; only a repo with commits is pointed at
        /// `--base`, and only an empty one is told it has none.
        #[test]
        fn the_unresolved_trunk_error_matches_what_the_repo_holds(trunk in "[a-z0-9._/]{1,20}", has in any::<bool>()) {
            let msg = unresolved_trunk(&trunk, has);
            let quoted = format!("`{trunk}`");
            prop_assert!(msg.contains(&quoted));
            prop_assert_eq!(msg.contains("--base"), has);
            prop_assert_eq!(msg.contains("no commits yet"), !has);
        }
    }
}
