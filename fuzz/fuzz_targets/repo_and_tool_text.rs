//! Text workon reads but did not write: files in the repo being opened
//! (`database.yml`, `schema.prisma`, `phpunit.xml`, `alembic.ini`, `mix.exs`, a
//! `.csproj` tag), stdout of `jj log -T bookmarks` and `<tool> --version`, and a
//! layout's lines.
//!
//! Mostly never-panics. Where a result feeds something else, it also asserts the
//! shape that consumer relies on:
//!
//! - the `mix.exs` app name is a non-empty identifier (it becomes part of a
//!   database name);
//! - the trunk bookmark is one whitespace-free word of the input, without jj's
//!   `*`/`?` markers and never an `@git` ref (it becomes a revset);
//! - every layout command is non-empty, quote-free and listed once (each is looked
//!   up on `PATH`).
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let text = String::from_utf8_lossy(data);
    let parsed = workon::fuzz_api::parse_all(&text);
    // Raw `ps` text: must terminate and not panic. `ps_tree` checks the answer.
    let _ = workon::fuzz_api::parse_descendants(&text, 1);

    if let Some(app) = &parsed.app_name {
        assert!(!app.is_empty() && app.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'), "app name {app:?}");
    }

    let b = &parsed.first_bookmark;
    if !b.is_empty() {
        assert!(text.split_whitespace().any(|w| w.starts_with(b.as_str())), "bookmark {b:?} is not from the input");
        assert!(!b.chars().any(char::is_whitespace), "bookmark {b:?} has whitespace");
        assert!(!b.ends_with('*') && !b.ends_with('?') && !b.ends_with("@git"), "bookmark {b:?}");
    }

    for (i, c) in parsed.commands.iter().enumerate() {
        assert!(!c.is_empty() && !c.contains('"'), "command {c:?}");
        assert!(!parsed.commands[..i].contains(c), "command {c:?} listed twice");
    }
});
