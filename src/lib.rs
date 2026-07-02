//! Library surface of the `workon` crate. The binary (`main.rs`) is a thin
//! shim over these modules, and `examples/generate_assets.rs` reaches
//! `cli::Cli` through here to render the man page and completion scripts.

pub mod claude_trust;
pub mod cli;
pub mod deps;
pub mod discover;
pub mod home;
pub mod layout;
pub mod resolve;
pub mod session;
pub mod trust;
pub mod vcs;
pub mod workspace;
