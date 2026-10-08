fn main() {
    let editor = std::env::var("EDITOR").unwrap_or_default();
    let tool = "mise";
    std::process::Command::new("/usr/bin/security").status().ok();
    std::process::Command::new("open").status().ok();
    Cmd::new(editor).run().ok();
    vcs_runner::binary_available(tool);
    vcs_runner::run_jj(std::path::Path::new("."), &["log"]).ok();
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_code_may_run_anything() {
        std::process::Command::new("/bin/echo").status().ok();
    }
}
