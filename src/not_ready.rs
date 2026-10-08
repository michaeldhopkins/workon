//! What workon says when a setup step failed (issue #2): a test database that could not be
//! created, or a schema load or migration that errors, does not stop provisioning, so the
//! workspace exists but is not ready. `create` says so last, names the command that opens the
//! workspace as it is, and exits with [`EXIT_NOT_READY`]; `-w` asks before zellij takes the
//! screen, which would hide the warning.
//!
//! There is no flag to skip a step: a failed step leaves the workspace exactly as skipping it
//! would, so the way past a failure is to open the workspace without setting it up again
//! (`workon attach`, or answering yes in `-w`).

use std::io::{BufRead, Write};

/// Exit status when a setup step failed and the workspace was not opened: `create` kept it, or
/// `-w` was told not to open it and removed it. Distinct from 1, an error, and 2, clap's usage
/// error.
pub const EXIT_NOT_READY: u8 = 3;

/// The warning naming every failed step, or `None` when nothing failed. Each failure's own
/// output was printed when it happened.
pub fn note(failed_steps: &[String]) -> Option<String> {
    (!failed_steps.is_empty())
        .then(|| format!("Warning: the workspace is not ready; failed (output above): {}", failed_steps.join(", ")))
}

/// The lines `workon create` ends with when a step failed: the warning, then the command that
/// opens the workspace without running setup again. Empty when nothing failed.
pub fn create_lines(ws_id: &str, failed_steps: &[String]) -> Vec<String> {
    note(failed_steps).map(|n| vec![n, format!("Open it anyway with: workon attach {ws_id}")]).unwrap_or_default()
}

/// The `-w` pause: when a step failed, print the warning and ask whether to open the session
/// anyway. Only `n` or `no` declines, since declining removes the workspace; Enter, end of
/// input, a typo, or a failure to write or read opens it. Nothing is asked when nothing failed.
pub fn ask_open_anyway(failed_steps: &[String], input: &mut dyn BufRead, err: &mut dyn Write) -> bool {
    let Some(note) = note(failed_steps) else {
        return true;
    };
    let mut answer = String::new();
    let asked = writeln!(err, "{note}")
        .and_then(|()| write!(err, "Open it anyway? [Y/n] "))
        .and_then(|()| err.flush())
        .and_then(|()| input.read_line(&mut answer));
    asked.is_err() || !declines(&answer)
}

/// `n` or `no`, in any case, around whitespace.
fn declines(answer: &str) -> bool {
    let a = answer.trim();
    a.eq_ignore_ascii_case("n") || a.eq_ignore_ascii_case("no")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn steps(names: &[&str]) -> Vec<String> {
        names.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn note_names_every_failed_step() {
        assert_eq!(note(&[]), None);
        assert_eq!(
            note(&steps(&["prisma schema apply", "prisma generate"])).as_deref(),
            Some("Warning: the workspace is not ready; failed (output above): prisma schema apply, prisma generate")
        );
    }

    #[test]
    fn create_ends_with_the_warning_then_the_command_that_opens_it() {
        assert!(create_lines("ws-abc123", &[]).is_empty());
        assert_eq!(
            create_lines("ws-abc123", &steps(&["rails db:schema:load"])),
            [
                "Warning: the workspace is not ready; failed (output above): rails db:schema:load",
                "Open it anyway with: workon attach ws-abc123",
            ]
        );
    }

    fn ask(failed: &[&str], typed: &str) -> (bool, String) {
        let mut err = Vec::new();
        let open = ask_open_anyway(&steps(failed), &mut typed.as_bytes(), &mut err);
        (open, String::from_utf8(err).unwrap())
    }

    #[test]
    fn nothing_failed_opens_without_asking_or_reading() {
        assert_eq!(ask(&[], "n\n"), (true, String::new()));
    }

    #[test]
    fn a_failed_step_asks_and_only_n_or_no_declines() {
        let prompt = "Warning: the workspace is not ready; failed (output above): migrate\nOpen it anyway? [Y/n] ";
        for (typed, open) in [
            ("\n", true),
            ("", true),
            ("y\n", true),
            ("yy\n", true),
            ("ues\n", true),
            ("n\n", false),
            (" N \n", false),
            ("no\n", false),
            ("NO\n", false),
        ] {
            assert_eq!(ask(&["migrate"], typed), (open, prompt.to_string()), "{typed:?}");
        }
    }

    /// Input that cannot be read is treated as end of input: the session opens.
    #[test]
    fn an_unreadable_answer_opens_the_session() {
        struct Broken;
        impl std::io::Read for Broken {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("closed"))
            }
        }
        let mut input = std::io::BufReader::new(Broken);
        assert!(ask_open_anyway(&steps(&["migrate"]), &mut input, &mut Vec::new()));
    }

    proptest::proptest! {
        /// The pause opens the session for every answer but `n` or `no` (any case, any
        /// surrounding whitespace), and always shows the same prompt first.
        #[test]
        fn only_n_or_no_keeps_the_session_closed(word in "(n|N|no|No|nO|NO|y|yes|nn|on|non|[a-z]{0,3})", pad in "[ \t]{0,2}") {
            let typed = format!("{pad}{word}{pad}\n");
            let (open, shown) = ask(&["migrate"], &typed);
            proptest::prop_assert_eq!(open, !["n", "no"].contains(&word.to_lowercase().as_str()));
            proptest::prop_assert!(shown.ends_with("Open it anyway? [Y/n] "));
        }
    }
}
