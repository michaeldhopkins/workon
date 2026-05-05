mod claude_trust;
mod cli;
mod deps;
mod home;
mod layout;
mod resolve;
mod session;
mod vcs;
mod workspace;

use anyhow::Result;
use clap::Parser;

use crate::workspace::WorkspaceOptions;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();

    let project = resolve::resolve(cli.project.as_deref())?;
    let config = cli.config.as_deref();

    // Resolve the layout first so deps::check_all knows which binaries to require
    // and we can fail fast on resume + non-claude-config combinations.
    let layout_content = layout::read_config(config)?;
    layout::validate_layout(&layout_content)?;
    deps::check_all(&layout_content)?;

    if cli.resume.is_some() {
        layout::ensure_resume_compatible(config.unwrap_or("default"), &layout_content)?;
    }

    if let Some(label) = cli.workspace {
        let label = if label.is_empty() { None } else { Some(label.as_str()) };
        let vcs = vcs::detect(&project.dir)?;
        let opts = WorkspaceOptions {
            skip_copy_ignored: cli.skip_copy_ignored,
            label,
            resume: cli.resume.as_deref(),
            config,
        };
        workspace::run_workspace(&project.dir, &project.name, opts, &*vcs)?;
    } else {
        let layout = layout::resolve_layout(config)?;
        session::run(
            &project.name,
            layout.path(),
            &project.dir,
            cli.new_session,
            &layout_content,
            config.unwrap_or("default"),
        )?;
    }

    Ok(())
}
