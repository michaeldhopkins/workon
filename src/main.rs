use std::process::ExitCode;

use anyhow::Result;
use clap::{CommandFactory, Parser};

use workon::workspace::{self, WorkspaceOptions};
use workon::{cli, layout, mux, not_ready, resolve, session, vcs};

fn main() -> Result<ExitCode> {
    // Serve shell-completion requests (COMPLETE=<shell> workon …) and exit;
    // a no-op on a normal invocation. Enables dynamic ws_id/nickname candidates.
    clap_complete::CompleteEnv::with_factory(cli::Cli::command).complete();

    // `--mux` is moved after the subcommand. See `cli::normalize_argv`.
    let cli = cli::Cli::parse_from(cli::normalize_argv(std::env::args_os()));
    mux::install(cli.mux)?;
    match cli.command {
        Some(command) => run_subcommand(command),
        None => run_session(cli.session),
    }
}

/// `true` (ready) is success; `false` means a setup step failed and the workspace is not ready.
fn exit_code(ready: bool) -> ExitCode {
    if ready {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(not_ready::EXIT_NOT_READY)
    }
}

fn run_subcommand(command: cli::Command) -> Result<ExitCode> {
    match command {
        cli::Command::Create { name, config, skip_copy_ignored, base, json } => {
            let project = resolve::resolve()?;
            let mut vcs = vcs::detect(&project.dir)?;
            if let Some(rev) = base.as_deref() {
                vcs = Box::new(vcs::PinnedBase::new(vcs, &project.dir, rev)?);
            }
            let args = workspace::CreateArgs {
                skip_copy_ignored,
                name: cli::given_name(name.as_deref()),
                config: config.as_deref(),
                json,
            };
            workspace::cmd_create(&project.dir, &project.name, args, &*vcs).map(exit_code)
        }
        cli::Command::Attach { reference, config } => {
            workspace::cmd_attach(reference.as_deref(), config.as_deref()).map(|()| ExitCode::SUCCESS)
        }
        cli::Command::Destroy { reference, no_save, json } => {
            workspace::cmd_destroy(reference.as_deref(), no_save, json).map(|()| ExitCode::SUCCESS)
        }
        cli::Command::List { json } => workspace::cmd_list(json).map(|()| ExitCode::SUCCESS),
        cli::Command::Path { reference } => workspace::cmd_path(reference.as_deref()).map(|()| ExitCode::SUCCESS),
        cli::Command::Run { name, config, command } => run_in_background(name, config, command),
        cli::Command::Send { name, text } => type_into(name, text),
        cli::Command::History { name, lines } => print_history(name, lines),
        cli::Command::Wait { name, text, timeout } => wait_for_text(name, text, timeout),
    }
}

fn run_in_background(name: Option<String>, config: Option<String>, command: Option<String>) -> Result<ExitCode> {
    let project = resolve::resolve()?;
    let cfg = layout::read_config(config.as_deref())?;
    layout::validate_layout(&cfg.layout)?;
    mux::require(&cfg.layout)?;
    let session = cli::given_name(name.as_deref()).unwrap_or(&project.name);
    let resolved = cfg.resolve()?;
    let empty = std::collections::HashMap::new();
    mux::run_background(session, &project.dir, resolved.path(), &empty, command.as_deref())?;
    Ok(ExitCode::SUCCESS)
}

fn type_into(name: Option<String>, text: String) -> Result<ExitCode> {
    mux::require("")?;
    let project = resolve::resolve()?;
    let session = cli::given_name(name.as_deref()).unwrap_or(&project.name);
    mux::send(session, &text)?;
    Ok(ExitCode::SUCCESS)
}

fn print_history(name: Option<String>, lines: usize) -> Result<ExitCode> {
    mux::require("")?;
    let project = resolve::resolve()?;
    let session = cli::given_name(name.as_deref()).unwrap_or(&project.name);
    println!("{}", mux::history(session, lines)?);
    Ok(ExitCode::SUCCESS)
}

fn wait_for_text(name: Option<String>, text: String, timeout: u64) -> Result<ExitCode> {
    mux::require("")?;
    let project = resolve::resolve()?;
    let session = cli::given_name(name.as_deref()).unwrap_or(&project.name);
    mux::wait_for(session, &text, std::time::Duration::from_secs(timeout))?;
    Ok(ExitCode::SUCCESS)
}

/// The default (no-subcommand) flow: a session in cwd, or the ephemeral `-w`
/// workspace. Unchanged from before subcommands existed.
fn run_session(session: cli::SessionArgs) -> Result<ExitCode> {
    let project = resolve::resolve()?;
    let config = session.config.as_deref();

    // Resolve the layout first so the multiplexer check knows which binaries to
    // require, and fail fast on resume or a config that can't resume.
    let cfg = layout::read_config(config)?;
    layout::validate_layout(&cfg.layout)?;
    mux::require(&cfg.layout)?;

    if session.resume.is_some() {
        cfg.ensure_resume_compatible(config.unwrap_or("default"))?;
    }

    // Only the workspace flow injects, and it provisions a worktree first — so
    // reject an un-injectable layout here rather than halfway through setup.
    if session.workspace {
        cfg.ensure_single_agent_pane()?;
    }

    let name = cli::given_name(session.name.as_deref());

    if session.workspace {
        let vcs = vcs::detect(&project.dir)?;
        let opts = WorkspaceOptions {
            skip_copy_ignored: session.skip_copy_ignored,
            label: name,
            resume: session.resume.as_deref(),
            config,
            cfg: &cfg,
        };
        return workspace::run_workspace(&project.dir, &project.name, opts, &*vcs).map(exit_code);
    }
    let layout = cfg.resolve()?;
    if mux::current() != mux::MuxKind::Zellij {
        let empty = std::collections::HashMap::new();
        mux::open_in_terminal(name.unwrap_or(&project.name), layout.path(), &project.dir, session.new_session, &empty)?;
        return Ok(ExitCode::SUCCESS);
    }
    session::run(
        name.unwrap_or(&project.name),
        layout.path(),
        &project.dir,
        session.new_session,
        &cfg.layout,
        config.unwrap_or("default"),
    )?;
    Ok(ExitCode::SUCCESS)
}
