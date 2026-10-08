//! The built `workon` on a pseudo-terminal, for each place it hands the terminal to zellij
//! or reads an answer from it. `tests/tui.toml` lists them; zellij is a stub that records
//! how it was called (`support::Stubs`).

use crate::support::World;

/// zellij's stub, answering as the default does and also doing `on_launch` (a shell
/// snippet, run in the session's directory) when asked for a new session.
fn zellij_that(on_launch: &str, listing: &str) -> String {
    format!(
        "case \"$1\" in\n\
         --version) echo 'zellij 0.43.1' ;;\n\
         list-sessions) {listing} ;;\n\
         --new-session-with-layout) {on_launch} ;;\n\
         esac"
    )
}

const NO_SESSIONS: &str = "echo 'No active zellij sessions found.' >&2; exit 1";

fn workon_branches(world: &World, proj: &std::path::Path) -> String {
    world.git(proj, &["branch", "--list", "workon/*"])
}

/// Waits (bounded) for the background `rm -rf` teardown starts.
fn assert_removed_soon(dir: &std::path::Path) {
    let start = std::time::Instant::now();
    loop {
        if !dir.exists() {
            return;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(10), "{} was never removed", dir.display());
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[test]
fn w_hands_a_new_workspace_to_zellij_and_removes_it_on_quit() {
    let world = World::new();
    let proj = world.project(&[("README.md", "hello\n")]);
    // The worktree is removed once zellij returns, so look at it while the session runs.
    world.stubs.set("zellij", &zellij_that("cp README.md \"$HOME/seen-at-launch\"", NO_SESSIONS));

    let mut tui = world.tui(&proj, &["-w"]);
    assert!(tui.wait_for_exit(), "{}", tui.screen());

    let launch = world.stubs.call("zellij", "--new-session-with-layout");
    assert_eq!(launch.args[2], "--session", "{launch:?}");
    assert!(launch.args[3].starts_with("proj-ws-"), "{launch:?}");
    assert!(launch.cwd.starts_with(world.worktrees()), "zellij runs in the new worktree: {launch:?}");
    let seen = std::fs::read_to_string(world.home().join("seen-at-launch")).unwrap_or_default();
    assert_eq!(seen, "hello\n", "the worktree holds the project when zellij starts");
    assert!(!tui.screen().contains("Save under"), "nothing changed, so nothing to ask");
    assert_removed_soon(&launch.cwd);
    assert_eq!(workon_branches(&world, &proj), "");
}

#[test]
fn save_prompt_yes_keeps_the_work_on_a_branch() {
    let world = World::new();
    let proj = world.project(&[("README.md", "hello\n")]);
    world.stubs.set("zellij", &zellij_that("echo work > notes.txt", NO_SESSIONS));

    let mut tui = world.tui(&proj, &["-w"]);
    tui.wait_for_text("changed:  notes.txt");
    tui.wait_for_text("? [Y/n]");
    tui.press("y\r");
    assert!(tui.wait_for_exit(), "{}", tui.screen());

    let ws_dir = world.stubs.call("zellij", "--new-session-with-layout").cwd;
    let ws_id = ws_dir.file_name().unwrap().to_string_lossy().trim_start_matches("proj-").to_string();
    assert_eq!(workon_branches(&world, &proj).trim(), format!("workon/{ws_id}"));
    let saved = world.git(&proj, &["show", &format!("workon/{ws_id}:notes.txt")]);
    assert_eq!(saved, "work\n");
    assert_removed_soon(&ws_dir);
}

#[test]
fn save_prompt_no_discards_the_work() {
    let world = World::new();
    let proj = world.project(&[("README.md", "hello\n")]);
    world.stubs.set("zellij", &zellij_that("echo work > notes.txt", NO_SESSIONS));

    let mut tui = world.tui(&proj, &["-w"]);
    tui.wait_for_text("? [Y/n]");
    tui.press("n\r");
    assert!(tui.wait_for_exit(), "{}", tui.screen());

    assert_eq!(workon_branches(&world, &proj), "");
    assert_removed_soon(&world.stubs.call("zellij", "--new-session-with-layout").cwd);
}

#[test]
fn workon_launches_a_session_named_for_the_project_in_the_project() {
    let world = World::new();
    let proj = world.project(&[]);

    let mut tui = world.tui(&proj, &[]);
    assert!(tui.wait_for_exit(), "{}", tui.screen());

    let launch = world.stubs.call("zellij", "--new-session-with-layout");
    assert_eq!(launch.args[2..], ["--session", "proj"]);
    assert_eq!(launch.cwd, proj);
}

#[test]
fn workon_attaches_to_the_projects_running_session() {
    let world = World::new();
    let proj = world.project(&[]);
    world.stubs.set("zellij", &zellij_that("exit 0", "echo 'proj [Created 1m ago]'"));

    let mut tui = world.tui(&proj, &[]);
    assert!(tui.wait_for_exit(), "{}", tui.screen());

    let attach = world.stubs.call("zellij", "attach");
    assert_eq!(attach.args, ["attach", "proj"]);
    assert_eq!(attach.cwd, proj);
    assert!(world.stubs.calls("zellij").iter().all(|c| c.args[0] != "--new-session-with-layout"));
}

#[test]
fn new_session_deletes_the_running_session_and_launches_anew() {
    let world = World::new();
    let proj = world.project(&[]);
    world.stubs.set("zellij", &zellij_that("exit 0", "echo 'proj [Created 1m ago]'"));

    let mut tui = world.tui(&proj, &["--new-session"]);
    assert!(tui.wait_for_exit(), "{}", tui.screen());

    assert_eq!(world.stubs.call("zellij", "delete-session").args, ["delete-session", "proj", "--force"]);
    assert_eq!(world.stubs.call("zellij", "--new-session-with-layout").cwd, proj);
}

#[test]
fn attach_hands_an_existing_workspace_to_zellij_and_keeps_it() {
    let world = World::new();
    let proj = world.project(&[]);
    let created = world.workon(&proj).args(["create", "--json"]).assert().success().get_output().stdout.clone();
    let created: serde_json::Value = serde_json::from_slice(&created).unwrap();
    let ws_id = created["ws_id"].as_str().unwrap();

    let mut tui = world.tui(&proj, &["attach", ws_id]);
    assert!(tui.wait_for_exit(), "{}", tui.screen());

    let launch = world.stubs.call("zellij", "--new-session-with-layout");
    assert_eq!(launch.cwd.to_str(), created["path"].as_str());
    assert!(launch.cwd.exists(), "attach leaves the workspace in place");
}

/// Issue #2: a Rails app whose schema load fails, so `-w` stops to ask before zellij.
fn world_whose_schema_load_fails() -> (World, std::path::PathBuf) {
    let world = World::new();
    let proj = world.project(&[("config/database.yml", "test:\n  adapter: postgresql\n")]);
    world.stubs.set("bundle", "echo 'bundler: cannot load' >&2; exit 1");
    (world, proj)
}

#[test]
fn not_ready_pause_enter_opens_the_session_anyway() {
    let (world, proj) = world_whose_schema_load_fails();

    let mut tui = world.tui(&proj, &["-w"]);
    tui.wait_for_text("Open it anyway? [Y/n]");
    assert!(world
        .stubs
        .calls("zellij")
        .iter()
        .all(|c| c.args.first().is_none_or(|a| a != "--new-session-with-layout")));
    tui.press("\r");
    assert!(tui.wait_for_exit(), "{}", tui.screen());

    let launch = world.stubs.call("zellij", "--new-session-with-layout");
    assert!(launch.cwd.starts_with(world.worktrees()), "{launch:?}");
    assert_removed_soon(&launch.cwd);
}

#[test]
fn not_ready_pause_n_removes_the_workspace_without_launching() {
    let (world, proj) = world_whose_schema_load_fails();

    let mut tui = world.tui(&proj, &["-w"]);
    tui.wait_for_text("Open it anyway? [Y/n]");
    tui.press("n\r");
    assert_eq!(tui.wait_for_exit_code(), 3, "{}", tui.screen());

    assert!(world
        .stubs
        .calls("zellij")
        .iter()
        .all(|c| c.args.first().is_none_or(|a| a != "--new-session-with-layout")));
    assert!(tui.screen().contains("Cleaning up workspace"), "{}", tui.screen());
    assert!(!tui.screen().contains("Save under"), "declining asks nothing more");
    assert_eq!(world.stubs.calls("dropdb").len(), 1, "the test database is dropped");
    let left: Vec<_> = std::fs::read_dir(world.worktrees()).unwrap().flatten().map(|e| e.path()).collect();
    for dir in &left {
        assert_removed_soon(dir);
    }
}
