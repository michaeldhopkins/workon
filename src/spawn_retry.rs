//! A retry for a spawn whose executable is briefly open for writing.
//!
//! Linux refuses to exec a file any process still holds a write descriptor on
//! — ETXTBSY, "Text file busy". A binary replaced under us is busy for a
//! moment only, so such a spawn is retried a few times with a short sleep
//! before the failure is allowed to stand; every other spawn failure stands as
//! it comes. The same race strikes a test that writes a stand-in script and
//! runs it at once — a fork of the test process while the script was open for
//! writing inherits the descriptor — which is why the spawn paths a test
//! drives carry this retry.

use std::io;

use vcs_runner::{Cmd, RunError};

/// `cmd` with a retry on a briefly busy executable: the default policy's few
/// short backoffs, an ETXTBSY spawn alone.
pub(crate) fn retry_busy_executable(cmd: Cmd) -> Cmd {
    cmd.retry_when(|err| matches!(err, RunError::Spawn { source, .. } if busy_executable(source)))
}

/// Whether the spawn failed because the executable is open for writing
/// somewhere. The kind is the portable reading; 26 is ETXTBSY's errno on Linux
/// and macOS alike, kept for an error that carries its errno past a mapping.
fn busy_executable(source: &io::Error) -> bool {
    source.kind() == io::ErrorKind::ExecutableFileBusy || source.raw_os_error() == Some(26)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_busy_kind_without_an_errno_is_still_busy() {
        assert!(busy_executable(&io::Error::from(io::ErrorKind::ExecutableFileBusy)));
        assert!(!busy_executable(&io::Error::from(io::ErrorKind::NotFound)));
    }

    proptest::proptest! {
        /// Only ETXTBSY — errno 26 on Linux and macOS alike — reads as a busy
        /// executable; every other errno is some other spawn failure.
        #[test]
        fn only_etxtbusy_reads_as_a_busy_executable(errno in 0i32..133) {
            proptest::prop_assert_eq!(busy_executable(&io::Error::from_raw_os_error(errno)), errno == 26);
        }
    }
}
