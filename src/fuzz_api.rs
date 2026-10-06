//! Entry points for the fuzz targets in `fuzz/`, compiled only under `cargo fuzz`
//! (which passes `--cfg fuzzing`). The functions they wrap are crate-private; this
//! module reaches them without making them part of the library API.

use std::collections::HashSet;

use crate::provision::{alembic, ef_core, laravel, phoenix, prisma, rails};

/// Every parser workon runs over a file in the repo being opened, over another
/// tool's stdout, or over a layout, with results returned so a target can check them.
pub struct Parsed {
    pub app_name: Option<String>,
    pub first_bookmark: String,
    pub major_minor: Option<(u32, u32)>,
    pub commands: Vec<String>,
}

pub fn parse_all(text: &str) -> Parsed {
    let _ = rails::adapter(text);
    let _ = prisma::datasource_provider(text);
    let _ = prisma::url_env_var(text);
    let _ = laravel::phpunit_db_connection(text);
    let _ = alembic::configured_url(text);
    let _ = ef_core::include_value(text);
    let _ = crate::layout::focused_command(text);
    Parsed {
        app_name: phoenix::app_name(text),
        first_bookmark: crate::vcs::jj::first_real_bookmark(text).to_string(),
        major_minor: crate::deps::parse_major_minor(text),
        commands: crate::deps::extract_commands(text),
    }
}

pub fn parse_descendants(ps_stdout: &str, root_pid: u32) -> HashSet<String> {
    crate::session::parse_descendants(ps_stdout, root_pid)
}

pub fn test_db_name(project_name: &str, ws_id: &str) -> String {
    crate::provision::test_db_name(project_name, ws_id)
}

pub fn partition_for(app: &str, ws_id: &str) -> String {
    phoenix::partition_for(app, ws_id)
}

pub fn auth_prefix(user: &str, password: Option<String>) -> String {
    crate::provision::auth_prefix(user, password)
}

pub fn percent_encode_userinfo(s: &str) -> String {
    crate::provision::percent_encode_userinfo(s)
}

pub fn anchored_server_pattern(name: &str) -> String {
    crate::session::anchored_server_pattern(name)
}

pub fn slugify(text: &str) -> String {
    crate::workspace::slugify(text)
}

pub fn postgres_url(auth: &str, host: &str, port: Option<&str>, name: &str) -> String {
    crate::provision::postgres_url(auth, host, port, name)
}

pub fn npgsql_connection_string(host: &str, port: &str, name: &str, user: &str, password: Option<&str>) -> String {
    ef_core::npgsql_connection_string_from(host, port, name, user, password)
}
