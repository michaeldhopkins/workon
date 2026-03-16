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

fn main() -> Result<()> {
    let cli = cli::Cli::parse();

    deps::check_all()?;

    let project = resolve::resolve(cli.project.as_deref())?;

    if let Some(label) = cli.workspace {
        let label = if label.is_empty() { None } else { Some(label.as_str()) };
        let vcs = vcs::detect(&project.dir)?;
        workspace::run_workspace(&project.dir, &project.name, cli.skip_copy_ignored, label, cli.resume.as_deref(), &*vcs)?;
    } else {
        let layout = layout::get_layout()?;
        session::run(&project.name, layout.path(), &project.dir, cli.new_session)?;
    }

    Ok(())
}
