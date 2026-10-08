fn main() {
    let zellij = "zellij";
    std::process::Command::new("zellij").status().ok();
    Cmd::new(zellij).run().ok();
    vcs_runner::run_git_utf8(std::path::Path::new("."), &["status"]).ok();
    vcs_runner::jj_available();
}

#[cfg(test)]
mod tests {
    #[test]
    fn y_saves() {}
}
