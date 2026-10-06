//! A workon config, and the layout workon writes from it for zellij.
//!
//! Input: `layout\0arg\0arg…`. The layout is a config as a user writes it; the
//! args stand in for what workon injects into the agent's pane (a minted session
//! id, a `--resume` id typed on the command line, the config's own templates).
//!
//! A config that does not parse is rejected, which is the right outcome, so the
//! target returns there. Past that point it asserts, using the kdl crate as the
//! oracle for what zellij will read:
//!
//! - the layout handed to zellij parses, and carries no `workon` block;
//! - injecting leaves exactly as many agent panes, and the same set of pane
//!   commands, as before (an arg cannot open a pane or break out of its string);
//! - the agent pane's first `args` node reads back as its old entries followed by
//!   exactly the injected args (zellij honours only the first `args` node);
//! - with no agent pane, injection changes nothing at all.
#![no_main]

use kdl::{KdlDocument, KdlEntry, KdlNode, KdlValue};
use libfuzzer_sys::fuzz_target;
use workon::layout::Config;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    let mut parts = text.split('\0');
    let src = parts.next().unwrap_or_default();
    let args: Vec<String> = parts.map(str::to_string).collect();

    let Ok(cfg) = Config::parse(src) else { return };
    let _ = workon::layout::focused_command(&cfg.layout);

    let before: KdlDocument = cfg
        .layout
        .parse()
        .unwrap_or_else(|e| panic!("the layout workon hands zellij does not parse: {e}\n{}", cfg.layout));
    assert!(before.get("workon").is_none(), "the workon block reached zellij");

    let Some(agent) = cfg.agent.clone() else { return };
    let resolved = match cfg.resolve_with_agent_args(&args) {
        Ok(r) => r,
        Err(_) => {
            assert!(
                cfg.ensure_single_agent_pane().is_err(),
                "injection failed on a layout with at most one agent pane"
            );
            return;
        }
    };
    let written = std::fs::read_to_string(resolved.path()).expect("read the resolved layout");
    let after: KdlDocument = written
        .parse()
        .unwrap_or_else(|e| panic!("the injected layout does not parse: {e}\nargs: {args:?}\n{written}"));

    if !cfg.runs_agent() {
        assert_eq!(written, cfg.layout, "no agent pane, yet injection changed the layout");
        return;
    }

    assert_eq!(commands(&before), commands(&after), "injection changed the set of pane commands");
    assert_eq!(
        other_panes(&before, &agent.command),
        other_panes(&after, &agent.command),
        "injection touched a pane that does not run the agent"
    );
    let old = agent_panes(&before, &agent.command);
    let new = agent_panes(&after, &agent.command);
    assert_eq!(old.len(), 1, "resolve succeeded with {} agent panes", old.len());
    assert_eq!(new.len(), 1, "injection changed the number of agent panes");

    let mut expected = first_args(old[0]);
    expected.extend(args.iter().map(|a| (None, KdlValue::String(a.clone()))));
    assert_eq!(first_args(new[0]), expected, "the agent's args did not read back as old ++ injected");
});

/// Panes whose `command` is `cmd`, as zellij would run them: any commanded pane
/// is a leaf, because zellij 0.43 rejects a pane with both a `command` and nested
/// panes (`kdl_layout_parser.rs`, "Cannot have both properties … and nested
/// children"), so nothing inside one runs.
fn agent_panes<'a>(doc: &'a KdlDocument, cmd: &str) -> Vec<&'a KdlNode> {
    let mut out = Vec::new();
    for node in doc.nodes() {
        match node.get("command") {
            Some(c) if c.value().as_string() == Some(cmd) => out.push(node),
            Some(_) => {}
            None => {
                if let Some(kids) = node.children() {
                    out.extend(agent_panes(kids, cmd));
                }
            }
        }
    }
    out
}

/// Every `command` property anywhere in the document, sorted.
fn commands(doc: &KdlDocument) -> Vec<String> {
    let mut out = Vec::new();
    for node in doc.nodes() {
        if let Some(c) = node.get("command") {
            out.push(c.value().to_string());
        }
        if let Some(kids) = node.children() {
            out.extend(commands(kids));
        }
    }
    out.sort();
    out
}

/// Every commanded pane other than the agent's, as its rendered text, sorted.
fn other_panes(doc: &KdlDocument, agent: &str) -> Vec<String> {
    let mut out = Vec::new();
    for node in doc.nodes() {
        match node.get("command").and_then(|e| e.value().as_string()) {
            Some(c) if c == agent => {}
            Some(_) => out.push(node.to_string()),
            None => {
                if let Some(kids) = node.children() {
                    out.extend(other_panes(kids, agent));
                }
            }
        }
    }
    out.sort();
    out
}

/// The entries of the pane's first `args` child, as (property name, value).
fn first_args(pane: &KdlNode) -> Vec<(Option<String>, KdlValue)> {
    pane.children()
        .and_then(|kids| kids.nodes().iter().find(|n| n.name().value() == "args"))
        .map(|n| n.entries().iter().map(entry).collect())
        .unwrap_or_default()
}

fn entry(e: &KdlEntry) -> (Option<String>, KdlValue) {
    (e.name().map(|n| n.value().to_string()), e.value().clone())
}
