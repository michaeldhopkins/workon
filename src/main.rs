mod claude_trust;
mod cli;
mod deps;
mod discover;
mod home;
mod layout;
mod resolve;
mod session;
mod trust;
mod vcs;
mod workspace;

use anyhow::Result;
use clap::Parser;

use crate::workspace::WorkspaceOptions;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();
    match cli.command {
        Some(command) => run_subcommand(command),
        None => run_session(cli.session),
    }
}

fn run_subcommand(command: cli::Command) -> Result<()> {
    match command {
        cli::Command::Create { name, config, skip_copy_ignored, json } => {
            let project = resolve::resolve()?;
            let vcs = vcs::detect(&project.dir)?;
            let args = workspace::CreateArgs {
                skip_copy_ignored,
                name: name.as_deref().filter(|s| !s.is_empty()),
                config: config.as_deref(),
                json,
            };
            workspace::cmd_create(&project.dir, &project.name, args, &*vcs)
        }
        cli::Command::Attach { reference, config } => {
            workspace::cmd_attach(reference.as_deref(), config.as_deref())
        }
        cli::Command::Destroy { reference, no_save, json } => {
            workspace::cmd_destroy(reference.as_deref(), no_save, json)
        }
        cli::Command::List { json } => workspace::cmd_list(json),
        cli::Command::Path { reference } => workspace::cmd_path(reference.as_deref()),
    }
}

/// The default (no-subcommand) flow: a session in cwd, or the ephemeral `-w`
/// workspace. Unchanged from before subcommands existed.
fn run_session(session: cli::SessionArgs) -> Result<()> {
    let project = resolve::resolve()?;
    let config = session.config.as_deref();

    // Resolve the layout first so deps::check_all knows which binaries to require
    // and we can fail fast on resume + non-claude-config combinations.
    let layout_content = layout::read_config(config)?;
    layout::validate_layout(&layout_content)?;
    deps::check_all(&layout_content)?;

    if session.resume.is_some() {
        layout::ensure_resume_compatible(config.unwrap_or("default"), &layout_content)?;
    }

    // Treat `--name ""` as no name so it falls back to the default.
    let name = session.name.as_deref().filter(|s| !s.is_empty());

    if session.workspace {
        let vcs = vcs::detect(&project.dir)?;
        let opts = WorkspaceOptions {
            skip_copy_ignored: session.skip_copy_ignored,
            label: name,
            resume: session.resume.as_deref(),
            config,
        };
        workspace::run_workspace(&project.dir, &project.name, opts, &*vcs)?;
    } else {
        let layout = layout::resolve_layout(config)?;
        session::run(
            name.unwrap_or(&project.name),
            layout.path(),
            &project.dir,
            session.new_session,
            &layout_content,
            config.unwrap_or("default"),
        )?;
    }

    Ok(())
}
