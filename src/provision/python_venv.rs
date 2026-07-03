//! Python virtualenv repair. A copied `.venv` is broken: `bin/activate` and
//! every console-script shebang hardcode the *old* absolute path, and editable
//! installs (`pip install -e .`) leave `.pth` / `__editable__…` / `*.egg-link`
//! artifacts pointing at the old *source* dir. Since the venv lives at
//! `<root>/.venv`, rewriting the old project root -> the new worktree fixes both
//! in one pass — no reinstall, no network. See specs/workspace-provisioners.md.
//!
//! This is a path-repair provisioner: it creates no external resource and
//! returns an empty `Setup`.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{ProvisionCtx, Provisioner, Setup};

pub struct PythonVenv;

impl Provisioner for PythonVenv {
    fn name(&self) -> &'static str {
        "python-venv"
    }

    fn detect(&self, ws_dir: &Path) -> bool {
        ws_dir.join(".venv/pyvenv.cfg").is_file()
    }

    fn setup(&self, ctx: &ProvisionCtx<'_>) -> Result<Setup> {
        let venv = ctx.ws_dir.join(".venv");
        let new_root = ctx.ws_dir.to_string_lossy().into_owned();
        let old_roots: Vec<String> = old_roots(&venv).into_iter().filter(|r| *r != new_root).collect();
        if old_roots.is_empty() {
            // Already correct — a relocatable venv, or nothing to read.
            return Ok(Setup::default());
        }
        eprintln!("Repairing copied .venv paths...");
        let changed = repair(&venv, &old_roots, &new_root);
        eprintln!("Rewrote {changed} venv path reference(s)");
        Ok(Setup::default())
    }
}

/// The project root(s) the venv was created under, as they appear baked into the
/// venv. There can be two forms of the same location: `bin/activate`'s
/// `VIRTUAL_ENV` uses `abspath` (symlinks kept) while console-script shebangs use
/// `realpath` (symlinks resolved), so both must be rewritten. The venv lives at
/// `<root>/.venv` and editable artifacts reference `<root>`, so rewriting each
/// `<root>` covers activation scripts, shebangs, and editable installs alike.
fn old_roots(venv: &Path) -> Vec<String> {
    let mut roots = Vec::new();
    if let Some(r) = activate_root(venv) {
        roots.push(r);
    }
    if let Some(r) = shebang_root(venv) {
        roots.push(r);
    }
    roots.sort();
    roots.dedup();
    roots
}

/// `<root>` from `bin/activate`'s `VIRTUAL_ENV="<root>/.venv"` (only if it's an
/// absolute-path literal — a relocatable venv computes it dynamically).
fn activate_root(venv: &Path) -> Option<String> {
    let activate = std::fs::read_to_string(venv.join("bin/activate")).ok()?;
    let value = activate.lines().find_map(|l| {
        let v = l.trim_start().strip_prefix("VIRTUAL_ENV=")?.trim().trim_matches(|c| c == '"' || c == '\'');
        v.starts_with('/').then(|| v.to_string())
    })?;
    Path::new(&value).parent().map(|p| p.to_string_lossy().into_owned())
}

/// `<root>` from a console script's shebang (`#!<root>/.venv/bin/python…`) — the
/// literal, realpath-form path every bin script carries.
fn shebang_root(venv: &Path) -> Option<String> {
    for e in std::fs::read_dir(venv.join("bin")).ok()?.flatten() {
        if !e.file_type().map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(e.path()) else {
            continue;
        };
        let Some(first) = content.lines().next() else {
            continue;
        };
        if let Some(interp) = first.strip_prefix("#!")
            && let Some((root, _)) = interp.trim().split_once("/.venv/")
            && root.starts_with('/')
        {
            return Some(root.to_string());
        }
    }
    None
}

/// Rewrite each `old` root -> `new` in the venv's activation scripts,
/// console-script shebangs, and editable-install artifacts. Returns the number
/// of files changed.
fn repair(venv: &Path, olds: &[String], new: &str) -> usize {
    let mut files: Vec<PathBuf> = Vec::new();

    // bin/: activate* and every console script. Skip the `python` symlink (it
    // points at the unmoved system interpreter) and anything non-regular.
    if let Ok(entries) = std::fs::read_dir(venv.join("bin")) {
        for e in entries.flatten() {
            if e.file_type().map(|t| t.is_file()).unwrap_or(false) {
                files.push(e.path());
            }
        }
    }

    // site-packages editable-install artifacts, which point at the old source.
    for sp in site_packages(venv) {
        if let Ok(entries) = std::fs::read_dir(&sp) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().into_owned();
                if name.ends_with(".pth")
                    || name.ends_with(".egg-link")
                    || (name.starts_with("__editable__") && name.ends_with(".py"))
                {
                    files.push(e.path());
                }
            }
        }
    }

    files.iter().filter(|f| rewrite_file(f, olds, new)).count()
}

fn site_packages(venv: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Ok(libs) = std::fs::read_dir(venv.join("lib")) {
        for lib in libs.flatten() {
            let sp = lib.path().join("site-packages");
            if sp.is_dir() {
                out.push(sp);
            }
        }
    }
    out
}

/// Replace every occurrence of each `old` with `new` in a UTF-8 text file,
/// preserving its mode (`fs::write` on an existing file keeps permissions, so the
/// exec bit on console scripts survives). Skips binaries (non-UTF-8) and files
/// that don't change. One read/write covers all old forms.
fn rewrite_file(path: &Path, olds: &[String], new: &str) -> bool {
    let Ok(content) = std::fs::read_to_string(path) else {
        return false;
    };
    let mut out = content.clone();
    for old in olds {
        out = out.replace(old, new);
    }
    if out == content {
        return false;
    }
    std::fs::write(path, out).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn activate_root_parses_virtual_env() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join(".venv/bin");
        std::fs::create_dir_all(&bin).unwrap();
        // `unset VIRTUAL_ENV` lines precede the assignment — must not be picked up.
        std::fs::write(
            bin.join("activate"),
            "    unset VIRTUAL_ENV\nVIRTUAL_ENV=\"/old/proj/.venv\"\nexport VIRTUAL_ENV\n",
        )
        .unwrap();
        assert_eq!(activate_root(&tmp.path().join(".venv")).as_deref(), Some("/old/proj"));
    }

    #[test]
    fn shebang_root_reads_the_realpath_form() {
        let tmp = tempfile::tempdir().unwrap();
        let bin = tmp.path().join(".venv/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::fs::write(bin.join("pip"), "#!/private/var/proj/.venv/bin/python3\nprint('x')\n").unwrap();
        assert_eq!(shebang_root(&tmp.path().join(".venv")).as_deref(), Some("/private/var/proj"));
    }

    #[test]
    fn rewrites_editable_artifacts_pointing_at_old_source() {
        // Pure file test — no Python needed. An editable finder + .pth that point
        // at the old source dir get rewritten to the new worktree.
        let tmp = tempfile::tempdir().unwrap();
        let sp = tmp.path().join(".venv/lib/python3.13/site-packages");
        std::fs::create_dir_all(&sp).unwrap();
        std::fs::create_dir_all(tmp.path().join(".venv/bin")).unwrap();
        std::fs::write(tmp.path().join(".venv/bin/activate"), "VIRTUAL_ENV=\"/old/proj/.venv\"\n").unwrap();
        std::fs::write(sp.join("__editable__.mypkg-0.0.0.pth"), "/old/proj/src\n").unwrap();
        std::fs::write(
            sp.join("__editable___mypkg_finder.py"),
            "MAPPING = {'mypkg': '/old/proj/src/mypkg'}\n",
        )
        .unwrap();

        let venv = tmp.path().join(".venv");
        let changed = repair(&venv, &["/old/proj".to_string()], &tmp.path().to_string_lossy());
        assert!(changed >= 2, "activate + 2 editable files rewritten, got {changed}");

        let pth = std::fs::read_to_string(sp.join("__editable__.mypkg-0.0.0.pth")).unwrap();
        assert!(pth.contains(&format!("{}/src", tmp.path().display())), "{pth}");
        assert!(!pth.contains("/old/proj"), "old path gone: {pth}");
    }

    /// End-to-end against a real venv: create one, copy it elsewhere, delete the
    /// original, repair the copy, and confirm a console script (pip) runs from
    /// the new path — which only works if the shebang was rewritten. Gated on
    /// `python3`; offline (venv bundles pip via ensurepip).
    #[test]
    fn repairs_a_real_copied_venv_so_pip_runs_from_new_path() {
        if !vcs_runner::binary_available("python3") {
            return;
        }
        let root = tempfile::tempdir().unwrap();
        let dir_a = root.path().join("projA");
        let dir_b = root.path().join("projB");
        std::fs::create_dir_all(&dir_a).unwrap();

        let venv_ok = Command::new("python3")
            .args(["-m", "venv", dir_a.join(".venv").to_str().unwrap()])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if !venv_ok {
            return; // venv creation unavailable (e.g. no ensurepip)
        }

        // Simulate workon's copy, then remove the original so the old shebang is
        // genuinely dead.
        assert!(Command::new("cp").args(["-R", dir_a.to_str().unwrap(), dir_b.to_str().unwrap()]).status().unwrap().success());
        std::fs::remove_dir_all(&dir_a).unwrap();

        // pip in the copy is broken until repaired (its shebang points at dir_a).
        let mise = std::collections::HashMap::new();
        let ctx = ProvisionCtx {
            project_dir: &dir_a,
            project_name: "projB",
            ws_id: "ws-venv",
            ws_dir: &dir_b,
            mise_vars: &mise,
        };
        PythonVenv.setup(&ctx).unwrap();

        let pip = dir_b.join(".venv/bin/pip");
        let ran = Command::new(&pip).arg("--version").status().map(|s| s.success()).unwrap_or(false);
        assert!(ran, "pip must run from the repaired venv path");
    }
}
