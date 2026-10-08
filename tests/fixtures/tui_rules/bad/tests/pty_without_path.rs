use portable_pty::CommandBuilder;

fn spawn() -> CommandBuilder {
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_workon"));
    cmd.env_clear();
    cmd
}
