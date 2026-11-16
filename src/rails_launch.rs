//! The environment a workspace session starts with.
//!
//! A Rails workspace refreshes the data-dir script and rebuilds `RUBYOPT` from
//! the caller's flags, mise, and the value stored at create. Any other workspace
//! drops a Rails hook the parent shell leaked, and the suffix that came with it.
//! A suffix with no hook is left alone.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};

use crate::provision::rails_suffix::{self, SUFFIX_VAR};

/// Env to set on the session, and names to remove so a parent shell cannot leak them in.
#[derive(Debug)]
pub struct Prepared {
    pub env: HashMap<String, String>,
    pub clear: Vec<String>,
}

/// Env for a session started in `dir` by plain `workon` or `workon run`.
///
/// A directory whose `.workon.json` records a Rails suffix refreshes the hook,
/// the same way attach does. Anywhere else, only a hook leaked by a parent Rails
/// session is removed. Mise's environment is not added to a plain session.
pub fn for_launch(dir: &Path) -> Result<Prepared> {
    let session = crate::workspace::read_meta(dir).session_env;
    if session.contains_key(SUFFIX_VAR) {
        return prepared(dir);
    }
    let process_rubyopt = std::env::var("RUBYOPT").ok();
    assemble(&HashMap::new(), &HashMap::new(), process_rubyopt.as_deref(), None)
}

/// Mise env plus `.workon.json`, with the Rails hook refreshed or stripped.
pub(crate) fn prepared(ws_dir: &Path) -> Result<Prepared> {
    let mise = crate::mise_env::mise_env("mise", ws_dir);
    let session = crate::workspace::read_meta(ws_dir).session_env;
    let process_rubyopt = std::env::var("RUBYOPT").ok();
    let script = if session.contains_key(SUFFIX_VAR) {
        Some(
            rails_suffix::install()
                .context("not starting the session: the Rails database hook could not be refreshed")?,
        )
    } else {
        None
    };
    assemble(&mise, &session, process_rubyopt.as_deref(), script.as_deref())
}

pub(crate) fn assemble(
    mise: &HashMap<String, String>,
    session: &HashMap<String, String>,
    process_rubyopt: Option<&str>,
    script: Option<&Path>,
) -> Result<Prepared> {
    let mut env = mise.clone();
    for (key, value) in session {
        if key != "RUBYOPT" && key != SUFFIX_VAR {
            env.insert(key.clone(), value.clone());
        }
    }
    if let Some(suffix) = session.get(SUFFIX_VAR) {
        let Some(script) = script else {
            bail!("not starting the session: the Rails database hook could not be refreshed");
        };
        let sources =
            [process_rubyopt, mise.get("RUBYOPT").map(String::as_str), session.get("RUBYOPT").map(String::as_str)];
        let rubyopt = rails_suffix::compose_rubyopt(sources.into_iter().flatten(), script)?;
        env.insert(SUFFIX_VAR.to_string(), suffix.clone());
        env.insert("RUBYOPT".to_string(), rubyopt);
        return Ok(Prepared { env, clear: Vec::new() });
    }
    let mut clear = Vec::new();
    strip_leaked_hook(&mut env, &mut clear, process_rubyopt);
    Ok(Prepared { env, clear })
}

/// A hook in the `RUBYOPT` the child would see is removed. The suffix is removed
/// with it, unless mise or the workspace set that suffix itself.
fn strip_leaked_hook(env: &mut HashMap<String, String>, clear: &mut Vec<String>, process_rubyopt: Option<&str>) {
    let mise_rubyopt = env.get("RUBYOPT").cloned();
    let inherited = mise_rubyopt.as_deref().or(process_rubyopt);
    let process_leaks = process_rubyopt.is_some_and(rails_suffix::loads_hook);
    if let Some(value) = inherited.filter(|value| rails_suffix::loads_hook(value)) {
        let stripped = rails_suffix::without_hook(value);
        if stripped.is_empty() {
            env.remove("RUBYOPT");
            clear.push("RUBYOPT".to_string());
        } else {
            env.insert("RUBYOPT".to_string(), stripped);
        }
    }
    if (process_leaks || inherited.is_some_and(rails_suffix::loads_hook)) && !env.contains_key(SUFFIX_VAR) {
        clear.push(SUFFIX_VAR.to_string());
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::Path;

    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(key, value)| ((*key).to_string(), (*value).to_string())).collect()
    }

    #[test]
    fn a_rails_session_merges_rubyopt_and_drops_the_old_hook() {
        let script = Path::new("/tmp/workon/rails_db_suffix.rb");
        let session = map(&[
            (SUFFIX_VAR, "_ws_abc"),
            ("RUBYOPT", "-r/old/rails_db_suffix.rb"),
            ("MIX_TEST_PARTITION", "_ws_abc"),
        ]);
        let ready = assemble(&map(&[("RUBYOPT", "--yjit")]), &session, Some("-W0"), Some(script)).unwrap();
        let rubyopt = ready.env.get("RUBYOPT").unwrap();
        assert!(rubyopt.starts_with("-r/tmp/workon/rails_db_suffix.rb"), "{rubyopt}");
        assert!(rubyopt.split_whitespace().any(|token| token == "-W0"), "{rubyopt}");
        assert!(rubyopt.split_whitespace().any(|token| token == "--yjit"), "{rubyopt}");
        assert!(!rubyopt.contains("/old/"), "{rubyopt}");
        assert_eq!(ready.env.get(SUFFIX_VAR).map(String::as_str), Some("_ws_abc"));
        assert_eq!(ready.env.get("MIX_TEST_PARTITION").map(String::as_str), Some("_ws_abc"));
        assert!(ready.clear.is_empty());
    }

    #[test]
    fn a_missing_script_does_not_export_the_suffix() {
        let session = map(&[(SUFFIX_VAR, "_ws_abc")]);
        let err = assemble(&HashMap::new(), &session, Some("-W0"), None).unwrap_err();
        assert!(err.to_string().contains("not starting the session"), "{err}");
    }

    #[test]
    fn a_leaked_hook_is_stripped_and_an_unrelated_suffix_is_not() {
        let leaked = assemble(&HashMap::new(), &HashMap::new(), Some("-W0 -r/leak/rails_db_suffix.rb"), None).unwrap();
        assert_eq!(leaked.env.get("RUBYOPT").map(String::as_str), Some("-W0"));
        assert!(leaked.clear.iter().any(|key| key == SUFFIX_VAR));
        assert!(!leaked.env.contains_key(SUFFIX_VAR));

        let only_hook = assemble(&HashMap::new(), &HashMap::new(), Some("-r/leak/rails_db_suffix.rb"), None).unwrap();
        assert!(!only_hook.env.contains_key("RUBYOPT"));
        assert!(only_hook.clear.iter().any(|key| key == "RUBYOPT"));
        assert!(only_hook.clear.iter().any(|key| key == SUFFIX_VAR));

        let unrelated = assemble(&HashMap::new(), &HashMap::new(), Some("-W0"), None).unwrap();
        assert!(unrelated.clear.is_empty(), "{:?}", unrelated.clear);
        assert!(!unrelated.env.contains_key("RUBYOPT"));
    }

    #[test]
    fn mise_rubyopt_replaces_a_leaked_hook_and_still_clears_the_suffix() {
        let mise = map(&[("RUBYOPT", "--yjit")]);
        let ready = assemble(&mise, &HashMap::new(), Some("-r/leak/rails_db_suffix.rb"), None).unwrap();
        assert_eq!(ready.env.get("RUBYOPT").map(String::as_str), Some("--yjit"));
        assert!(!ready.clear.iter().any(|key| key == "RUBYOPT"));
        assert!(ready.clear.iter().any(|key| key == SUFFIX_VAR));
    }

    fn with_data_and_rubyopt<T>(data: &Path, rubyopt: Option<&str>, body: impl FnOnce() -> T) -> T {
        let _guard = rails_suffix::ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let prior_data = std::env::var_os("WORKON_DATA_DIR");
        let prior_ruby = std::env::var_os("RUBYOPT");
        // SAFETY: env mutation is serialized with `ENV_LOCK`, held across this call.
        unsafe {
            std::env::set_var("WORKON_DATA_DIR", data);
            match rubyopt {
                Some(value) => std::env::set_var("RUBYOPT", value),
                None => std::env::remove_var("RUBYOPT"),
            }
        }
        let out = body();
        // SAFETY: still under `ENV_LOCK`, held by `_guard`.
        unsafe {
            restore("WORKON_DATA_DIR", prior_data);
            restore("RUBYOPT", prior_ruby);
        }
        out
    }

    /// The caller holds [`rails_suffix::ENV_LOCK`].
    unsafe fn restore(var: &str, prior: Option<std::ffi::OsString>) {
        match prior {
            // SAFETY: the caller holds `ENV_LOCK` for this update.
            Some(value) => unsafe { std::env::set_var(var, value) },
            // SAFETY: the caller holds `ENV_LOCK` for this update.
            None => unsafe { std::env::remove_var(var) },
        }
    }

    #[test]
    fn a_truncated_script_is_replaced_before_the_session_starts() {
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(
            ws.path().join(".workon.json"),
            r#"{"session_env":{"WORKON_RAILS_DB_SUFFIX":"_ws_abc","RUBYOPT":"-r/old/rails_db_suffix.rb"}}"#,
        )
        .unwrap();
        std::fs::write(data.path().join(rails_suffix::SCRIPT_NAME), "truncated").unwrap();
        let ready = with_data_and_rubyopt(data.path(), Some("-W0"), || prepared(ws.path()).unwrap());
        let script = std::fs::read_to_string(data.path().join(rails_suffix::SCRIPT_NAME)).unwrap();
        assert_eq!(script, include_str!("provision/rails_db_suffix.rb"));
        let rubyopt = ready.env.get("RUBYOPT").unwrap();
        let canonical = data.path().canonicalize().unwrap().join(rails_suffix::SCRIPT_NAME);
        assert!(rubyopt.contains(&format!("-r{}", canonical.display())), "{rubyopt}");
        assert!(rubyopt.split_whitespace().any(|token| token == "-W0"), "{rubyopt}");
        assert!(!rubyopt.contains("/old/"), "{rubyopt}");
        assert_eq!(ready.env.get(SUFFIX_VAR).map(String::as_str), Some("_ws_abc"));
    }

    #[test]
    fn a_hook_that_cannot_be_refreshed_does_not_start() {
        let repo = tempfile::tempdir().unwrap();
        std::fs::create_dir(repo.path().join(".git")).unwrap();
        std::fs::write(repo.path().join(".workon.json"), r#"{"session_env":{"WORKON_RAILS_DB_SUFFIX":"_ws_abc"}}"#)
            .unwrap();
        let err = with_data_and_rubyopt(repo.path(), None, || prepared(repo.path()).unwrap_err());
        assert!(err.to_string().contains("not starting the session"), "{err}");
        assert!(!repo.path().join(rails_suffix::SCRIPT_NAME).exists());
    }

    #[test]
    fn a_plain_directory_drops_a_leaked_hook_and_does_not_take_mise_env() {
        let dir = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("mise.toml"), "[env]\nFROM_MISE = \"yes\"\n").unwrap();
        let ready = with_data_and_rubyopt(data.path(), Some("-W0 -r/leak/rails_db_suffix.rb"), || {
            for_launch(dir.path()).unwrap()
        });
        assert_eq!(ready.env.get("RUBYOPT").map(String::as_str), Some("-W0"));
        assert_eq!(ready.env.len(), 1, "mise env is not injected into a plain session: {:?}", ready.env);
        assert!(ready.clear.iter().any(|key| key == SUFFIX_VAR));
        assert!(!ready.env.contains_key(SUFFIX_VAR));
        assert!(!data.path().join(rails_suffix::SCRIPT_NAME).exists());
    }

    #[test]
    fn a_session_in_a_rails_workspace_refreshes_the_hook() {
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(
            ws.path().join(".workon.json"),
            r#"{"session_env":{"WORKON_RAILS_DB_SUFFIX":"_ws_abc","RUBYOPT":"-r/old/rails_db_suffix.rb"}}"#,
        )
        .unwrap();
        std::fs::write(data.path().join(rails_suffix::SCRIPT_NAME), "truncated").unwrap();
        let ready = with_data_and_rubyopt(data.path(), Some("-W0"), || for_launch(ws.path()).unwrap());
        let script = std::fs::read_to_string(data.path().join(rails_suffix::SCRIPT_NAME)).unwrap();
        assert_eq!(script, include_str!("provision/rails_db_suffix.rb"));
        let rubyopt = ready.env.get("RUBYOPT").unwrap();
        assert!(rubyopt.split_whitespace().any(|token| token == "-W0"), "{rubyopt}");
        assert!(!rubyopt.contains("/old/"), "{rubyopt}");
        assert_eq!(ready.env.get(SUFFIX_VAR).map(String::as_str), Some("_ws_abc"));
    }

    #[test]
    fn a_non_rails_workspace_keeps_its_session_env_off_a_plain_launch() {
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        std::fs::write(ws.path().join(".workon.json"), r#"{"session_env":{"MIX_TEST_PARTITION":"_ws_abc"}}"#).unwrap();
        let ready =
            with_data_and_rubyopt(data.path(), Some("-r/leak/rails_db_suffix.rb"), || for_launch(ws.path()).unwrap());
        assert!(!ready.env.contains_key("MIX_TEST_PARTITION"), "{:?}", ready.env);
        assert!(!ready.env.contains_key("RUBYOPT"));
        assert!(ready.clear.iter().any(|key| key == "RUBYOPT"));
        assert!(ready.clear.iter().any(|key| key == SUFFIX_VAR));
    }

    #[test]
    fn a_workspace_without_the_suffix_does_not_write_the_script() {
        let ws = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let ready =
            with_data_and_rubyopt(data.path(), Some("-r/leak/rails_db_suffix.rb"), || prepared(ws.path()).unwrap());
        assert!(!data.path().join(rails_suffix::SCRIPT_NAME).exists());
        assert!(ready.clear.iter().any(|key| key == SUFFIX_VAR));
        assert!(!ready.env.contains_key(SUFFIX_VAR));
    }
}
