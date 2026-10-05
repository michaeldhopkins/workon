//! How workon reads zellij's answers to `list-sessions` and `delete-session`.
//!
//! Kept apart from the calls that produce them so each outcome (a listing, a
//! hang, a fresh machine, any other failure) can be tested without zellij.

use anyhow::Result;
use vcs_runner::RunError;

/// What `zellij list-sessions --no-formatting` said about one session.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Listing {
    Found(bool),
    /// IPC is hung. It could be this session's server or an unrelated orphan
    /// (zellij IPC blocks globally on a single bad socket), so the caller
    /// recovers only what is bound to its own session and launches fresh.
    Hung,
}

/// Read the stdout (or failure) of `zellij list-sessions --no-formatting`.
pub(crate) fn read_listing(name: &str, result: Result<String, RunError>) -> Result<Listing> {
    match result {
        Ok(stdout) => Ok(Listing::Found(stdout.lines().any(|line| {
            line.split_whitespace()
                .next()
                .is_some_and(|first| first == name)
        }))),
        Err(ref e) if e.is_timeout() => Ok(Listing::Hung),
        // Fresh machine / fully-reaped sessions: zellij exits 1 with this
        // stderr sentinel. Semantically equivalent to "our session does not
        // exist", so the caller launches a new one.
        Err(ref e) if is_no_sessions_error(e) => Ok(Listing::Found(false)),
        Err(e) => Err(e.into()),
    }
}

fn is_no_sessions_error(err: &RunError) -> bool {
    err.stderr()
        .is_some_and(|s| s.contains("No active zellij sessions"))
}

/// Whether `zellij delete-session` wedged and needs the surgical kill. Any
/// other failure typically means "no such session", which needs nothing.
pub(crate) fn delete_hung<T>(result: &Result<T, RunError>) -> bool {
    matches!(result, Err(e) if e.is_timeout())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;
    use vcs_runner::Cmd;

    /// Build a real `RunError::NonZeroExit` by running `sh -c "...>&2; exit 1"`.
    /// `CmdDisplay::new` is crate-private upstream, so direct construction
    /// isn't an option — running a real subprocess is the supported path.
    fn non_zero_exit_with_stderr(stderr: &str) -> RunError {
        let script = format!("printf %s {} 1>&2; exit 1", shell_single_quote(stderr));
        Cmd::new("sh")
            .args(["-c", &script])
            .timeout(Duration::from_secs(5))
            .run()
            .expect_err("expected non-zero exit")
    }

    /// Single-quote a string for POSIX shell. Inputs in this file are static
    /// test fixtures, but using the right quoting keeps the helper reusable.
    fn shell_single_quote(s: &str) -> String {
        let mut out = String::with_capacity(s.len() + 2);
        out.push('\'');
        for ch in s.chars() {
            if ch == '\'' {
                out.push_str("'\\''");
            } else {
                out.push(ch);
            }
        }
        out.push('\'');
        out
    }

    fn timeout_error() -> RunError {
        let err = Cmd::new("sleep")
            .arg("60")
            .timeout(Duration::from_millis(100))
            .run()
            .expect_err("expected timeout");
        assert!(err.is_timeout());
        err
    }

    const LISTING: &str = "\
dev-api [Created 2h ago]
dev [Created 1h ago] (EXITED - attach to resurrect)
";

    #[test]
    fn a_session_is_found_only_by_its_exact_name() {
        let read = |name: &str| read_listing(name, Ok(LISTING.to_string())).unwrap();
        assert_eq!(read("dev"), Listing::Found(true));
        assert_eq!(read("dev-api"), Listing::Found(true));
        assert_eq!(read("de"), Listing::Found(false), "a prefix is not a match");
        assert_eq!(read("other"), Listing::Found(false));
    }

    #[test]
    fn a_hung_listing_is_reported_as_hung() {
        assert_eq!(
            read_listing("dev", Err(timeout_error())).unwrap(),
            Listing::Hung
        );
    }

    #[test]
    fn no_sessions_on_the_machine_means_not_found() {
        let err = non_zero_exit_with_stderr("No active zellij sessions found.");
        assert_eq!(
            read_listing("dev", Err(err)).unwrap(),
            Listing::Found(false)
        );
    }

    #[test]
    fn any_other_listing_failure_is_an_error() {
        let err = non_zero_exit_with_stderr("some other zellij failure");
        let err = read_listing("dev", Err(err)).expect_err("not a listing");
        assert!(
            format!("{err:#}").contains("some other zellij failure"),
            "{err:#}"
        );
    }

    #[test]
    fn only_a_timed_out_delete_needs_recovery() {
        assert!(delete_hung::<()>(&Err(timeout_error())));
        assert!(!delete_hung::<()>(&Err(non_zero_exit_with_stderr(
            "no such session"
        ))));
        assert!(!delete_hung(&Ok(())));
    }

    #[test]
    fn no_sessions_error_recognized_on_fresh_machine() {
        // The exact stderr zellij emits when no sessions exist on the host.
        // Without this classifier, workon's first run on a clean machine
        // aborts. See specs/no-active-sessions-bug.md.
        let err = non_zero_exit_with_stderr("No active zellij sessions found.");
        assert!(is_no_sessions_error(&err));
    }

    #[test]
    fn no_sessions_error_tolerates_punctuation_drift() {
        let err = non_zero_exit_with_stderr("No active zellij sessions found");
        assert!(is_no_sessions_error(&err));
    }

    #[test]
    fn unrelated_non_zero_exit_is_not_no_sessions() {
        let err = non_zero_exit_with_stderr("some other zellij failure");
        assert!(!is_no_sessions_error(&err));
    }

    #[test]
    fn timeout_error_is_not_no_sessions() {
        assert!(!is_no_sessions_error(&timeout_error()));
    }

    #[test]
    fn spawn_error_is_not_no_sessions() {
        let err = Cmd::new("definitely-not-a-real-binary-zxqv-9001")
            .timeout(Duration::from_secs(5))
            .run()
            .expect_err("expected spawn failure");
        assert!(err.is_spawn_failure());
        assert!(!is_no_sessions_error(&err));
    }
}
