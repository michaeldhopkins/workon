//! Names and credentials workon writes into something else's syntax: a database
//! name, a Phoenix test partition, the user, password, host and port in a
//! database URL, the fields of an Npgsql connection string, the `pgrep` pattern
//! that finds a session's zellij server, and a workspace slug.
//!
//! Input: `a\0b\0c\0d`. Each value comes from somewhere workon does not control: a
//! project directory's name, a `--name` label, a `mix.exs` app name, `PGUSER` /
//! `PGPASSWORD` / `PGHOST` / `PGPORT`. `d` is the Npgsql port as text; the URL's
//! port is a number derived from its length, and absent when `d` is. Each check
//! fixes the frame and fuzzes the value,
//! then reads the result back with an independent parser and asserts exact
//! equality.
#![no_main]

use libfuzzer_sys::fuzz_target;
use percent_encoding::percent_decode_str;
use workon::fuzz_api as w;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else { return };
    let mut parts = text.splitn(4, '\0');
    let a = parts.next().unwrap_or_default();
    let b = parts.next().unwrap_or_default();
    let c = parts.next().unwrap_or_default();
    let d = parts.next();
    let port = d.map(|d| (d.len() * 7919 % 65536) as u16);

    db_name(a, b);
    partition(a, b);
    url_credentials(a, b, c, port);
    npgsql(c, d.unwrap_or_default(), a, b);
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

/// A PGHOST as a real environment holds it: an IPv6 literal (unbracketed), or a
/// name or IPv4 address. Other characters are dropped from the fuzzed text, since
/// no server has such a host and libpq would reject it before workon's URL mattered.
fn host_from(seed: &str) -> String {
    if seed.parse::<std::net::Ipv6Addr>().is_ok() {
        return seed.to_string();
    }
    let host: String = seed.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '-').collect();
    if host.is_empty() { "localhost".to_string() } else { host }
}

/// `postgresql://{user}:{password}@{host}[:{port}]/db` must read back as exactly
/// that user, password, host and port, whatever the credentials hold.
fn url_credentials(user: &str, password: &str, host_seed: &str, port: Option<u16>) {
    let host = host_from(host_seed);
    let auth = w::auth_prefix(user, Some(password.to_string()));
    let port_text = port.map(|p| p.to_string());
    let raw = w::postgres_url(&auth, &host, port_text.as_deref(), "db");
    let url = url::Url::parse(&raw).unwrap_or_else(|e| panic!("{raw:?} does not parse: {e}"));
    match host.parse::<std::net::Ipv6Addr>() {
        Ok(ip) => assert_eq!(url.host(), Some(url::Host::Ipv6(ip)), "{raw:?}"),
        Err(_) => assert_eq!(url.host_str(), Some(host.as_str()), "{raw:?}"),
    }
    assert_eq!(url.port(), port, "{raw:?}");
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

/// The whole Npgsql connection string must read back field for field under the
/// ADO.NET rules Npgsql uses (`DbConnectionStringBuilder`): a value may be wrapped
/// in `'` or `"`, a doubled quote inside is one quote, and an unquoted value ends
/// at `;` and has surrounding whitespace trimmed. Host and port come from the
/// environment as much as the credentials do, so all four are fuzzed.
///
/// Control characters are left out: how .NET treats one depends on where it sits,
/// this oracle does not model that, and no environment value carries one.
fn npgsql(host: &str, port: &str, user: &str, password: &str) {
    if [host, port, user, password].iter().any(|v| v.chars().any(char::is_control)) {
        return;
    }
    let conn = w::npgsql_connection_string(host, port, "app_test", user, Some(password));
    let pairs = parse_connection_string(&conn).unwrap_or_else(|e| panic!("{conn:?}: {e}"));
    let got: Vec<(&str, &str)> = pairs.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    let port = if port.is_empty() { "5432" } else { port };
    let mut expected = vec![("Host", host), ("Port", port), ("Database", "app_test"), ("Username", user)];
    if !password.is_empty() {
        expected.push(("Password", password));
    }
    assert_eq!(got, expected, "{conn:?}");
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
