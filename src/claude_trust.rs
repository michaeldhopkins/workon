use std::path::{Path, PathBuf};

use anyhow::Result;
use serde_json::Value;

pub fn approve_workspace(ws_dir: &Path) -> Result<()> {
    let claude_json = home_dir()?.join(".claude.json");
    approve_workspace_at(&claude_json, ws_dir)
}

fn approve_workspace_at(claude_json: &Path, ws_dir: &Path) -> Result<()> {
    let ws_key = ws_dir.to_string_lossy().into_owned();

    let mut doc = if claude_json.is_file() {
        let content = std::fs::read_to_string(claude_json)?;
        match serde_json::from_str::<Value>(&content) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("warning: could not parse ~/.claude.json: {e}");
                return Ok(());
            }
        }
    } else {
        Value::Object(serde_json::Map::new())
    };

    let Some(entry) = trust_entry(&mut doc, &ws_key) else {
        eprintln!("warning: ~/.claude.json is not in the expected shape; workspace not pre-trusted");
        return Ok(());
    };
    entry.insert("hasTrustDialogAccepted".into(), Value::Bool(true));

    let output = serde_json::to_string_pretty(&doc)?;

    let parent = claude_json.parent().unwrap_or(Path::new("."));
    let tmp = tempfile::NamedTempFile::new_in(parent)?;
    std::fs::write(tmp.path(), &output)?;
    tmp.persist(claude_json)?;
    Ok(())
}

/// `projects.<ws_key>` as an object, created when missing; `None` when the file holds some other
/// shape there, which is not ours to overwrite.
fn trust_entry<'a>(doc: &'a mut Value, ws_key: &str) -> Option<&'a mut serde_json::Map<String, Value>> {
    let projects = doc.as_object_mut()?.entry("projects").or_insert_with(|| Value::Object(serde_json::Map::new()));
    projects.as_object_mut()?.entry(ws_key).or_insert_with(|| Value::Object(serde_json::Map::new())).as_object_mut()
}

fn home_dir() -> Result<PathBuf> {
    crate::home::home_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_trust_entry_in_existing_json() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_json = tmp.path().join(".claude.json");
        std::fs::write(&claude_json, r#"{"existingKey": true}"#).unwrap();

        let ws_dir = tmp.path().join(".worktrees/myproject-ws-abc123");
        approve_workspace_at(&claude_json, &ws_dir).unwrap();

        let content: Value = serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
        let ws_key = ws_dir.to_string_lossy().into_owned();
        assert_eq!(content["existingKey"], true);
        assert_eq!(content["projects"][&ws_key]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn creates_new_claude_json_if_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_json = tmp.path().join(".claude.json");
        let ws_dir = tmp.path().join(".worktrees/proj-ws-def456");

        approve_workspace_at(&claude_json, &ws_dir).unwrap();

        let content: Value = serde_json::from_str(&std::fs::read_to_string(&claude_json).unwrap()).unwrap();
        let ws_key = ws_dir.to_string_lossy().into_owned();
        assert_eq!(content["projects"][&ws_key]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn handles_malformed_json_gracefully() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_json = tmp.path().join(".claude.json");
        std::fs::write(&claude_json, "not json{{{").unwrap();

        let ws_dir = tmp.path().join(".worktrees/proj-ws-bad");
        let result = approve_workspace_at(&claude_json, &ws_dir);
        assert!(result.is_ok());
    }
    /// Valid JSON of the wrong shape (a top level or `projects` that is not an object, or a
    /// workspace entry that is not one) used to panic on an `unwrap`; it is left untouched.
    #[test]
    fn leaves_json_of_an_unexpected_shape_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let claude_json = tmp.path().join(".claude.json");
        let ws_dir = tmp.path().join(".worktrees/proj-ws-shape");
        let ws_key = ws_dir.to_string_lossy().into_owned();
        let shapes = [
            "[]".to_string(),
            "null".to_string(),
            r#"{"projects": 3}"#.to_string(),
            serde_json::json!({ "projects": { ws_key.clone(): "x" } }).to_string(),
        ];
        for shape in shapes {
            std::fs::write(&claude_json, &shape).unwrap();
            assert!(approve_workspace_at(&claude_json, &ws_dir).is_ok(), "{shape}");
            assert_eq!(std::fs::read_to_string(&claude_json).unwrap(), shape, "unchanged");
        }
    }
}
