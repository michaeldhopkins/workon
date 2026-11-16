//! Database URLs from the dotenv files Rails loads before `database.yml`.
//!
//! dotenv-rails 3.2 loads, with `overwrite: false`, `.env.<env>.local`, then
//! `.env.local` (except in test), then `.env.<env>`, then `.env`. The first
//! value wins, and a variable already in the environment wins over every file.
//! Development and test are resolved separately, because `db:test:purge` boots
//! test. A file that cannot be read or parsed is a fault, not an empty file:
//! treating it as "no URL" would leave the primary checkout's database in place.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use super::rails_yml;

const DEVELOPMENT: &[&str] = &[".env.development.local", ".env.local", ".env.development", ".env"];
const TEST: &[&str] = &[".env.test.local", ".env.test", ".env"];
const FILES: &[&str] =
    &[".env.development.local", ".env.local", ".env.development", ".env.test.local", ".env.test", ".env"];

#[derive(Debug)]
pub(crate) struct DotenvFault {
    pub path: PathBuf,
    detail: &'static str,
}

impl DotenvFault {
    pub(crate) fn reason(&self) -> &'static str {
        self.detail
    }
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ParseFault {
    pub file: String,
    detail: &'static str,
}

impl ParseFault {
    fn new(file: &str, detail: &'static str) -> Self {
        Self { file: file.to_string(), detail }
    }

    pub(crate) fn reason(&self) -> &'static str {
        self.detail
    }
}

/// Database URLs the workspace's development and test boots take from dotenv
/// files. Keys already in `env` are omitted; the caller already has them.
pub(crate) fn database_urls(root: &Path, env: &HashMap<String, String>) -> Result<Vec<(String, String)>, DotenvFault> {
    let mut files = HashMap::new();
    for name in FILES {
        if let Some(text) = read_named(root, name)? {
            files.insert((*name).to_string(), text);
        }
    }
    database_urls_in(&files, env).map_err(|fault| {
        let detail = fault.reason();
        DotenvFault { path: root.join(fault.file), detail }
    })
}

/// `database_urls` for files already in memory. `files` holds only the files
/// that exist.
pub(crate) fn database_urls_in(
    files: &HashMap<String, String>,
    env: &HashMap<String, String>,
) -> Result<Vec<(String, String)>, ParseFault> {
    let mut urls = Vec::new();
    for order in [DEVELOPMENT, TEST] {
        let mut bound = env.clone();
        for name in order {
            let Some(text) = files.get(*name) else { continue };
            for (key, value) in parse_file(text, &bound).map_err(|detail| ParseFault::new(name, detail))? {
                if bound.contains_key(&key) {
                    continue;
                }
                if rails_yml::is_database_url_key(&key) {
                    urls.push((key.clone(), value.clone()));
                }
                bound.insert(key, value);
            }
        }
    }
    Ok(urls)
}

/// Parse one file the way the fuzzer throws text at every other repo parser.
#[cfg(fuzzing)]
pub(crate) fn parse_for_fuzz(text: &str) {
    let _ = parse_file(text, &HashMap::new());
}

fn read_named(root: &Path, name: &str) -> Result<Option<String>, DotenvFault> {
    let path = root.join(name);
    match std::fs::read(&path) {
        Ok(bytes) => decode(&path, &bytes),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) if err.kind() == std::io::ErrorKind::IsADirectory => Ok(None),
        Err(_) => Err(DotenvFault { path, detail: "could not be read" }),
    }
}

fn decode(path: &Path, bytes: &[u8]) -> Result<Option<String>, DotenvFault> {
    let bytes = bytes.strip_prefix(b"\xEF\xBB\xBF").unwrap_or(bytes);
    match String::from_utf8(bytes.to_vec()) {
        Ok(text) => Ok(Some(text)),
        Err(_) => Err(DotenvFault { path: path.to_path_buf(), detail: "not utf-8" }),
    }
}

enum Item {
    Blank,
    ExportUnset(String),
    Set { key: String, raw: String, quote: Quote },
}

enum Quote {
    None,
    Single,
    Double,
}

fn parse_file(text: &str, bound: &HashMap<String, String>) -> Result<Vec<(String, String)>, &'static str> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let text = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut local: HashMap<String, String> = HashMap::new();
    for line in text.lines() {
        match classify(line)? {
            Item::Blank => {}
            Item::ExportUnset(key) => {
                if !local.contains_key(&key) && !bound.contains_key(&key) {
                    return Err("cannot parse");
                }
            }
            Item::Set { key, raw, quote } => {
                if let Some(existing) = bound.get(&key) {
                    local.insert(key, existing.clone());
                    continue;
                }
                local.insert(key, interpret(&raw, quote, &local, bound)?);
            }
        }
    }
    Ok(local.into_iter().filter(|(key, _)| !bound.contains_key(key)).collect())
}

fn classify(line: &str) -> Result<Item, &'static str> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with('#') {
        return Ok(Item::Blank);
    }
    let rest = line.trim_start();
    let (export, rest) = strip_export(rest);
    let (key, rest) = take_key(rest).ok_or("cannot parse")?;
    let rest = rest.trim_start();
    if rest.is_empty() {
        return if export {
            Ok(Item::ExportUnset(key))
        } else {
            Ok(Item::Set { key, raw: String::new(), quote: Quote::None })
        };
    }
    let rest = strip_separator(rest).ok_or("cannot parse")?;
    let (quote, raw, after) = take_value(rest)?;
    if after.trim().is_empty() || after.trim_start().starts_with('#') {
        Ok(Item::Set { key, raw, quote })
    } else {
        Err("cannot parse")
    }
}

fn strip_export(rest: &str) -> (bool, &str) {
    let Some(after) = rest.strip_prefix("export") else {
        return (false, rest);
    };
    if after.is_empty() || after.starts_with(char::is_whitespace) {
        (true, after.trim_start())
    } else {
        (false, rest)
    }
}

fn take_key(rest: &str) -> Option<(String, &str)> {
    let len = rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_' || c == '.')).unwrap_or(rest.len());
    (len > 0).then(|| (rest[..len].to_string(), &rest[len..]))
}

fn strip_separator(rest: &str) -> Option<&str> {
    let rest = rest.trim_start();
    if let Some(after) = rest.strip_prefix('=') {
        return Some(after.trim_start());
    }
    let after = rest.strip_prefix(':')?;
    after.starts_with(char::is_whitespace).then(|| after.trim_start())
}

fn take_value(rest: &str) -> Result<(Quote, String, &str), &'static str> {
    if rest.is_empty() {
        return Ok((Quote::None, String::new(), ""));
    }
    let bytes = rest.as_bytes();
    let quote = bytes[0];
    if quote == b'\'' || quote == b'"' {
        return quoted_value(rest, quote);
    }
    let end = rest.find('#').unwrap_or(rest.len());
    Ok((Quote::None, rest[..end].trim_end().to_string(), &rest[end..]))
}

fn quoted_value(rest: &str, quote: u8) -> Result<(Quote, String, &str), &'static str> {
    let body = &rest[1..];
    let bytes = body.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'\\' && i + 1 < bytes.len() && bytes[i + 1] == quote {
            i += 2;
            continue;
        }
        if bytes[i] == quote {
            let raw = body[..i].to_string();
            let kind = if quote == b'\'' { Quote::Single } else { Quote::Double };
            return Ok((kind, raw, &body[i + 1..]));
        }
        i += 1;
    }
    Err("cannot parse")
}

fn interpret(
    raw: &str,
    quote: Quote,
    local: &HashMap<String, String>,
    bound: &HashMap<String, String>,
) -> Result<String, &'static str> {
    match quote {
        Quote::Single => Ok(raw.to_string()),
        Quote::Double | Quote::None => substitute(&unescape(raw), local, bound),
    }
}

fn unescape(raw: &str) -> String {
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] != '$' {
            out.push(chars[i + 1]);
            i += 2;
        } else {
            out.push(chars[i]);
            i += 1;
        }
    }
    out
}

fn substitute(
    raw: &str,
    local: &HashMap<String, String>,
    bound: &HashMap<String, String>,
) -> Result<String, &'static str> {
    if raw.contains("$(") || raw.contains('`') {
        return Err("command substitution");
    }
    let chars: Vec<char> = raw.chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == '\\' && i + 1 < chars.len() && chars[i + 1] == '$' {
            out.push('$');
            i += 2;
            continue;
        }
        if chars[i] != '$' {
            out.push(chars[i]);
            i += 1;
            continue;
        }
        let (name, next) = read_name(&chars, i + 1);
        if name.is_empty() {
            out.push('$');
            i += 1;
            continue;
        }
        if let Some(value) = local.get(&name).or_else(|| bound.get(&name)) {
            out.push_str(value);
        }
        i = next;
    }
    Ok(out)
}

fn read_name(chars: &[char], start: usize) -> (String, usize) {
    let braced = chars.get(start) == Some(&'{');
    let name_at = if braced { start + 1 } else { start };
    let mut end = name_at;
    while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
        end += 1;
    }
    if braced && chars.get(end) != Some(&'}') {
        return (String::new(), start);
    }
    let name: String = chars[name_at..end].iter().collect();
    let next = if braced { end + 1 } else { end };
    (name, next)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(name, text)| ((*name).to_string(), (*text).to_string())).collect()
    }

    fn urls(pairs: &[(&str, &str)]) -> Vec<String> {
        database_urls_in(&files(pairs), &HashMap::new()).unwrap().into_iter().map(|(_, url)| url).collect()
    }

    #[test]
    fn the_review_files_are_server_urls_for_development_and_test() {
        let got = urls(&[
            (".env.development", "DATABASE_URL=postgres://localhost/app_development\n"),
            (".env", "DATABASE_URL=postgres://localhost/app_development\n"),
            (".env.test", "DATABASE_URL=postgres://localhost/app_test\n"),
        ]);
        assert!(got.iter().any(|url| url == "postgres://localhost/app_development"), "{got:?}");
        assert!(got.iter().any(|url| url == "postgres://localhost/app_test"), "{got:?}");
    }

    #[test]
    fn a_server_url_only_in_env_test_is_kept() {
        let got = urls(&[(".env.test", "DATABASE_URL=postgres://localhost/app_test\n")]);
        assert_eq!(got, ["postgres://localhost/app_test"]);
    }

    #[test]
    fn env_local_does_not_apply_to_test_and_the_first_file_wins() {
        let got = urls(&[
            (".env.development.local", "DATABASE_URL=sqlite3:storage/development.sqlite3\n"),
            (".env", "DATABASE_URL=postgres://localhost/app_development\n"),
        ]);
        assert!(got.iter().any(|url| url == "sqlite3:storage/development.sqlite3"), "{got:?}");
        assert!(got.iter().any(|url| url == "postgres://localhost/app_development"), "{got:?}");

        let dev_only = urls(&[(".env.local", "DATABASE_URL=postgres://localhost/from_local\n")]);
        assert_eq!(dev_only, ["postgres://localhost/from_local"]);
    }

    #[test]
    fn a_variable_already_set_wins_over_the_file() {
        let mut env = HashMap::new();
        env.insert("DATABASE_URL".to_string(), "sqlite3:storage/development.sqlite3".to_string());
        let got =
            database_urls_in(&files(&[(".env", "DATABASE_URL=postgres://localhost/app_development\n")]), &env).unwrap();
        assert!(got.is_empty(), "{got:?}");
    }

    #[test]
    fn quotes_export_comments_and_substitution_resolve() {
        let got = urls(&[(
            ".env",
            "\
# DATABASE_URL=postgres://localhost/commented
export HOST=localhost
DATABASE_URL=\"postgres://${HOST}/app_development\"
QUEUE_DATABASE_URL='postgres://localhost/queue'
",
        )]);
        assert!(got.iter().any(|url| url == "postgres://localhost/app_development"), "{got:?}");
        assert!(got.iter().any(|url| url == "postgres://localhost/queue"), "{got:?}");
    }

    #[test]
    fn a_bad_line_is_a_fault_and_not_an_empty_url_list() {
        let err = database_urls_in(
            &files(&[(".env", "NOT A VALID LINE\nDATABASE_URL=postgres://localhost/app\n")]),
            &HashMap::new(),
        )
        .unwrap_err();
        assert_eq!(err.file, ".env");
        assert_eq!(err.reason(), "cannot parse");
    }

    #[test]
    fn command_substitution_is_a_fault() {
        let err =
            database_urls_in(&files(&[(".env", "DATABASE_URL=$(echo postgres://localhost/app)\n")]), &HashMap::new())
                .unwrap_err();
        assert_eq!(err.reason(), "command substitution");
        let literal = urls(&[(".env", "DATABASE_URL='postgres://localhost/$(echo app)'\n")]);
        assert!(!literal.is_empty(), "{literal:?}");
        assert!(literal.iter().all(|url| url == "postgres://localhost/$(echo app)"), "{literal:?}");
    }

    #[test]
    fn a_directory_is_ignored_and_unreadable_or_non_utf8_is_a_fault() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join(".env")).unwrap();
        assert!(database_urls(tmp.path(), &HashMap::new()).unwrap().is_empty());

        let bad = tmp.path().join(".env.test");
        std::fs::write(&bad, [0xFF, 0xFE]).unwrap();
        let err = database_urls(tmp.path(), &HashMap::new()).unwrap_err();
        assert_eq!(err.path, bad);
        assert_eq!(err.reason(), "not utf-8");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_a_fault() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(".env");
        std::fs::write(&path, "DATABASE_URL=postgres://localhost/app\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        let result = database_urls(tmp.path(), &HashMap::new());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = result.unwrap_err();
        assert_eq!(err.path, path);
        assert_eq!(err.reason(), "could not be read");
    }

    proptest::proptest! {
        /// The URL Rails opens is the parent value when set, otherwise the first
        /// file in that environment's load order. A line that does not parse is
        /// a fault, never the same as a file with no URL.
        #[test]
        fn a_server_url_in_dotenv_follows_load_order_and_a_bad_line_is_a_fault(
            dev_local in url_line(),
            env_local in url_line(),
            dev in url_line(),
            shared in url_line(),
            test_local in url_line(),
            test_file in url_line(),
            process in url_line(),
        ) {
            let slots = [
                (".env.development.local", &dev_local),
                (".env.local", &env_local),
                (".env.development", &dev),
                (".env", &shared),
                (".env.test.local", &test_local),
                (".env.test", &test_file),
            ];
            let mut present = HashMap::new();
            for (name, body) in slots {
                if let Some(body) = body {
                    present.insert(name.to_string(), format!("{body}\n"));
                }
            }
            let process_url = process.as_deref().and_then(|line| line.strip_prefix("DATABASE_URL="));
            let mut env = HashMap::new();
            if let Some(url) = process_url {
                env.insert("DATABASE_URL".to_string(), url.to_string());
            }
            let got = database_urls_in(&present, &env).unwrap();
            let server = got.iter().any(|(_, url)| rails_yml::server_database_url(url))
                || process_url.is_some_and(rails_yml::server_database_url);
            let dev_order = [dev_local.as_deref(), env_local.as_deref(), dev.as_deref(), shared.as_deref()];
            let test_order = [test_local.as_deref(), test_file.as_deref(), shared.as_deref()];
            let expect = [dev_order.as_slice(), test_order.as_slice()]
                .iter()
                .any(|order| winning(process_url, order).is_some_and(rails_yml::server_database_url));
            proptest::prop_assert_eq!(server, expect);

            present.insert(".env".to_string(), "NOT A VALID LINE\n".to_string());
            proptest::prop_assert!(database_urls_in(&present, &env).is_err());
        }
    }

    fn url_line() -> impl proptest::strategy::Strategy<Value = Option<String>> {
        use proptest::prelude::*;
        prop_oneof![
            Just(None),
            Just(Some("DATABASE_URL=sqlite3:storage/development.sqlite3".to_string())),
            Just(Some("DATABASE_URL=postgres://localhost/app".to_string())),
        ]
    }

    fn winning<'a>(process: Option<&'a str>, files: &[Option<&'a str>]) -> Option<&'a str> {
        if let Some(url) = process {
            return Some(url);
        }
        files.iter().find_map(|line| line.and_then(|line| line.strip_prefix("DATABASE_URL=")))
    }
}
