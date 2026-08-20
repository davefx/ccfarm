use crate::{paths, tmux};
use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Claude Code messages that signal a failure recoverable with
/// /remote-control.
const FAILURE: &[&str] = &[
    "run /remote-control to reconnect",
    "could not reach the remote control server",
    "previous session is unavailable",
    "couldn't reconnect to your remote control session",
    "could not verify the signed-in account",
];

/// Cases where reconnecting would STEAL the session from another device.
/// The documentation is explicit: it must only be done on purpose.
const SKIP: &[&str] = &[
    "took over",
    "taken over",
    "ended or archived",
    "can't find the session",
    "cannot find the session",
    "remote control not started here",
];

fn contains_any(text: &str, needles: &[&str]) -> bool {
    needles.iter().any(|n| text.contains(n))
}

/// WARNING: this is a heuristic. Claude Code does not publish the Remote
/// Control state in a way that is readable from outside, so the pane text
/// is what gets read. If Anthropic changes the wording, detection stops
/// working.
pub fn run(session: &str) {
    let interval = std::env::var("CCFARM_RC_INTERVAL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(90u64);
    let cooldown = std::env::var("CCFARM_RC_COOLDOWN")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300u64);

    std::fs::create_dir_all(paths::state_dir()).ok();
    println!("[rc-watch] Watching '{}' every {}s.", session, interval);

    let mut last: HashMap<String, Instant> = HashMap::new();

    loop {
        std::thread::sleep(Duration::from_secs(interval));
        if !tmux::has_session(session) {
            continue;
        }

        for pane in tmux::panes(session) {
            // Pane sitting at the shell: there is nothing to reconnect.
            if matches!(pane.command.as_str(), "bash" | "sh" | "zsh" | "fish") {
                continue;
            }
            let text = match tmux::capture_pane(&pane.id, 120) {
                Some(t) => t.to_lowercase(),
                None => continue,
            };
            if !contains_any(&text, FAILURE) {
                continue;
            }
            if contains_any(&text, SKIP) {
                println!("[rc-watch] {}: another device took it over; not reconnecting.", pane.id);
                continue;
            }
            if let Some(t) = last.get(&pane.id) {
                if t.elapsed() < Duration::from_secs(cooldown) {
                    continue;
                }
            }
            println!("[rc-watch] {}: reactivating Remote Control.", pane.id);
            tmux::send_keys(&pane.id, "/remote-control");
            last.insert(pane.id.clone(), Instant::now());
        }
    }
}
