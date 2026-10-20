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

/// One window of the farm (they all have a single pane).
#[derive(Clone)]
pub struct Pane {
    pub window_id: String,
    pub window_name: String,
    pub pid: u32,
    pub start_command: String,
}

pub fn panes_full(session: &str) -> Vec<Pane> {
    run(&[
        "list-panes",
        "-s",
        "-t",
        &format!("={}", session),
        "-F",
        "#{window_id}|#{window_name}|#{pane_pid}|#{pane_start_command}",
    ])
    .map(|s| {
        s.lines()
            .filter_map(|l| {
                let mut it = l.splitn(4, '|');
                Some(Pane {
                    window_id: it.next()?.into(),
                    window_name: it.next()?.into(),
                    pid: it.next()?.parse().ok()?,
                    start_command: it.next().unwrap_or("").into(),
                })
            })
            .collect()
    })
    .unwrap_or_default()
}

/// Types `text` as is (no key names) into `target`.
pub fn send_literal(target: &str, text: &str) -> Result<()> {
    run(&["send-keys", "-t", target, "-l", text]).map(|_| ())
}

pub fn send_enter(target: &str) -> Result<()> {
    run(&["send-keys", "-t", target, "Enter"]).map(|_| ())
}

/// Runs a shell command from the tmux server, detached from any pane.
pub fn run_background(cmd: &str) -> Result<()> {
    run(&["run-shell", "-b", cmd]).map(|_| ())
}

/// Replaces whatever runs in `target` with `cmd`, killing it first.
pub fn respawn_pane(target: &str, cmd: &str) -> Result<()> {
    run(&["respawn-pane", "-k", "-t", target, cmd]).map(|_| ())
}

/// Window id of the tmux pane this process runs in, if any. tmux exports
/// TMUX_PANE to everything a pane starts, Claude's tool calls included.
pub fn caller_window() -> Option<String> {
    let pane = std::env::var("TMUX_PANE").ok()?;
    let out = run(&["display-message", "-p", "-t", &pane, "#{window_id}"]).ok()?;
    Some(out.trim().to_string()).filter(|s| !s.is_empty())
}

/// Last time someone pressed a key (or scrolled) in a client showing
/// window `window_id`, through any session of its group.
pub fn last_input(window_id: &str) -> Option<std::time::SystemTime> {
    run(&["list-clients", "-F", "#{window_id}|#{client_activity}"])
        .ok()?
        .lines()
        .filter_map(|l| {
            let (id, t) = l.split_once('|')?;
            (id == window_id).then(|| t.parse::<u64>().ok()).flatten()
        })
        .max()
        .map(|t| std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(t))
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

/// Id (`@N`) of the window named exactly `window`. A plain `session:name`
/// target falls back to prefix matching, so with "Cu_ntamo" gone it would
/// hit "Cu_ntamo___Habeas" instead.
pub fn window_id(session: &str, window: &str) -> Option<String> {
    run(&[
        "list-windows",
        "-t",
        &format!("={}", session),
        "-F",
        "#{window_id}|#{window_name}",
    ])
    .ok()?
    .lines()
    .find_map(|l| {
        let (id, name) = l.split_once('|')?;
        (name == window).then(|| id.to_string())
    })
}

pub fn kill_window(session: &str, window: &str) -> bool {
    match window_id(session, window) {
        Some(id) => ok(&["kill-window", "-t", &id]),
        None => false,
    }
}

/// Keeps a window in place when its process is killed by a signal (the
/// OOM killer, typically) and starts its original command again. Without
/// this the window vanishes and every view on it slides onto another
/// window of the group. A clean exit still closes the window.
pub fn respawn_on_kill(session: &str, window: &str) {
    let Some(id) = window_id(session, window) else { return };
    let _ = run(&["set-option", "-w", "-t", &id, "remain-on-exit", "on"]);
    let _ = run(&[
        "set-hook",
        "-w",
        "-t",
        &id,
        "pane-died",
        "if-shell -F '#{pane_dead_signal}' 'respawn-pane -k' 'kill-pane'",
    ]);
}

/// Ties view session `alias` to window `window_id`: the moment it would
/// show any other window (that one closed and the group moved on), its
/// clients are detached and `@ccfarm_lost` is set, so `ccfarm view` knows
/// to wait for the window instead of quitting.
pub fn pin_view(alias: &str, window_id: &str) {
    // Options and hooks take a pane target: "=name" alone does not resolve.
    let t = format!("={}:", alias);
    let _ = run(&["set-option", "-u", "-t", &t, "@ccfarm_lost"]);
    let _ = run(&[
        "set-hook",
        "-t",
        &t,
        "session-window-changed",
        &format!(
            "if-shell -F '#{{!=:#{{window_id}},{}}}' \"set-option -t '{}' @ccfarm_lost 1 ; detach-client -s '={}'\"",
            window_id, t, alias
        ),
    ]);
}

/// The window pinned by `pin_view` went away while attached.
pub fn view_lost(alias: &str) -> bool {
    run(&["show-options", "-v", "-t", &format!("={}:", alias), "@ccfarm_lost"])
        .map_or(false, |v| v.trim() == "1")
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

/// Makes `window` (exact name) the current window of `session`.
pub fn select_window(session: &str, window: &str) -> bool {
    match window_id(session, window) {
        Some(id) => run(&["select-window", "-t", &format!("={}:{}", session, id)]).is_ok(),
        None => false,
    }
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
