use anyhow::{bail, Result};
use std::path::Path;
use std::process::{Command, Stdio};

fn tmux() -> Command {
    Command::new("tmux")
}

pub fn available() -> bool {
    Command::new("tmux")
        .arg("-V")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn version() -> Option<String> {
    let out = tmux().arg("-V").output().ok()?;
    Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Runs tmux and returns stdout; errors if the command fails.
fn run(args: &[&str]) -> Result<String> {
    let out = tmux().args(args).output()?;
    if !out.status.success() {
        bail!(
            "tmux {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Same, but a failure only means "it does not exist".
fn ok(args: &[&str]) -> bool {
    tmux()
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn has_session(name: &str) -> bool {
    ok(&["has-session", "-t", &format!("={}", name)])
}

pub fn windows(session: &str) -> Vec<String> {
    run(&[
        "list-windows",
        "-t",
        &format!("={}", session),
        "-F",
        "#{window_name}",
    ])
    .map(|s| s.lines().map(|l| l.to_string()).collect())
    .unwrap_or_default()
}

pub struct WindowInfo {
    pub name: String,
    pub command: String,
    pub path: String,
}

pub fn window_info(session: &str) -> Vec<WindowInfo> {
    run(&[
        "list-windows",
        "-t",
        &format!("={}", session),
        "-F",
        "#{window_name}|#{pane_current_command}|#{pane_current_path}",
    ])
    .map(|s| {
        s.lines()
            .filter_map(|l| {
                let mut it = l.splitn(3, '|');
                Some(WindowInfo {
                    name: it.next()?.into(),
                    command: it.next().unwrap_or("").into(),
                    path: it.next().unwrap_or("").into(),
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

pub struct PaneInfo {
    pub id: String,
    pub command: String,
}

pub fn panes(session: &str) -> Vec<PaneInfo> {
    run(&[
        "list-panes",
        "-s",
        "-t",
        &format!("={}", session),
        "-F",
        "#{pane_id}|#{pane_current_command}",
    ])
    .map(|s| {
        s.lines()
            .filter_map(|l| {
                let mut it = l.splitn(2, '|');
                Some(PaneInfo {
                    id: it.next()?.into(),
                    command: it.next().unwrap_or("").into(),
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

pub fn capture_pane(pane: &str, lines: u32) -> Option<String> {
    run(&["capture-pane", "-p", "-t", pane, "-S", &format!("-{}", lines)]).ok()
}

pub fn send_keys(pane: &str, text: &str) {
    let _ = run(&["send-keys", "-t", pane, text, "Enter"]);
}

pub fn new_session_detached(session: &str, window: &str, cwd: &Path, cmd: &str) -> Result<()> {
    run(&[
        "new-session",
        "-d",
        "-s",
        session,
        "-n",
        window,
        "-c",
        &cwd.to_string_lossy(),
        cmd,
    ])
    .map(|_| ())
}

pub fn new_window(session: &str, window: &str, cwd: &Path, cmd: &str) -> Result<()> {
    run(&[
        "new-window",
        "-d",
        "-t",
        &format!("={}", session),
        "-n",
        window,
        "-c",
        &cwd.to_string_lossy(),
        cmd,
    ])
    .map(|_| ())
}

pub fn kill_window(session: &str, window: &str) -> bool {
    ok(&["kill-window", "-t", &format!("{}:{}", session, window)])
}

pub fn kill_session(session: &str) -> bool {
    ok(&["kill-session", "-t", &format!("={}", session)])
}

pub fn list_sessions() -> Vec<String> {
    run(&["list-sessions", "-F", "#{session_name}"])
        .map(|s| s.lines().map(|l| l.to_string()).collect())
        .unwrap_or_default()
}

pub fn has_clients(session: &str) -> bool {
    run(&["list-clients", "-t", &format!("={}", session)])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

pub fn setenv(session: Option<&str>, var: &str, value: &str) {
    match session {
        Some(s) => {
            let _ = run(&["setenv", "-t", &format!("={}", s), var, value]);
        }
        None => {
            let _ = run(&["setenv", "-g", var, value]);
        }
    }
}

pub fn select_window(target: &str) {
    let _ = run(&["select-window", "-t", target]);
}

/// Creates a "grouped" session: shared windows, independent focus.
pub fn new_grouped_session(base: &str, alias: &str) -> Result<()> {
    run(&["new-session", "-d", "-t", base, "-s", alias]).map(|_| ())
}

/// Attaches by replacing this process. `takeover` evicts other clients.
pub fn attach_exec(session: &str, takeover: bool) -> ! {
    use std::os::unix::process::CommandExt;
    let target = format!("={}", session);
    let mut c = tmux();
    c.arg("attach");
    if takeover {
        c.arg("-d");
    }
    c.args(["-t", &target]);
    let err = c.exec();
    eprintln!("Could not attach to tmux: {}", err);
    std::process::exit(1);
}
// Note: tmux sanitizes the output of -F and turns tabs into '_', which is
// why the field separator is '|' and not a tab.
