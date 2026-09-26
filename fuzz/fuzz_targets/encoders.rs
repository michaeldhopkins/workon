//! Names and credentials workon writes into something else's syntax: a database
//! name, a Phoenix test partition, the user and password in a database URL, a
//! value in an Npgsql connection string, the `pgrep` pattern that finds a
//! session's zellij server, and a workspace slug.
//!
//! Input: `a\0b`. Each value comes from somewhere workon does not control: a
//! project directory's name, a `--name` label, a `mix.exs` app name, `PGUSER` /
//! `PGPASSWORD` / `MYSQL_PWD`. Each check fixes the frame and fuzzes the value,
//! then reads the result back with an independent parser and asserts exact
//! equality.
#![no_main]

use libfuzzer_sys::fuzz_target;
use percent_encoding::percent_decode_str;
use workon::fuzz_api as w;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    let (a, b) = text.split_once('\0').unwrap_or((text, ""));

    db_name(a, b);
    partition(a, b);
    url_credentials(a, b);
    npgsql(b);
    server_pattern(a);
    slug(a);
});

fn is_ident(s: &str) -> bool {
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Postgres truncates an identifier past 63 bytes, and teardown drops the name
/// workon recorded, so the two must be the same string.
fn db_name(project: &str, ws_id: &str) {
    let name = w::test_db_name(project, ws_id);
    assert!(is_ident(&name), "{name:?}");
    assert!(name.ends_with("_test"), "{name:?}");
    if ws_id.chars().count() + "__test".len() <= 63 {
        assert!(name.len() <= 63, "{name:?} is {} bytes", name.len());
    }
    if is_ident(ws_id) {
        assert!(name.ends_with(&format!("_{ws_id}_test")), "the ws id, which carries uniqueness, was cut: {name:?}");
    }
}

/// Phoenix names the test database `{app}_test{partition}`. The app name is an
/// identifier by the time it gets here (`app_name` guarantees it, and
/// `repo_and_tool_text` asserts that), so only identifier characters are kept.
fn partition(app: &str, ws_id: &str) {
    let app: String = app.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '_').collect();
    let p = w::partition_for(&app, ws_id);
    assert!(is_ident(&p), "{p:?}");
    if app.len() + "_test".len() <= 63 {
        let db = format!("{app}_test{p}");
        assert!(db.len() <= 63, "{db:?} is {} bytes", db.len());
    }
}

/// `postgresql://{user}:{password}@localhost/db` must read back as exactly that
/// user, password and host, whatever the credentials hold.
fn url_credentials(user: &str, password: &str) {
    let auth = w::auth_prefix(user, Some(password.to_string()));
    let raw = format!("postgresql://{auth}localhost/db");
    let url = url::Url::parse(&raw).unwrap_or_else(|e| panic!("{raw:?} does not parse: {e}"));
    assert_eq!(url.host_str(), Some("localhost"), "{raw:?}");
    assert_eq!(url.path(), "/db", "{raw:?}");
    let decode = |s: &str| percent_decode_str(s).decode_utf8().expect("utf8").into_owned();
    if user.is_empty() {
        assert_eq!((url.username(), url.password()), ("", None), "{raw:?}");
        return;
    }
    assert_eq!(decode(url.username()), user, "{raw:?}");
    let expected = (!password.is_empty()).then(|| password.to_string());
    assert_eq!(url.password().map(decode), expected, "{raw:?}");
}

/// A value placed between two other pairs must read back unchanged under the
/// ADO.NET connection-string rules Npgsql uses (`DbConnectionStringBuilder`): a
/// value may be wrapped in `'` or `"`, a doubled quote inside is one quote, and an
/// unquoted value ends at `;` and has surrounding whitespace trimmed.
///
/// Control characters are left out: how .NET treats one depends on where it sits,
/// this oracle does not model that, and no credential in an environment variable
/// carries one.
fn npgsql(value: &str) {
    if value.chars().any(char::is_control) {
        return;
    }
    let conn = format!("Host=localhost;Username={};Port=5432", w::npgsql_value(value));
    let pairs = parse_connection_string(&conn).unwrap_or_else(|e| panic!("{conn:?}: {e}"));
    let got: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(got, [("Host", "localhost"), ("Username", value), ("Port", "5432")], "{conn:?}");
}

fn parse_connection_string(s: &str) -> Result<Vec<(String, String)>, String> {
    let mut out = Vec::new();
    let mut chars = s.chars().peekable();
    loop {
        while chars.peek().is_some_and(|c| c.is_whitespace() || *c == ';') {
            chars.next();
        }
        if chars.peek().is_none() {
            return Ok(out);
        }
        let key: String = chars.by_ref().take_while(|c| *c != '=').collect();
        while chars.peek().is_some_and(|c| c.is_whitespace()) {
            chars.next();
        }
        let value = match chars.peek().copied() {
            Some(q @ ('\'' | '"')) => {
                chars.next();
                let mut v = String::new();
                loop {
                    match chars.next() {
                        None => return Err("unterminated quote".into()),
                        Some(c) if c == q && chars.peek() == Some(&q) => {
                            chars.next();
                            v.push(q);
                        }
                        Some(c) if c == q => break,
                        Some(c) => v.push(c),
                    }
                }
                while chars.peek().is_some_and(|c| c.is_whitespace()) {
                    chars.next();
                }
                match chars.next() {
                    None | Some(';') => v,
                    Some(c) => return Err(format!("{c:?} after a quoted value")),
                }
            }
            _ => chars.by_ref().take_while(|c| *c != ';').collect::<String>().trim().to_string(),
        };
        out.push((key.trim().to_string(), value));
    }
}

/// The pattern `pgrep -f` runs must match this session's server and nothing
/// else. `pgrep` takes a POSIX ERE and this oracle is Rust's `regex`; they agree
/// on everything `regex_escape` escapes, and on `[[:space:]]`.
///
/// Names with `/`, whitespace or control characters are left out: the pattern
/// finds the name as the last path element of the server's socket, which such a
/// name cannot be.
fn server_pattern(name: &str) {
    if name.is_empty() || name.chars().any(|c| c == '/' || c.is_whitespace() || c.is_control()) {
        return;
    }
    let pattern = w::anchored_server_pattern(name);
    let re = regex::Regex::new(&pattern).unwrap_or_else(|e| panic!("{pattern:?} does not compile: {e}"));
    let server = format!("zellij --server /tmp/zellij-501/0.43.1/{name}");
    assert!(re.is_match(&server), "{pattern:?} misses {server:?}");
    assert!(re.is_match(&format!("{server} --debug")), "{pattern:?} misses {server:?} with argv after it");
    let mut others = vec![format!("{server}x"), format!("zellij --server /tmp/zellij-501/0.43.1/x{name}")];
    // One character changed at a time: an unescaped `.` or class would still match.
    for (i, c) in name.char_indices().take(32) {
        let swap = if c == 'x' { "y" } else { "x" };
        others.push(format!("zellij --server /tmp/zellij-501/0.43.1/{}{swap}{}", &name[..i], &name[i + c.len_utf8()..]));
    }
    for other in others {
        assert!(!re.is_match(&other), "{pattern:?} matches another session's server {other:?}");
    }
}

/// Workspaces are found by comparing slugs, so a slug must be stable under
/// re-slugging and hold only `[a-z0-9-]` with no empty segments.
fn slug(text: &str) {
    let s = w::slugify(text);
    assert!(s.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'), "{s:?}");
    assert!(!s.starts_with('-') && !s.ends_with('-') && !s.contains("--"), "{s:?}");
    assert_eq!(w::slugify(&s), s, "slugify is not idempotent on {text:?}");
}
