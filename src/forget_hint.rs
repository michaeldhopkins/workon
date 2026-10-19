//! What `destroy` says when it can't tell which jj workspace a directory is: why it stopped,
//! and the `jj workspace forget` commands that get the user unstuck, one per line.

use std::path::Path;

pub fn inexact_root(path: &Path, names: &[&str]) -> String {
    format!(
        "can't safely delete {path}: jj reports this workspace's path differently from how it appears on disk, \
         usually because the folder name isn't valid UTF-8. To remove it yourself:\n{}\n  then delete {path}",
        forget_lines(names),
        path = path.display(),
    )
}

pub fn several_names(path: &Path, names: &[&str]) -> String {
    let mut sorted = names.to_vec();
    sorted.sort_unstable();
    format!(
        "{} is registered as more than one jj workspace ({}). Forget the one you don't want, then run destroy again:\n{}",
        path.display(),
        sorted.join(", "),
        forget_lines(&sorted),
    )
}

fn forget_lines(names: &[&str]) -> String {
    let mut names = names.to_vec();
    names.sort_unstable();
    names.iter().map(|name| format!("  jj workspace forget {}", shell_word(name))).collect::<Vec<_>>().join("\n")
}

/// Quoted only when a shell would otherwise split or expand it, so the common name stays readable.
fn shell_word(word: &str) -> String {
    if !word.is_empty() && word.chars().all(|c| c.is_ascii_alphanumeric() || "._-/".contains(c)) {
        word.to_string()
    } else {
        format!("'{}'", word.replace('\'', r"'\''"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_a_shell_would_split_are_quoted() {
        assert_eq!(shell_word("feature-1"), "feature-1");
        assert_eq!(shell_word("two words"), "'two words'");
        assert_eq!(shell_word("it's"), r"'it'\''s'");
        assert_eq!(shell_word(""), "''");
        assert_eq!(forget_lines(&["b", "a"]), "  jj workspace forget a\n  jj workspace forget b");
    }

    proptest::proptest! {
        #[test]
        fn one_sorted_forget_line_per_name(names in proptest::collection::vec("[a-z' ]{0,6}", 1..5)) {
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let lines = forget_lines(&refs);
            let got: Vec<&str> = lines.lines().collect();
            proptest::prop_assert_eq!(got.len(), names.len());
            let mut sorted = refs.clone();
            sorted.sort_unstable();
            for (line, name) in got.iter().zip(&sorted) {
                proptest::prop_assert_eq!(*line, format!("  jj workspace forget {}", shell_word(name)));
            }
        }

        #[test]
        fn a_quoted_word_unquotes_to_the_original(word in "[ -~]{0,12}") {
            let quoted = shell_word(&word);
            let unquoted = if quoted.starts_with('\'') {
                quoted[1..quoted.len() - 1].replace(r"'\''", "'")
            } else {
                quoted.clone()
            };
            proptest::prop_assert_eq!(unquoted, word);
        }
    }
}
