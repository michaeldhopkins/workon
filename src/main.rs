mod claude_trust;
mod cli;
mod deps;
mod home;
mod layout;
mod resolve;
mod session;
mod workspace;

use anyhow::Result;
use clap::Parser;

fn main() -> Result<()> {
    let cli = cli::Cli::parse();

    deps::check_all()?;

    let project = resolve::resolve(cli.project.as_deref())?;
    let layout = layout::get_layout()?;

    if let Some(label) = cli.workspace {
        let label = if label.is_empty() { None } else { Some(label.as_str()) };
        workspace::ensure_jj(&project.dir)?;
        workspace::run_workspace(&project.dir, &project.name, layout.path(), cli.skip_copy_ignored, label)?;
    } else {
        session::run(&project.name, layout.path(), &project.dir, cli.new_session)?;
    }

    Ok(())
}
