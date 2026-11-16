//! What `config/database.yml` says about server databases, read as text.
//! A Rails app whose databases cannot be classified as SQLite-only has to prove
//! isolation at runtime. Quoting, comments, and an earlier SQLite adapter do
//! not hide a later server database.

/// A server adapter workon can suffix and drop, or an earlier SQLite adapter
/// when the file has no server adapter.
pub(crate) fn first_adapter(database_yml: &str) -> Option<String> {
    let mut first = None;
    for token in adapter_tokens(database_yml) {
        if first.is_none() {
            first = Some(token.clone());
        }
        if is_server(&token) {
            return Some(token);
        }
    }
    first
}

/// `false` only when every adapter is SQLite and the file has no server URL
/// and no ERB connection target. A pool size written with ERB is still SQLite.
/// An adapter, `url:`, or database name that ERB fills in has to be proven by
/// `rails runner`, because the file does not say which server it will be.
pub(crate) fn needs_server_database(database_yml: &str) -> bool {
    if contains_server_url(database_yml) || dynamic_connection(database_yml) {
        return true;
    }
    let tokens = adapter_tokens(database_yml);
    match first_adapter(database_yml).as_deref() {
        Some(token) if is_server(token) => true,
        Some(_) if !tokens.is_empty() && tokens.iter().all(|token| is_sqlite(token)) => false,
        Some(_) | None => true,
    }
}

/// A sqlite file still has to be proven when a database URL is not sqlite.
/// The URL is what Rails opens for the current environment, in place of the file.
pub(crate) fn needs_isolation<'a>(database_yml: &str, database_urls: impl IntoIterator<Item = &'a str>) -> bool {
    needs_server_database(database_yml) || database_urls.into_iter().any(server_database_url)
}

/// `DATABASE_URL` or `<NAME>_DATABASE_URL`, the variables Active Record uses
/// in place of a configuration from the file.
pub(crate) fn is_database_url_key(key: &str) -> bool {
    key == "DATABASE_URL" || key.ends_with("_DATABASE_URL")
}

/// A non-empty database URL whose scheme is not sqlite. A value that cannot
/// be classified is not proof that the app stays on a sqlite file.
pub(crate) fn server_database_url(url: &str) -> bool {
    let url = url.trim();
    if url.is_empty() {
        return false;
    }
    let scheme = url_scheme(url).to_ascii_lowercase();
    !matches!(scheme.as_str(), "sqlite" | "sqlite3")
}

fn url_scheme(url: &str) -> &str {
    let Some(end) = url.find(':') else {
        return "";
    };
    let scheme = &url[..end];
    if scheme.bytes().all(|byte| byte.is_ascii_alphanumeric()) {
        scheme
    } else {
        ""
    }
}

/// `gem "rails"` or `gem 'rails'`, not a comment and not a name that only
/// contains the word.
pub(crate) fn gemfile_depends_on_rails(gemfile: &str) -> bool {
    gemfile.lines().any(|line| {
        let code = line.split('#').next().unwrap_or("");
        code.split_whitespace().any(|word| word == "gem")
            && (code.contains("\"rails\"") || code.contains("'rails'"))
            && !code.contains("\"rails-")
            && !code.contains("'rails-")
            && !code.contains("-rails\"")
            && !code.contains("-rails'")
    })
}

fn adapter_tokens(database_yml: &str) -> Vec<String> {
    database_yml.lines().filter_map(adapter_token).collect()
}

fn adapter_token(line: &str) -> Option<String> {
    let rest = line.trim().strip_prefix("adapter:")?.trim();
    if rest.is_empty() {
        return None;
    }
    let token = unquote(rest);
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Quotes around the value, and a `#` comment that is outside quotes, are not
/// part of the adapter name.
fn unquote(value: &str) -> String {
    let bytes = value.as_bytes();
    let (quote, body) = if bytes.first() == Some(&b'"') {
        ('"', &value[1..])
    } else if bytes.first() == Some(&b'\'') {
        ('\'', &value[1..])
    } else {
        return strip_comment(value).trim().to_string();
    };
    match body.find(quote) {
        Some(end) => body[..end].to_string(),
        None => body.to_string(),
    }
}

fn strip_comment(value: &str) -> &str {
    match value.find(" #") {
        Some(at) => &value[..at],
        None => value,
    }
}

fn is_server(token: &str) -> bool {
    matches!(token, "postgresql" | "postgres" | "mysql2" | "mysql" | "trilogy" | "postgis")
}

fn is_sqlite(token: &str) -> bool {
    matches!(token, "sqlite" | "sqlite3")
}

/// `url:` or `database:` whose value is ERB. A pool or a sqlite path is not.
fn dynamic_connection(database_yml: &str) -> bool {
    database_yml.lines().any(|line| {
        let trimmed = line.trim();
        let Some(rest) = trimmed.strip_prefix("url:").or_else(|| trimmed.strip_prefix("database:")) else {
            return false;
        };
        if !rest.contains("<%") {
            return false;
        }
        trimmed.starts_with("url:") || rest.contains("DATABASE_URL") || rest.contains("://")
    })
}

fn contains_server_url(text: &str) -> bool {
    text.contains("postgres://")
        || text.contains("postgresql://")
        || text.contains("mysql://")
        || text.contains("mysql2://")
        || text.contains("trilogy://")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_quoted_or_commented_server_adapter_is_the_adapter() {
        assert_eq!(first_adapter("adapter: \"postgresql\"\n").as_deref(), Some("postgresql"));
        assert_eq!(first_adapter("adapter: 'mysql2'\n").as_deref(), Some("mysql2"));
        assert_eq!(first_adapter("adapter: postgresql # primary\n").as_deref(), Some("postgresql"));
        assert_eq!(first_adapter("adapter: postgis\n").as_deref(), Some("postgis"));
    }

    #[test]
    fn sqlite_before_postgres_still_needs_a_server_database() {
        let yml = "adapter: sqlite3\nadapter: postgresql\n";
        assert_eq!(first_adapter(yml).as_deref(), Some("postgresql"));
        assert!(needs_server_database(yml));
    }

    #[test]
    fn only_sqlite_does_not_need_a_server_database() {
        assert!(!needs_server_database("adapter: sqlite3\nadapter: sqlite\n"));
    }

    #[test]
    fn erb_and_a_server_url_need_a_proof() {
        assert!(needs_server_database("adapter: <%= ENV[\"DB_ADAPTER\"] %>\n"));
        assert!(needs_server_database("url: postgres://localhost/app\n"));
        assert!(needs_server_database("url: trilogy://localhost/app\n"));
        assert!(needs_server_database("adapter: sqlite3\ndatabase: <%= ENV[\"DATABASE_URL\"] %>\n"));
    }

    #[test]
    fn an_unclassifiable_database_url_on_sqlite_needs_isolation() {
        let yml = "adapter: sqlite3\ndatabase: storage/development.sqlite3\n";
        assert!(!needs_isolation(yml, std::iter::empty::<&str>()));
        assert!(!needs_isolation(yml, ["", "sqlite3:storage/development.sqlite3"]));
        assert!(needs_isolation(yml, ["postgres://localhost/app_development"]));
        assert!(needs_isolation(yml, ["not a url"]));
    }

    /// A pool size is ERB in a default Rails sqlite config. It is not a server
    /// database, and it must not make workon boot Rails or export the hook.
    #[test]
    fn an_erb_pool_on_sqlite_is_not_a_server_database() {
        let yml = "\
adapter: sqlite3
pool: <%= ENV.fetch(\"RAILS_MAX_THREADS\") { 5 } %>
database: storage/development.sqlite3
";
        assert!(!needs_server_database(yml), "pool ERB is not a server database");
    }

    proptest::proptest! {
        /// A server adapter is the one workon has to isolate, wherever it sits,
        /// and quotes or a trailing comment are not part of its name.
        #[test]
        fn a_server_adapter_wins_over_sqlite_without_its_quotes_or_comment(
            sqlite_first in proptest::bool::ANY,
            quoted in proptest::bool::ANY,
            comment in proptest::bool::ANY,
            server in "(postgresql|postgres|mysql2|mysql|trilogy|postgis)",
        ) {
            let shown = if quoted { format!("\"{server}\"") } else { server.clone() };
            let shown = if comment { format!("{shown} # note") } else { shown };
            let mut yml = String::new();
            if sqlite_first {
                yml.push_str("  adapter: sqlite3\n");
            }
            yml.push_str(&format!("  adapter: {shown}\n"));
            let got = first_adapter(&yml);
            proptest::prop_assert_eq!(got.as_deref(), Some(server.as_str()));
            proptest::prop_assert!(needs_server_database(&yml));
        }

        /// ERB in a pool size leaves a sqlite file alone. ERB in `url:` does not.
        #[test]
        fn erb_outside_the_adapter_does_not_hide_sqlite_or_a_url(
            name in "[A-Z_]{1,12}",
        ) {
            let pool = format!("<%= ENV[{name}] %>");
            let sqlite = format!("adapter: sqlite3\npool: {pool}\ndatabase: storage/development.sqlite3\n");
            proptest::prop_assert!(!needs_server_database(&sqlite));
            let url = format!("adapter: sqlite3\nurl: {pool}\n");
            proptest::prop_assert!(needs_server_database(&url));
        }

        /// A sqlite file needs isolation exactly when a database URL is not sqlite.
        /// A server file needs it with or without a URL.
        #[test]
        fn a_sqlite_file_needs_isolation_exactly_when_a_url_is_not_sqlite(
            scheme in "(postgres|postgresql|mysql|mysql2|trilogy|sqlite|sqlite3)",
            name in "[a-z]{1,8}",
            blank in proptest::bool::ANY,
        ) {
            let yml = "adapter: sqlite3\ndatabase: storage/development.sqlite3\n";
            let url = if blank { String::new() } else { format!("{scheme}://localhost/{name}") };
            let expect = !blank && scheme != "sqlite" && scheme != "sqlite3";
            proptest::prop_assert_eq!(needs_isolation(yml, [url.as_str()]), expect);
            proptest::prop_assert!(needs_isolation("adapter: postgresql\n", std::iter::empty::<&str>()));
            proptest::prop_assert!(needs_isolation(
                "adapter: postgresql\n",
                ["sqlite3:storage/x.sqlite3"],
            ));
        }

        /// `gem "rails"` is a dependency. A comment, `rails-ujs`, and `haml-rails` are not.
        #[test]
        fn a_rails_gem_is_the_word_rails_in_quotes(
            quote in "['\"]",
            commented in proptest::bool::ANY,
        ) {
            let line = format!("gem {quote}rails{quote}");
            let text = if commented { format!("# {line}") } else { line.clone() };
            proptest::prop_assert_eq!(gemfile_depends_on_rails(&text), !commented);
            let lookalike = format!("gem {quote}haml-rails{quote}\ngem {quote}rails-ujs{quote}");
            proptest::prop_assert!(!gemfile_depends_on_rails(&lookalike));
        }
    }
}
