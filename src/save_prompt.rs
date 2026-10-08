//! Teardown's question about unsaved work, and its default-yes reading of the answer.

use std::io::Write;

use anyhow::Result;

/// How teardown decides whether to rescue unsaved work. `Prompt` is the
/// interactive `[Y/n]` (ephemeral quit); `Save`/`NoSave` are the non-interactive
/// `destroy` choices, since a headless run can't block on stdin.
pub(crate) enum SaveMode {
    Prompt,
    Save,
    NoSave,
}

/// Default-yes answer: empty input (bare Enter, or EOF from a closed
/// session) and any `y`/`yes` mean yes; only an explicit `n`/`no`/other
/// declines.
pub(crate) fn is_affirmative(answer: &str) -> bool {
    let a = answer.trim();
    a.is_empty() || a.eq_ignore_ascii_case("y") || a.eq_ignore_ascii_case("yes")
}

/// Decide whether teardown saves the unsaved work it found. `Save`/`NoSave` are
/// non-interactive; `Prompt` asks on stderr and reads stdin (default yes).
pub(crate) fn should_save(save: &SaveMode, ws_id: &str) -> Result<bool> {
    match save {
        SaveMode::Save => Ok(true),
        SaveMode::NoSave => Ok(false),
        SaveMode::Prompt => {
            // Default yes: the prompt only fires for work that would otherwise
            // be lost, so preserving is almost always what you want. Empty/EOF
            // (you closed the session without answering) counts as yes.
            eprint!("Save under workon/{ws_id}? [Y/n] ");
            std::io::stderr().flush()?;
            let mut answer = String::new();
            std::io::stdin().read_line(&mut answer)?;
            Ok(is_affirmative(&answer))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_affirmative_defaults_to_yes() {
        // Bare Enter and EOF (closed session) both arrive as empty -> save.
        assert!(is_affirmative(""));
        assert!(is_affirmative("\n"));
        assert!(is_affirmative("y"));
        assert!(is_affirmative("Y\n"));
        assert!(is_affirmative("yes"));
    }

    #[test]
    fn is_affirmative_explicit_no_declines() {
        assert!(!is_affirmative("n"));
        assert!(!is_affirmative("N\n"));
        assert!(!is_affirmative("no"));
        assert!(!is_affirmative("nope"));
    }

    #[test]
    fn non_interactive_modes_never_read_stdin() {
        assert!(should_save(&SaveMode::Save, "ws-1").unwrap());
        assert!(!should_save(&SaveMode::NoSave, "ws-1").unwrap());
    }
}
