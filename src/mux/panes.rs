//! Layout leaves, in the order a multiplexer creates them.
//!
//! A pane with a `command` is a leaf: zellij rejects a pane that also has
//! nested panes, so nothing inside one runs. A pane whose children include
//! panes is a container. Anything else that is a pane is a shell.

use anyhow::Result;
use kdl::KdlNode;

use crate::layout;

/// One pane a multiplexer can start.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Leaf {
    pub program: Option<String>,
    pub args: Vec<String>,
    pub focus: bool,
}

impl Leaf {
    pub fn shell() -> Self {
        Self { program: None, args: Vec::new(), focus: false }
    }
}

/// The panes of `layout`, in document order.
///
/// `layout` is the KDL workon already parsed and wrote back out. A document
/// that does not parse is an error. The panic guard lives in [`layout::parse_kdl`].
pub fn leaves(layout_src: &str) -> Result<Vec<Leaf>> {
    let doc = layout::parse_kdl(layout_src)?;
    let mut out = Vec::new();
    walk(doc.nodes(), &mut out);
    Ok(out)
}

fn walk(nodes: &[KdlNode], out: &mut Vec<Leaf>) {
    for node in nodes {
        if node.name().value() == "pane"
            && let Some(leaf) = leaf_of(node)
        {
            out.push(leaf);
            continue;
        }
        if let Some(kids) = node.children() {
            walk(kids.nodes(), out);
        }
    }
}

/// `None` when this pane is only a container for other panes.
fn leaf_of(node: &KdlNode) -> Option<Leaf> {
    let has_command = node.get("command").is_some();
    let has_child_pane = node.children().is_some_and(|doc| doc.nodes().iter().any(|n| n.name().value() == "pane"));
    if !has_command && has_child_pane {
        return None;
    }
    let program = node
        .get("command")
        .and_then(|entry| entry.value().as_string())
        .filter(|cmd| !cmd.is_empty())
        .map(str::to_string);
    Some(Leaf { program, args: args_of(node), focus: is_focused(node) })
}

fn is_focused(node: &KdlNode) -> bool {
    node.get("focus").and_then(|entry| entry.value().as_bool()).unwrap_or(false)
}

fn args_of(node: &KdlNode) -> Vec<String> {
    let Some(kids) = node.children() else { return Vec::new() };
    let Some(args) = kids.nodes().iter().find(|n| n.name().value() == "args") else {
        return Vec::new();
    };
    args.entries()
        .iter()
        .filter(|entry| entry.name().is_none())
        .filter_map(|entry| entry.value().as_string().map(str::to_string))
        .collect()
}

/// Index of the focused leaf, or `0` when none is marked.
pub fn focused_index(leaves: &[Leaf]) -> usize {
    leaves.iter().position(|leaf| leaf.focus).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nested_layout_keeps_order_focus_and_args() {
        let src = r#"
            layout {
                pane split_direction="vertical" {
                    pane command="claude" focus=true
                    pane size="20%"
                    pane command="branchdiff" {
                        args "--watch"
                    }
                }
            }
        "#;
        let got = leaves(src).unwrap();
        assert_eq!(got.len(), 3);
        assert_eq!(got[0].program.as_deref(), Some("claude"));
        assert!(got[0].focus);
        assert_eq!(got[1], Leaf::shell());
        assert_eq!(got[2].program.as_deref(), Some("branchdiff"));
        assert_eq!(got[2].args, vec!["--watch".to_string()]);
        assert_eq!(focused_index(&got), 0);
    }

    #[test]
    fn a_commanded_pane_is_a_leaf_even_with_a_nested_pane() {
        let src = "pane command=\"vim\" {\n    pane command=\"claude\"\n}\n";
        let got = leaves(src).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].program.as_deref(), Some("vim"));
    }

    #[test]
    fn an_empty_layout_has_no_leaves() {
        assert!(leaves("layout { }").unwrap().is_empty());
    }

    proptest::proptest! {
        /// Commanded panes read back in order, including a focused one that is not first.
        #[test]
        fn commanded_panes_read_back_in_order(cmds in proptest::collection::vec("[a-z]{1,6}", 1..5usize)) {
            let mut src = String::from("layout {\n");
            for (i, cmd) in cmds.iter().enumerate() {
                if i == cmds.len() - 1 {
                    src.push_str(&format!("  pane command=\"{cmd}\" focus=true\n"));
                } else {
                    src.push_str(&format!("  pane command=\"{cmd}\"\n"));
                }
            }
            src.push_str("}\n");
            let got = leaves(&src).unwrap();
            let programs: Vec<String> = got.iter().map(|leaf| leaf.program.clone().unwrap()).collect();
            proptest::prop_assert_eq!(programs, cmds.clone());
            proptest::prop_assert_eq!(focused_index(&got), cmds.len() - 1);
            proptest::prop_assert!(got.last().unwrap().focus);
        }
    }
}
