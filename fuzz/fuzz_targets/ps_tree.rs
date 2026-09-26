//! `ps -A -o pid=,ppid=,comm=` output, and which commands run under a zellij
//! server. workon refuses to attach to a session whose process tree lacks the
//! requested layout's focused command, so a wrong answer either blocks a good
//! attach or lets a mismatched one through.
//!
//! The input builds a process table rather than being parsed as text: three bytes
//! per process pick its parent (any process, or launchd), its name and the form
//! `comm` takes. The forms are the ones captured from real `ps` on macOS and
//! Linux (2026-09-26): a bare name, a full path, a login shell's `-name`, macOS's
//! `-/opt/homebrew/bin/zsh`, and an app bundle path with spaces in it. Parents
//! may form a cycle, which real `ps` cannot print but a racy snapshot could.
//!
//! Asserts the parser returns exactly the names of the processes reachable from
//! the root, the root itself excluded.
#![no_main]

use std::collections::HashSet;

use libfuzzer_sys::fuzz_target;

/// Names with no `/` and no leading `-`: the parser takes the text after the last
/// `/` and strips a login shell's dash, so a name with either cannot be written
/// in a form that reads back as itself.
const NAMES: &[&str] = &[
    "claude", "zsh", "branchdiff", "zellij", "node", "opencode", "bash", "Google Chrome Helper", "mcp@latest", "a-b",
];

fn comm(name: &str, form: u8) -> String {
    match form % 5 {
        0 => name.to_string(),
        1 => format!("/usr/bin/{name}"),
        2 => format!("-{name}"),
        3 => format!("-/opt/homebrew/bin/{name}"),
        _ => format!("/Applications/Some App.app/Contents/MacOS/{name}"),
    }
}

fuzz_target!(|data: &[u8]| {
    let Some((&head, rest)) = data.split_first() else { return };
    let procs: Vec<(usize, &str, u8)> = rest
        .chunks_exact(3)
        .map(|c| (c[0] as usize, NAMES[c[1] as usize % NAMES.len()], c[2]))
        .collect();
    if procs.is_empty() {
        return;
    }
    let n = procs.len();
    // Pids of varied width, unique, not in pid order relative to parents.
    let pid = |i: usize| 2 + (i * 7919) % 99_991 + i * 100_000;
    // Parent selector n means launchd (pid 1), outside the table.
    let parent = |i: usize| match procs[i].0 % (n + 1) {
        p if p == n => 1,
        p => pid(p),
    };

    let mut lines = vec![format!("{:>5} {:>5} /sbin/launchd", 1, 0)];
    for (i, &(_, name, form)) in procs.iter().enumerate() {
        lines.push(format!("{:>5} {:>5} {}", pid(i), parent(i), comm(name, form)));
    }
    if head & 1 == 1 {
        lines.reverse();
    }
    let root = pid((head >> 1) as usize % n);

    let mut expected = HashSet::new();
    let mut seen = HashSet::from([root]);
    let mut frontier = vec![root];
    while let Some(p) = frontier.pop() {
        for i in 0..n {
            if parent(i) == p && seen.insert(pid(i)) {
                expected.insert(procs[i].1.to_string());
                frontier.push(pid(i));
            }
        }
    }

    let found = workon::fuzz_api::parse_descendants(&lines.join("\n"), root as u32);
    assert_eq!(found, expected, "root {root}\n{}", lines.join("\n"));
});
