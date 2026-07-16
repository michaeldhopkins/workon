use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use kdl::{KdlDocument, KdlEntry, KdlNode};
use tempfile::NamedTempFile;

use crate::agent::{self, AgentSpec};
use crate::trust;

const EMBEDDED_LAYOUT: &str = include_str!("../layouts/workon.kdl");

pub(crate) const CREATING_A_CONFIG_URL: &str =
    "https://github.com/michaeldhopkins/workon#creating-a-config";

/// Where the `workon { agent ... }` block is documented. Agent errors point
/// here rather than at the general config section.
pub(crate) const DECLARING_AN_AGENT_URL: &str =
    "https://github.com/michaeldhopkins/workon#declaring-the-agent";

#[derive(Debug)]
pub struct ResolvedLayout {
    temp: NamedTempFile,
}

impl ResolvedLayout {
    pub fn path(&self) -> &Path {
        self.temp.path()
    }
}

/// A parsed workon config: the zellij layout it contributes, plus the agent it
/// drives. Constructed by [`read_config`], which is the only place that reads
/// (and trust-checks) the file.
#[derive(Debug)]
pub struct Config {
    /// Zellij-only layout source, with workon's own `workon` block removed.
    /// Everything downstream — dependency checks, the focused-command guard,
    /// the tempfile handed to zellij — sees only what zellij understands.
    pub layout: String,
    /// The agent this config drives, or `None` when it deliberately drives none.
    pub agent: Option<AgentSpec>,
    /// The same layout, still parsed. Injection works from this rather than
    /// re-parsing `layout`, so workon never has to blame the user for its own
    /// serialization.
    doc: KdlDocument,
    /// How many panes run [`Self::agent`]. Counted structurally while the
    /// document is parsed, so it can't disagree with what injection matches.
    agent_panes: usize,
}

impl Config {
    /// Split a raw config into its agent declaration and the zellij remainder.
    pub fn parse(src: &str) -> Result<Config> {
        let mut doc: KdlDocument = src
            .parse()
            .context("workon config is not valid KDL")?;
        let agent = agent::from_document(&doc)?;
        doc.nodes_mut().retain(|n| n.name().value() != "workon");
        let agent_panes = agent.as_ref().map_or(0, |a| count_panes_running(&doc, &a.command));
        Ok(Config { layout: doc.to_string(), agent, doc, agent_panes })
    }

    /// Whether the layout actually runs the declared agent. A config can name an
    /// agent its layout never launches — the Claude compatibility default over a
    /// config whose panes are all something else is exactly that case.
    pub fn runs_agent(&self) -> bool {
        self.agent_panes > 0
    }

    /// The layout as-is, for a session workon injects nothing into.
    pub fn resolve(&self) -> Result<ResolvedLayout> {
        build(self.layout.clone())
    }

    /// Bail if the layout runs the agent in more than one pane.
    ///
    /// One session id can only belong to one process; two agent panes would both
    /// be handed it and race on the same transcript. Only sessions that actually
    /// inject care, so this is a separate check rather than part of parsing — a
    /// plain session workon rewrites nothing in is none of its business.
    ///
    /// Call it eagerly, before anything is provisioned, the way the CLI does.
    pub fn ensure_single_agent_pane(&self) -> Result<()> {
        let Some(agent) = self.agent.as_ref() else {
            return Ok(());
        };
        if self.agent_panes > 1 {
            bail!(
                "layout runs {} panes with command=\"{}\", so workon can't hand out one session id. \
                 Run the agent in a single pane.\n\n\
                 See: {DECLARING_AN_AGENT_URL}",
                self.agent_panes,
                agent.command,
            );
        }
        Ok(())
    }

    /// The layout with `args` injected into the agent's pane. A no-op when the
    /// config declares no agent or the layout doesn't run it.
    pub fn resolve_with_agent_args(&self, args: &[String]) -> Result<ResolvedLayout> {
        let Some(agent) = self.agent.as_ref() else {
            return self.resolve();
        };
        self.ensure_single_agent_pane()?;
        let mut doc = self.doc.clone();
        inject_into(&mut doc, &agent.command, args);
        build(doc.to_string())
    }

    /// Bail unless this config can resume: it must declare an agent, that agent
    /// must support resuming, and its pane must actually be in the layout.
    pub fn ensure_resume_compatible(&self, config_name: &str) -> Result<()> {
        let Some(agent) = self.agent.as_ref() else {
            bail!(
                "--resume needs a config that drives an agent, but '{config_name}' declares none\n\n\
                 See: {DECLARING_AN_AGENT_URL}"
            );
        };
        if agent.resume.is_none() {
            bail!(
                "--resume only works with an agent that declares a 'resume' capability; \
                 '{config_name}' declares none for '{}'\n\n\
                 See: {DECLARING_AN_AGENT_URL}",
                agent.command,
            );
        }
        if !self.runs_agent() {
            bail!(
                "--resume only works with configs whose layout runs the agent, but '{config_name}' \
                 has no pane running '{}'",
                agent.command,
            );
        }
        Ok(())
    }
}

/// How many panes in the document run `command`. Structural, matching what
/// [`inject_into`] targets — a line scan would miss KDL spellings the parser
/// accepts (raw strings, escapes, continuations).
fn count_panes_running(doc: &KdlDocument, command: &str) -> usize {
    doc.nodes()
        .iter()
        .map(|node| {
            let here = usize::from(
                node.get("command").and_then(|e| e.value().as_string()).is_some_and(|c| c == command),
            );
            // A commanded pane is a leaf — its children are args/env, never
            // nested panes — so a match ends the descent.
            if here == 1 {
                return 1;
            }
            node.children().map_or(0, |kids| count_panes_running(kids, command))
        })
        .sum()
}

/// Read, trust-check, and parse the config named by `config`.
///
/// `None` or `Some("default")` resolves to, in order:
///   1. `~/.config/workon/configs/default.kdl`
///   2. `~/.config/workon/layout.kdl` (legacy single-config path)
///   3. The embedded layout
///
/// Any other name resolves only to `~/.config/workon/configs/<name>.kdl`,
/// erroring if the file is absent.
pub fn read_config(config: Option<&str>) -> Result<Config> {
    let workon_dir = config_dir()?.join("workon");
    read_config_from(&workon_dir, config)
}

/// Return the `command="..."` value of the layout's focused pane (the one
/// marked `focus=true`). Falls back to the first commanded pane if nothing
/// is explicitly focused. Returns `None` when the layout has no commanded
/// panes at all.
///
/// Bails if **more than one** commanded pane is marked `focus=true` — that's
/// almost always a typo, and our mismatch guard would otherwise silently
/// pick whichever line we saw first.
///
/// Used to detect whether a running zellij session matches the requested
/// layout: if the focused command isn't somewhere in the session's process
/// tree, the user almost certainly launched the session with a different
/// config and attaching would silently apply that config's layout instead.
pub fn focused_command(layout: &str) -> Result<Option<String>> {
    let focused: Vec<&str> = layout
        .lines()
        .filter(|line| line.contains("focus=true"))
        .filter_map(command_in_line)
        .collect();

    if focused.len() > 1 {
        bail!(
            "your layout has {} panes marked focus=true ({}). Mark only one. workon uses the focused pane to tell which config a session was launched with.\n\n\
             See: {}",
            focused.len(),
            focused.join(", "),
            CREATING_A_CONFIG_URL,
        );
    }

    if let Some(cmd) = focused.first() {
        return Ok(Some((*cmd).to_string()));
    }

    Ok(layout.lines().filter_map(command_in_line).next().map(String::from))
}

/// Eager validation hook for layouts. Currently just probes `focused_command`
/// so the multi-focus error surfaces before any subprocesses run. Add other
/// checks here as needed.
pub fn validate_layout(layout: &str) -> Result<()> {
    let _ = focused_command(layout)?;
    Ok(())
}

fn command_in_line(line: &str) -> Option<&str> {
    let after = line.split("command=\"").nth(1)?;
    let end = after.find('"')?;
    Some(&after[..end])
}

fn build(content: String) -> Result<ResolvedLayout> {
    let tmp = NamedTempFile::with_suffix(".kdl")?;
    std::fs::write(tmp.path(), &content)?;
    Ok(ResolvedLayout { temp: tmp })
}

fn read_config_from(workon_dir: &Path, config: Option<&str>) -> Result<Config> {
    Config::parse(&read_config_source(workon_dir, config)?)
}

fn read_config_source(workon_dir: &Path, config: Option<&str>) -> Result<String> {
    let configs_dir = workon_dir.join("configs");
    match config {
        None | Some("default") => {
            let default_path = configs_dir.join("default.kdl");
            if default_path.is_file() {
                return trust::read_trusted(workon_dir, &default_path);
            }
            let legacy = workon_dir.join("layout.kdl");
            if legacy.is_file() {
                return trust::read_trusted(workon_dir, &legacy);
            }
            // The embedded layout ships inside the binary — nothing on disk can
            // tamper with it, so it needs no trust pin.
            Ok(EMBEDDED_LAYOUT.to_string())
        }
        Some(name) => {
            if !is_valid_config_name(name) {
                bail!("invalid config name '{name}': use letters, digits, '-', or '_'");
            }
            let path = configs_dir.join(format!("{name}.kdl"));
            if !path.is_file() {
                bail!(
                    "workon config '{}' not found.\n\
                     Looked at: {}\n\
                     How to create one: {}",
                    name,
                    path.display(),
                    CREATING_A_CONFIG_URL,
                );
            }
            trust::read_trusted(workon_dir, &path)
        }
    }
}

fn is_valid_config_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// Append `args` to the pane running `agent_cmd`.
///
/// Merges into an existing `args` node rather than adding a second one: zellij
/// silently honors only the *first* `args` node in a pane and drops the rest,
/// so a sibling would quietly discard one set or the other. Injected args go
/// last, which also lets them win for flags whose last occurrence takes effect.
fn inject_into(doc: &mut KdlDocument, agent_cmd: &str, args: &[String]) {
    for node in doc.nodes_mut() {
        let is_agent = node
            .get("command")
            .and_then(|e| e.value().as_string())
            .is_some_and(|c| c == agent_cmd);

        if is_agent {
            append_args(node, args);
            continue;
        }

        if let Some(children) = node.children_mut() {
            inject_into(children, agent_cmd, args);
        }
    }
}

fn append_args(pane: &mut KdlNode, args: &[String]) {
    let children = pane.ensure_children();
    match children.get_mut("args") {
        Some(existing) => {
            for arg in args {
                existing.push(KdlEntry::new(arg.clone()));
            }
        }
        None => {
            let mut node = KdlNode::new("args");
            for arg in args {
                node.push(KdlEntry::new(arg.clone()));
            }
            children.nodes_mut().push(node);
        }
    }
}

fn config_dir() -> Result<PathBuf> {
    std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|_| std::env::var("HOME").map(|h| PathBuf::from(h).join(".config")))
        .map_err(|_| anyhow::anyhow!("cannot determine config directory"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Append a `[[trusted]]` pin for an on-disk config so `read_config_from`
    /// will load it — the test-side equivalent of a user hand-editing
    /// `trusted.toml`. `body` must equal the file's exact bytes on disk.
    fn bless(workon_dir: &Path, file: &Path, body: &str) {
        use std::io::Write;
        let canon = std::fs::canonicalize(file).unwrap();
        let entry = format!(
            "[[trusted]]\npath = {:?}\nsha256 = \"{}\"\n",
            canon.to_string_lossy(),
            trust::sha256_hex(body.as_bytes()),
        );
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(workon_dir.join("trusted.toml"))
            .unwrap();
        f.write_all(entry.as_bytes()).unwrap();
    }

    /// A minimal valid layout body, since configs must now parse as KDL.
    fn layout_of(command: &str) -> String {
        format!("layout {{\n    pane command=\"{command}\" size=\"80%\" focus=true\n}}\n")
    }

    fn write_config(workon_dir: &Path, name: &str, body: &str) {
        std::fs::create_dir_all(workon_dir.join("configs")).unwrap();
        let path = workon_dir.join(format!("configs/{name}.kdl"));
        std::fs::write(&path, body).unwrap();
        bless(workon_dir, &path, body);
    }

    fn resolved_text(resolved: &ResolvedLayout) -> String {
        std::fs::read_to_string(resolved.path()).unwrap()
    }

    #[test]
    fn default_uses_embedded_when_nothing_present() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = read_config_from(tmp.path(), None).unwrap();
        assert!(cfg.layout.contains("default_mode"));
        assert!(cfg.layout.contains("branchdiff"));
        assert!(cfg.layout.contains("claude"));
    }

    #[test]
    fn embedded_layout_declares_the_claude_agent() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = read_config_from(tmp.path(), None).unwrap();
        let agent = cfg.agent.expect("embedded layout should declare an agent");
        assert_eq!(agent.command, "claude");
        assert_eq!(agent.new_args("abc").unwrap(), vec!["--session-id", "abc"]);
        assert_eq!(agent.resume_args("abc").unwrap(), vec!["-r", "abc"]);
    }

    #[test]
    fn embedded_layout_agent_matches_the_compatibility_default() {
        // The embedded layout declares explicitly what an undeclared config
        // gets implicitly. If these drift, upgrading a config to an explicit
        // block would silently change its behavior.
        let tmp = tempfile::tempdir().unwrap();
        let cfg = read_config_from(tmp.path(), None).unwrap();
        assert_eq!(cfg.agent.unwrap(), AgentSpec::claude_default());
    }

    #[test]
    fn workon_block_is_stripped_from_the_zellij_layout() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = read_config_from(tmp.path(), None).unwrap();
        assert!(!cfg.layout.contains("workon {"), "{}", cfg.layout);
        assert!(!cfg.layout.contains("--session-id"), "{}", cfg.layout);
        // The zellij half survives intact.
        assert!(cfg.layout.contains("command=\"claude\""));
        assert!(cfg.layout.contains("on_force_close"));
    }

    #[test]
    fn stripped_layout_is_still_valid_kdl() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = read_config_from(tmp.path(), None).unwrap();
        assert!(cfg.layout.parse::<KdlDocument>().is_ok());
    }

    #[test]
    fn default_uses_legacy_layout_kdl_when_no_configs_default() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("layout.kdl");
        let body = layout_of("legacycmd");
        std::fs::write(&legacy, &body).unwrap();
        bless(tmp.path(), &legacy, &body);

        let cfg = read_config_from(tmp.path(), None).unwrap();
        assert!(cfg.layout.contains("legacycmd"));
    }

    #[test]
    fn default_prefers_configs_default_over_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("layout.kdl"), layout_of("legacycmd")).unwrap();
        write_config(tmp.path(), "default", &layout_of("newdefault"));

        let cfg = read_config_from(tmp.path(), None).unwrap();
        assert!(cfg.layout.contains("newdefault"));
    }

    #[test]
    fn explicit_default_name_uses_same_resolution_as_none() {
        let tmp = tempfile::tempdir().unwrap();
        let legacy = tmp.path().join("layout.kdl");
        let body = layout_of("legacycmd");
        std::fs::write(&legacy, &body).unwrap();
        bless(tmp.path(), &legacy, &body);

        let cfg = read_config_from(tmp.path(), Some("default")).unwrap();
        assert!(cfg.layout.contains("legacycmd"));
    }

    #[test]
    fn named_config_loads_from_configs_dir() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "opencode", &layout_of("opencode"));

        let cfg = read_config_from(tmp.path(), Some("opencode")).unwrap();
        assert!(cfg.layout.contains("opencode"));
    }

    #[test]
    fn named_config_refused_until_blessed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("configs")).unwrap();
        std::fs::write(tmp.path().join("configs/evil.kdl"), layout_of("evil")).unwrap();

        let err = read_config_from(tmp.path(), Some("evil")).unwrap_err().to_string();
        assert!(err.contains("untrusted config"), "{err}");
        assert!(err.contains("[[trusted]]"), "{err}");
    }

    #[test]
    fn default_kdl_refused_until_blessed() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("configs")).unwrap();
        std::fs::write(tmp.path().join("configs/default.kdl"), layout_of("claude")).unwrap();

        let err = read_config_from(tmp.path(), None).unwrap_err().to_string();
        assert!(err.contains("untrusted config"), "{err}");
    }

    #[test]
    fn named_config_missing_errors_with_helpful_message() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_config_from(tmp.path(), Some("missing")).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("'missing'"), "{msg}");
        assert!(msg.contains("configs/missing.kdl"), "{msg}");
        assert!(msg.contains("#creating-a-config"), "{msg}");
    }

    #[test]
    fn named_config_does_not_fall_back_to_legacy() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("layout.kdl"), layout_of("claude")).unwrap();

        let err = read_config_from(tmp.path(), Some("opencode")).unwrap_err();
        assert!(err.to_string().contains("opencode"));
    }

    #[test]
    fn invalid_kdl_config_errors_clearly() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "broken", "layout { pane \"unterminated\n");
        let err = read_config_from(tmp.path(), Some("broken")).unwrap_err();
        assert!(err.to_string().contains("not valid KDL"), "{err}");
    }

    #[test]
    fn build_writes_content_to_tempfile_at_path() {
        let resolved = build("HELLO".to_string()).unwrap();
        assert_eq!(resolved_text(&resolved), "HELLO");
    }

    #[test]
    fn rejects_empty_config_name() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_config_from(tmp.path(), Some("")).unwrap_err();
        assert!(err.to_string().contains("invalid config name"), "{err}");
    }

    #[test]
    fn rejects_path_traversal_in_config_name() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_config_from(tmp.path(), Some("../etc/hosts")).unwrap_err();
        assert!(err.to_string().contains("invalid config name"), "{err}");
    }

    #[test]
    fn rejects_subdirectory_config_name() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_config_from(tmp.path(), Some("foo/bar")).unwrap_err();
        assert!(err.to_string().contains("invalid config name"), "{err}");
    }

    #[test]
    fn rejects_dotfile_config_name() {
        let tmp = tempfile::tempdir().unwrap();
        let err = read_config_from(tmp.path(), Some(".hidden")).unwrap_err();
        assert!(err.to_string().contains("invalid config name"), "{err}");
    }

    #[test]
    fn accepts_valid_config_name_with_dash_and_underscore() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "my-cfg_2", &layout_of("ok"));
        let cfg = read_config_from(tmp.path(), Some("my-cfg_2")).unwrap();
        assert!(cfg.layout.contains("ok"));
    }

    #[test]
    fn is_valid_config_name_rules() {
        assert!(is_valid_config_name("opencode"));
        assert!(is_valid_config_name("my-cfg_2"));
        assert!(is_valid_config_name("ABC123"));
        assert!(!is_valid_config_name(""));
        assert!(!is_valid_config_name("a/b"));
        assert!(!is_valid_config_name("a b"));
        assert!(!is_valid_config_name(".dot"));
        assert!(!is_valid_config_name(".."));
    }

    #[test]
    fn resolve_with_agent_args_injects_session_id() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "myclaude", &layout_of("claude"));
        let cfg = read_config_from(tmp.path(), Some("myclaude")).unwrap();

        let args = cfg.agent.as_ref().unwrap().new_args("abc-123").unwrap();
        let resolved = cfg.resolve_with_agent_args(&args).unwrap();
        let content = resolved_text(&resolved);
        assert!(content.contains("command=\"claude\""));
        assert!(content.contains(r#"args "--session-id" "abc-123""#), "{content}");
    }

    #[test]
    fn resolve_with_agent_args_injects_resume_args() {
        let tmp = tempfile::tempdir().unwrap();
        write_config(tmp.path(), "myclaude", &layout_of("claude"));
        let cfg = read_config_from(tmp.path(), Some("myclaude")).unwrap();

        let args = cfg.agent.as_ref().unwrap().resume_args("uuid-xyz").unwrap();
        let resolved = cfg.resolve_with_agent_args(&args).unwrap();
        assert!(resolved_text(&resolved).contains(r#"args "-r" "uuid-xyz""#));
    }

    #[test]
    fn resolve_leaves_layout_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let cfg = read_config_from(tmp.path(), None).unwrap();
        assert_eq!(resolved_text(&cfg.resolve().unwrap()), cfg.layout);
    }

    #[test]
    fn focused_command_returns_command_on_focus_true_line() {
        let layout = r#"layout {
    pane command="claude" size="80%" focus=true
    pane command="branchdiff" size="50%"
}"#;
        assert_eq!(focused_command(layout).unwrap(), Some("claude".to_string()));
    }

    #[test]
    fn focused_command_picks_focused_when_not_first() {
        let layout = r#"layout {
    pane command="branchdiff" size="50%"
    pane command="opencode" size="80%" focus=true
}"#;
        assert_eq!(focused_command(layout).unwrap(), Some("opencode".to_string()));
    }

    #[test]
    fn focused_command_falls_back_to_first_when_no_focus() {
        let layout = r#"pane command="branchdiff"
pane command="specdiff""#;
        assert_eq!(focused_command(layout).unwrap(), Some("branchdiff".to_string()));
    }

    #[test]
    fn focused_command_returns_none_when_no_commands() {
        let layout = r#"layout {
    pane size="20%"
    pane size="80%"
}"#;
        assert_eq!(focused_command(layout).unwrap(), None);
    }

    #[test]
    fn focused_command_finds_focus_in_embedded_layout() {
        let cfg = Config::parse(EMBEDDED_LAYOUT).unwrap();
        assert_eq!(focused_command(&cfg.layout).unwrap(), Some("claude".to_string()));
    }

    /// The `workon` block names `command="claude"` too. If it ever reached the
    /// line scanner, the no-focus fallback would match the agent declaration
    /// instead of a real pane.
    #[test]
    fn focused_command_ignores_the_workon_block() {
        let src = r#"workon {
    agent command="ghost" {
        new "--x"
    }
}

layout {
    pane command="realpane" size="80%"
}
"#;
        let cfg = Config::parse(src).unwrap();
        assert_eq!(focused_command(&cfg.layout).unwrap(), Some("realpane".to_string()));
    }

    #[test]
    fn focused_command_errors_when_multiple_panes_are_focused() {
        let layout = r#"layout {
    pane command="claude" size="80%" focus=true
    pane command="branchdiff" size="50%" focus=true
}"#;
        let err = focused_command(layout).expect_err("should error on multi-focus");
        let msg = err.to_string();
        assert!(msg.contains("2 panes"), "{msg}");
        assert!(msg.contains("claude"), "{msg}");
        assert!(msg.contains("branchdiff"), "{msg}");
        assert!(msg.contains("Mark only one"), "{msg}");
        assert!(msg.contains("#creating-a-config"), "{msg}");
    }

    #[test]
    fn focused_command_ignores_focus_on_panes_without_command() {
        // A focused empty pane shouldn't count toward the multi-focus check —
        // it's not something the mismatch guard could match against anyway.
        let layout = r#"layout {
    pane command="claude" focus=true
    pane size="20%" focus=true
}"#;
        assert_eq!(focused_command(layout).unwrap(), Some("claude".to_string()));
    }

    #[test]
    fn validate_layout_passes_for_well_formed_layout() {
        let cfg = Config::parse(EMBEDDED_LAYOUT).unwrap();
        assert!(validate_layout(&cfg.layout).is_ok());
    }

    #[test]
    fn validate_layout_rejects_multi_focus() {
        let layout = r#"pane command="claude" focus=true
pane command="branchdiff" focus=true"#;
        assert!(validate_layout(layout).is_err());
    }

    #[test]
    fn ensure_resume_compatible_passes_for_the_embedded_layout() {
        let cfg = Config::parse(EMBEDDED_LAYOUT).unwrap();
        assert!(cfg.ensure_resume_compatible("default").is_ok());
    }

    #[test]
    fn ensure_resume_compatible_errors_when_layout_lacks_the_agent_pane() {
        // The opencode case: the compatibility default names claude, but no
        // claude pane exists to resume into.
        let cfg = Config::parse(&layout_of("opencode")).unwrap();
        let err = cfg.ensure_resume_compatible("opencode").unwrap_err().to_string();
        assert!(err.contains("--resume"), "{err}");
        assert!(err.contains("opencode"), "{err}");
        assert!(err.contains("claude"), "{err}");
    }

    /// The agent pane is found the same way injection finds it — by parsing.
    /// A KDL raw string is the same pane to zellij, but scanning lines for the
    /// literal `command="claude"` would miss it and reject a resumable config.
    #[test]
    fn agent_pane_is_matched_structurally_not_textually() {
        let src = "layout {\n    pane command=r\"claude\" focus=true\n}";
        let cfg = Config::parse(src).unwrap();
        assert!(cfg.ensure_resume_compatible("raw").is_ok());

        let args = cfg.agent.as_ref().unwrap().new_args("abc").unwrap();
        let resolved = cfg.resolve_with_agent_args(&args).unwrap();
        assert!(resolved_text(&resolved).contains("abc"), "injection must find it too");
    }

    #[test]
    fn ensure_resume_compatible_errors_when_config_declares_no_agent() {
        let cfg = Config::parse("workon {\n}\nlayout {\n    pane command=\"vim\"\n}").unwrap();
        let err = cfg.ensure_resume_compatible("noagent").unwrap_err().to_string();
        assert!(err.contains("declares none"), "{err}");
    }

    #[test]
    fn ensure_resume_compatible_errors_when_agent_cannot_resume() {
        let src = r#"workon {
    agent command="codex" {
        new "--session" "{session_id}"
    }
}

layout {
    pane command="codex"
}
"#;
        let cfg = Config::parse(src).unwrap();
        let err = cfg.ensure_resume_compatible("codex").unwrap_err().to_string();
        assert!(err.contains("'resume' capability"), "{err}");
        assert!(err.contains("codex"), "{err}");
    }

    /// Inject through the real path: parse a config, then resolve it with args.
    fn inject(src: &str, args: &[&str]) -> Result<String> {
        let owned: Vec<String> = args.iter().map(|a| (*a).to_string()).collect();
        let resolved = Config::parse(src)?.resolve_with_agent_args(&owned)?;
        Ok(resolved_text(&resolved))
    }

    #[test]
    fn inject_targets_only_the_agent_pane() {
        let result = inject(
            "layout {\n    pane command=\"claude\" size=\"80%\" focus=true\n    pane command=\"branchdiff\" size=\"50%\"\n}",
            &["--session-id", "abc-123"],
        )
        .unwrap();
        assert!(result.contains(r#"args "--session-id" "abc-123""#));
        for line in result.lines() {
            if line.contains("branchdiff") {
                assert!(!line.contains("session-id"), "branchdiff pane should not get session-id");
            }
        }
    }

    #[test]
    fn inject_reaches_deeply_nested_panes() {
        let result = inject(
            "layout {\n    tab {\n        pane split_direction=\"vertical\" {\n            pane split_direction=\"horizontal\" {\n                pane command=\"claude\" size=\"80%\" focus=true\n            }\n        }\n    }\n}",
            &["--x"],
        )
        .unwrap();
        assert!(result.contains(r#"args "--x""#), "{result}");
    }

    /// The regression that motivated structural injection: zellij honors only
    /// the *first* `args` node in a pane and silently ignores any sibling, so
    /// injected args must merge into the user's node, not sit beside it.
    #[test]
    fn inject_merges_into_an_existing_args_node() {
        let result = inject(
            "layout {\n    pane command=\"claude\" size=\"80%\" focus=true {\n        args \"--model\" \"opus\"\n    }\n}",
            &["--session-id", "abc-123"],
        )
        .unwrap();

        let doc: KdlDocument = result.parse().expect("injected layout must be valid KDL");
        let pane = doc.get("layout").unwrap().children().unwrap().get("pane").unwrap();
        let args: Vec<&KdlNode> = pane
            .children()
            .unwrap()
            .nodes()
            .iter()
            .filter(|n| n.name().value() == "args")
            .collect();
        assert_eq!(args.len(), 1, "must be exactly one args node, got: {result}");

        let values: Vec<&str> = args[0].entries().iter().filter_map(|e| e.value().as_string()).collect();
        assert_eq!(values, vec!["--model", "opus", "--session-id", "abc-123"]);
    }

    #[test]
    fn inject_output_is_valid_kdl_when_creating_an_args_node() {
        let result = inject("layout {\n    pane command=\"claude\" size=\"80%\" focus=true\n}", &["--x"]).unwrap();
        assert!(result.parse::<KdlDocument>().is_ok(), "{result}");
    }

    #[test]
    fn inject_is_a_noop_when_no_pane_matches() {
        let result = inject("layout {\n    pane command=\"opencode\" size=\"80%\"\n}", &["-r", "some-uuid"]).unwrap();
        assert!(!result.contains("some-uuid"));
    }

    #[test]
    fn resolve_with_agent_args_is_a_noop_without_an_agent() {
        let result = inject("workon {\n}\nlayout {\n    pane command=\"vim\"\n}", &["--x"]).unwrap();
        assert!(!result.contains("--x"));
    }

    /// One session id can only belong to one process. Two agent panes would
    /// both be handed it and race on the same transcript.
    #[test]
    fn inject_refuses_a_layout_with_two_agent_panes() {
        let err = inject(
            "layout {\n    pane command=\"claude\" focus=true\n    pane command=\"claude\"\n}",
            &["--session-id", "abc"],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("2 panes"), "{err}");
        assert!(err.contains("single pane"), "{err}");
    }

    /// The guard is on injection, not on parsing: a config workon injects
    /// nothing into is none of its business.
    #[test]
    fn two_agent_panes_are_fine_when_nothing_is_injected() {
        let src = "layout {\n    pane command=\"claude\" focus=true\n    pane command=\"claude\"\n}";
        let cfg = Config::parse(src).unwrap();
        assert_eq!(cfg.agent_panes, 2);
        assert!(cfg.resolve().is_ok(), "a plain session must still launch");
    }

    /// The CLI calls this before provisioning, so a `-w` run fails without
    /// leaving a half-built worktree behind.
    #[test]
    fn ensure_single_agent_pane_rejects_two_and_accepts_one() {
        let two = Config::parse(
            "layout {\n    pane command=\"claude\" focus=true\n    pane command=\"claude\"\n}",
        )
        .unwrap();
        let err = two.ensure_single_agent_pane().unwrap_err().to_string();
        assert!(err.contains("2 panes"), "{err}");

        let one = Config::parse(&layout_of("claude")).unwrap();
        assert!(one.ensure_single_agent_pane().is_ok());
    }

    /// A config with no agent can't have too many agent panes.
    #[test]
    fn ensure_single_agent_pane_passes_without_an_agent() {
        let cfg = Config::parse("workon {\n}\nlayout {\n    pane command=\"vim\"\n    pane command=\"vim\"\n}").unwrap();
        assert!(cfg.ensure_single_agent_pane().is_ok());
    }

    #[test]
    fn runs_agent_is_false_when_the_layout_lacks_the_agent_pane() {
        let cfg = Config::parse(&layout_of("opencode")).unwrap();
        assert!(cfg.agent.is_some(), "compatibility default still names claude");
        assert!(!cfg.runs_agent(), "but no pane runs it");
    }

    #[test]
    fn count_panes_running_counts_across_nesting() {
        let src = "layout {\n    tab {\n        pane command=\"claude\"\n        pane split_direction=\"vertical\" {\n            pane command=\"claude\"\n        }\n    }\n    pane command=\"vim\"\n}";
        let cfg = Config::parse(src).unwrap();
        assert_eq!(cfg.agent_panes, 2);
    }
}

