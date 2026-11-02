//! Whether one workspace's stack holds a divergent change.
//!
//! The stack is `trunk()..<workspace>@` plus the working-copy commit. Divergence
//! is by change id, so one copy in the stack counts when the other copy sits
//! elsewhere. A jj error reads as `active`. Reads pass `--ignore-working-copy`.

use std::collections::BTreeSet;
use std::path::Path;

use vcs_runner::{jj_divergent_change_ids, run_jj_utf8_ignore_wc};

/// A listed workspace is scoped to its own stack. One jj does not list by name has no stack to
/// scope by, so it reads the whole repo.
pub(crate) fn listed_status(project_dir: &Path, listed: Option<&str>) -> &'static str {
    match listed {
        Some(name) => workspace_status(project_dir, name),
        None => repo_status(project_dir),
    }
}

/// `"divergent"` when any change in the repo is divergent. A jj error reads as `active`.
fn repo_status(project_dir: &Path) -> &'static str {
    if project_dir.join(".jj").is_dir() && jj_divergent_change_ids(project_dir).is_ok_and(|ids| !ids.is_empty()) {
        "divergent"
    } else {
        "active"
    }
}

/// `"divergent"` when `workspace`'s stack holds a divergent change, otherwise `"active"`.
pub(crate) fn workspace_status(project_dir: &Path, workspace: &str) -> &'static str {
    // `.jj` marks the repo root. Without that check, jj walks up to a parent
    // repo and reports that repo's divergence for a directory that is not one.
    if !project_dir.join(".jj").is_dir() {
        return "active";
    }
    match (stack_change_ids(project_dir, workspace), jj_divergent_change_ids(project_dir)) {
        (Some(stack), Ok(divergent)) if stack_touches_divergence(&stack, &divergent) => "divergent",
        _ => "active",
    }
}

fn stack_change_ids(project_dir: &Path, workspace: &str) -> Option<BTreeSet<String>> {
    let revset = stack_revset(workspace);
    let out =
        run_jj_utf8_ignore_wc(project_dir, &["log", "--no-graph", "-r", &revset, "-T", r#"change_id ++ "\n""#]).ok()?;
    Some(out.lines().map(String::from).collect())
}

/// `trunk()..<workspace>@` plus the working-copy commit.
///
/// `x..y` leaves `y` out when `y` is an ancestor of `x`, so a workspace checked
/// out on trunk is in the stack only through the second term.
fn stack_revset(workspace: &str) -> String {
    let quoted = quote_symbol(workspace);
    format!("(trunk()..{quoted}@) | {quoted}@")
}

/// A jj double-quoted symbol. `\` and `"` are escaped.
fn quote_symbol(name: &str) -> String {
    let mut out = String::from("\"");
    for c in name.chars() {
        if c == '\\' || c == '"' {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    out
}

/// True when any divergent change id is also in the stack.
fn stack_touches_divergence(stack: &BTreeSet<String>, divergent: &[String]) -> bool {
    divergent.iter().any(|id| stack.contains(id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stack_revset_quotes_the_name_and_keeps_the_working_copy() {
        assert_eq!(stack_revset("ws-fresh"), r#"(trunk().."ws-fresh"@) | "ws-fresh"@"#);
        assert_eq!(quote_symbol("a\"b\\c"), r#""a\"b\\c""#);
    }

    /// Jj's string literal, decoded. An unknown escape is refused, so a quote
    /// or backslash that was not escaped does not round-trip.
    fn unquote_symbol(quoted: &str) -> Option<String> {
        let body = quoted.strip_prefix('"')?.strip_suffix('"')?;
        let mut out = String::new();
        let mut chars = body.chars();
        while let Some(c) = chars.next() {
            if c == '\\' {
                match chars.next()? {
                    '\\' => out.push('\\'),
                    '"' => out.push('"'),
                    _ => return None,
                }
            } else if c == '"' {
                return None;
            } else {
                out.push(c);
            }
        }
        Some(out)
    }

    proptest::proptest! {
        #[test]
        fn divergence_outside_the_stack_leaves_the_workspace_active(
            stack in proptest::collection::btree_set("[0-9a-f]{8}", 0..8),
            outside in proptest::collection::vec("[0-9a-f]{8}", 0..8),
        ) {
            let outside: Vec<String> = outside.into_iter().filter(|id| !stack.contains(id)).collect();
            proptest::prop_assert!(!stack_touches_divergence(&stack, &outside));
        }

        #[test]
        fn one_stack_change_id_among_divergent_ids_is_enough(
            stack in proptest::collection::btree_set("[0-9a-f]{8}", 1..8),
            mut divergent in proptest::collection::vec("[0-9a-f]{8}", 0..8),
        ) {
            divergent.retain(|id| !stack.contains(id));
            divergent.push("not-in-the-stack".to_string());
            proptest::prop_assert!(!stack_touches_divergence(&stack, &divergent));
            let shared = stack.iter().next().expect("the strategy is non-empty").clone();
            divergent.push(shared);
            proptest::prop_assert!(stack_touches_divergence(&stack, &divergent));
        }

        #[test]
        fn a_workspace_name_round_trips_through_the_revset_string(name in "[ -~]{0,24}") {
            let quoted = quote_symbol(&name);
            let revset = stack_revset(&name);
            proptest::prop_assert_eq!(revset, format!("(trunk()..{quoted}@) | {quoted}@"));
            proptest::prop_assert_eq!(unquote_symbol(&quoted), Some(name));
        }
    }
}
