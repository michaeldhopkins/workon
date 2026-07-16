//! The agent a config drives: which pane command *is* the agent, and the
//! argument templates workon injects into it.
//!
//! Declared in a config's `workon` block, which workon strips before handing
//! the rest to zellij:
//!
//! ```kdl
//! workon {
//!     agent command="claude" {
//!         new "--session-id" "{session_id}"
//!         resume "-r" "{session_id}"
//!     }
//! }
//! ```
//!
//! `new` is used when workon opens a fresh workspace, `resume` when `--resume`
//! reopens one. Both are optional: an agent declaring neither is just a pane
//! workon never rewrites.

use anyhow::{bail, Result};
use kdl::{KdlDocument, KdlNode};

use crate::layout::DECLARING_AN_AGENT_URL;

/// Expanded to the session id workon mints (or the one being resumed).
const SESSION_ID: &str = "{session_id}";

/// What workon knows about the agent CLI in a config's layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentSpec {
    /// The pane `command="..."` that identifies the agent.
    pub command: String,
    /// Args for a fresh session. `None` means the agent can't be handed a
    /// session id, so workon mints none and prints no resume hint.
    pub new: Option<Vec<String>>,
    /// Args to resume a session. `None` means `--resume` is unsupported.
    pub resume: Option<Vec<String>>,
}

impl AgentSpec {
    /// The agent assumed when a config declares no `workon` block at all.
    ///
    /// workon drove Claude exclusively before the block existed, so every
    /// config written against those versions keeps its behavior. A config that
    /// wants a different agent — or none — declares a `workon` block.
    pub fn claude_default() -> Self {
        Self {
            command: "claude".to_string(),
            new: Some(vec!["--session-id".to_string(), SESSION_ID.to_string()]),
            resume: Some(vec!["-r".to_string(), SESSION_ID.to_string()]),
        }
    }

    pub fn new_args(&self, session_id: &str) -> Option<Vec<String>> {
        self.new.as_deref().map(|t| expand(t, session_id))
    }

    pub fn resume_args(&self, session_id: &str) -> Option<Vec<String>> {
        self.resume.as_deref().map(|t| expand(t, session_id))
    }
}

fn expand(template: &[String], session_id: &str) -> Vec<String> {
    template.iter().map(|a| a.replace(SESSION_ID, session_id)).collect()
}

/// Parse a `workon` node into an agent, if it declares one.
///
/// `Ok(None)` means the config deliberately drives no agent (`workon {}` with
/// no `agent` child) — distinct from the absent-block case, which the caller
/// maps to [`AgentSpec::claude_default`].
pub fn parse_block(node: &KdlNode) -> Result<Option<AgentSpec>> {
    let Some(children) = node.children() else {
        return Ok(None);
    };

    for child in children.nodes() {
        if child.name().value() != "agent" {
            bail!(
                "workon config: unknown node '{}' in the workon block (only 'agent' is supported)\n\n\
                 See: {DECLARING_AN_AGENT_URL}",
                child.name().value(),
            );
        }
    }

    let agents: Vec<&KdlNode> = children.nodes().iter().filter(|n| n.name().value() == "agent").collect();
    if agents.len() > 1 {
        bail!(
            "workon config: {} agent nodes in the workon block. Declare at most one.\n\n\
             See: {DECLARING_AN_AGENT_URL}",
            agents.len(),
        );
    }
    let Some(agent) = agents.first() else {
        return Ok(None);
    };

    let command = agent
        .get("command")
        .and_then(|e| e.value().as_string())
        .filter(|c| !c.is_empty());
    let Some(command) = command else {
        bail!(
            "workon config: agent needs a non-empty command=\"...\" naming the pane it drives\n\n\
             See: {DECLARING_AN_AGENT_URL}"
        );
    };

    let mut spec = AgentSpec { command: command.to_string(), new: None, resume: None };

    if let Some(caps) = agent.children() {
        for cap in caps.nodes() {
            let name = cap.name().value();
            let slot = match name {
                "new" => &mut spec.new,
                "resume" => &mut spec.resume,
                other => bail!(
                    "workon config: unknown agent capability '{other}' (expected 'new' or 'resume')\n\n\
                     See: {DECLARING_AN_AGENT_URL}"
                ),
            };
            if slot.is_some() {
                bail!(
                    "workon config: agent declares '{name}' twice\n\n\
                     See: {DECLARING_AN_AGENT_URL}"
                );
            }
            *slot = Some(capability_args(cap, name)?);
        }
    }

    Ok(Some(spec))
}

/// The bare string arguments of a capability node. Rejects properties
/// (`new key="v"`) and non-strings so a typo surfaces here rather than as a
/// baffling agent CLI error three layers down.
fn capability_args(node: &KdlNode, name: &str) -> Result<Vec<String>> {
    let mut args = Vec::new();
    for entry in node.entries() {
        if entry.name().is_some() {
            bail!(
                "workon config: agent capability '{name}' takes plain arguments, not properties \
                 (write: {name} \"--flag\" \"value\")\n\n\
                 See: {DECLARING_AN_AGENT_URL}"
            );
        }
        let Some(value) = entry.value().as_string() else {
            bail!(
                "workon config: agent capability '{name}' takes string arguments; found {}\n\n\
                 See: {DECLARING_AN_AGENT_URL}",
                entry.value(),
            );
        };
        args.push(value.to_string());
    }

    if args.is_empty() {
        bail!(
            "workon config: agent capability '{name}' needs at least one argument\n\n\
             See: {DECLARING_AN_AGENT_URL}"
        );
    }
    Ok(args)
}

/// Read the agent from a whole config document. `None` for the `workon` node
/// itself means "no block" — the Claude-compatibility default.
pub fn from_document(doc: &KdlDocument) -> Result<Option<AgentSpec>> {
    match doc.get("workon") {
        None => Ok(Some(AgentSpec::claude_default())),
        Some(node) => parse_block(node),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn doc(src: &str) -> KdlDocument {
        src.parse().unwrap()
    }

    fn agent_of(src: &str) -> Option<AgentSpec> {
        from_document(&doc(src)).unwrap()
    }

    fn err_of(src: &str) -> String {
        from_document(&doc(src)).unwrap_err().to_string()
    }

    #[test]
    fn absent_workon_block_defaults_to_claude() {
        let spec = agent_of("layout {\n    pane command=\"claude\"\n}").unwrap();
        assert_eq!(spec, AgentSpec::claude_default());
        assert_eq!(spec.command, "claude");
    }

    #[test]
    fn claude_default_matches_the_historical_hardcoded_args() {
        let spec = AgentSpec::claude_default();
        assert_eq!(spec.new_args("abc-123").unwrap(), vec!["--session-id", "abc-123"]);
        assert_eq!(spec.resume_args("abc-123").unwrap(), vec!["-r", "abc-123"]);
    }

    #[test]
    fn empty_workon_block_declares_no_agent() {
        assert_eq!(agent_of("workon {\n}\nlayout {\n}"), None);
    }

    #[test]
    fn childless_workon_node_declares_no_agent() {
        assert_eq!(agent_of("workon\nlayout {\n}"), None);
    }

    #[test]
    fn parses_declared_agent_with_both_capabilities() {
        let spec = agent_of(
            r#"workon {
    agent command="codex" {
        new "--session" "{session_id}"
        resume "resume" "{session_id}"
    }
}"#,
        )
        .unwrap();
        assert_eq!(spec.command, "codex");
        assert_eq!(spec.new_args("xyz").unwrap(), vec!["--session", "xyz"]);
        assert_eq!(spec.resume_args("xyz").unwrap(), vec!["resume", "xyz"]);
    }

    #[test]
    fn agent_without_capabilities_supports_neither() {
        let spec = agent_of("workon {\n    agent command=\"codex\"\n}").unwrap();
        assert_eq!(spec.command, "codex");
        assert!(spec.new_args("x").is_none());
        assert!(spec.resume_args("x").is_none());
    }

    #[test]
    fn agent_may_declare_only_one_capability() {
        let spec = agent_of("workon {\n    agent command=\"codex\" {\n        new \"--x\"\n    }\n}").unwrap();
        assert!(spec.new_args("x").is_some());
        assert!(spec.resume_args("x").is_none());
    }

    #[test]
    fn args_without_placeholder_pass_through_verbatim() {
        let spec = agent_of("workon {\n    agent command=\"c\" {\n        new \"--remote-control\"\n    }\n}").unwrap();
        assert_eq!(spec.new_args("ignored").unwrap(), vec!["--remote-control"]);
    }

    #[test]
    fn placeholder_expands_inside_a_larger_string() {
        let spec = agent_of("workon {\n    agent command=\"c\" {\n        new \"id={session_id}!\"\n    }\n}").unwrap();
        assert_eq!(spec.new_args("42").unwrap(), vec!["id=42!"]);
    }

    #[test]
    fn rejects_agent_without_command() {
        let err = err_of("workon {\n    agent {\n        new \"--x\"\n    }\n}");
        assert!(err.contains("needs a non-empty command"), "{err}");
    }

    #[test]
    fn rejects_agent_with_empty_command() {
        let err = err_of("workon {\n    agent command=\"\"\n}");
        assert!(err.contains("needs a non-empty command"), "{err}");
    }

    #[test]
    fn rejects_unknown_capability() {
        let err = err_of("workon {\n    agent command=\"c\" {\n        teleport \"--x\"\n    }\n}");
        assert!(err.contains("unknown agent capability 'teleport'"), "{err}");
        assert!(err.contains("#declaring-the-agent"), "{err}");
    }

    #[test]
    fn rejects_unknown_node_in_workon_block() {
        let err = err_of("workon {\n    agnet command=\"c\"\n}");
        assert!(err.contains("unknown node 'agnet'"), "{err}");
    }

    #[test]
    fn rejects_multiple_agents() {
        let err = err_of("workon {\n    agent command=\"a\"\n    agent command=\"b\"\n}");
        assert!(err.contains("2 agent nodes"), "{err}");
    }

    #[test]
    fn rejects_duplicate_capability() {
        let err = err_of("workon {\n    agent command=\"c\" {\n        new \"--a\"\n        new \"--b\"\n    }\n}");
        assert!(err.contains("declares 'new' twice"), "{err}");
    }

    #[test]
    fn rejects_empty_capability() {
        let err = err_of("workon {\n    agent command=\"c\" {\n        new\n    }\n}");
        assert!(err.contains("needs at least one argument"), "{err}");
    }

    #[test]
    fn rejects_capability_written_as_property() {
        let err = err_of("workon {\n    agent command=\"c\" {\n        new flag=\"--x\"\n    }\n}");
        assert!(err.contains("plain arguments, not properties"), "{err}");
    }

    #[test]
    fn rejects_non_string_capability_argument() {
        let err = err_of("workon {\n    agent command=\"c\" {\n        new 42\n    }\n}");
        assert!(err.contains("string arguments"), "{err}");
    }
}
