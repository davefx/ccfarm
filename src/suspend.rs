//! Suspending and restoring sessions to free memory.
//!
//! Suspending replaces the window's process (the restart loop and its
//! `claude`) with a small waiting screen, through `respawn-pane -k`. The
//! conversation is already on disk, so nothing is lost but what was running
//! at that moment, which is why busy sessions are refused. Restoring puts
//! the restart loop back, and it resumes the conversation by id. Both work
//! on windows started by older ccfarm builds, since only tmux is involved.

use crate::config::{self, Config, Session};
use crate::transcripts;
use crate::tmux::{self, Pane};
use anyhow::{bail, Result};
use std::collections::HashMap;

/// The pane runs the waiting screen rather than the restart loop.
fn is_suspended(p: &Pane) -> bool {
    p.start_command.contains("'parked'")
}

/// Why a session must not be suspended now, if it must not. Two sources:
/// the pane (Claude Code mid-turn, or background shells in its status
/// line — a heuristic on its wording, like rc-watch) and the transcript
/// (background tasks that have not reported back, a pending wake-up, or a
/// turn left unanswered), which also covers work that leaves no trace on
/// screen.
fn busy_reason(s: &Session, p: &Pane) -> Option<String> {
    if let Some(text) = tmux::capture_pane(&p.window_id, 0) {
        // A permission prompt or a question waits for you with no
        // "esc to interrupt" on screen.
        if text.contains("Do you want to proceed?") || text.contains("Esc to cancel") {
            return Some("waiting for your answer".into());
        }
        let tail: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).rev().take(6).collect();
        for l in tail {
            if l.contains("esc to interrupt") {
                return Some("working".into());
            }
            let words: Vec<&str> = l.split(|c: char| c.is_whitespace() || c == '·' || c == ',').filter(|w| !w.is_empty()).collect();
            if words.windows(2).any(|w| w[0].parse::<u32>().map_or(false, |n| n > 0) && w[1].starts_with("shell")) {
                return Some("background shells".into());
            }
        }
    }
    let a = activity_of(s, p)?;
    if !a.pending_tasks.is_empty() {
        return Some(format!("{} background task(s)", a.pending_tasks.len()));
    }
    if a.wakeup_pending {
        return Some("wake-up scheduled".into());
    }
    let recent = a.last_message.and_then(|t| t.elapsed().ok()).map_or(false, |d| d.as_secs() < 6 * 3600);
    if a.turn_open && recent {
        return Some("turn unfinished".into());
    }
    None
}

fn is_busy(s: &Session, p: &Pane) -> bool {
    busy_reason(s, p).is_some()
}

/// When the pane's current process started.
fn started(p: &Pane) -> Option<std::time::SystemTime> {
    std::fs::metadata(format!("/proc/{}", p.pid)).and_then(|m| m.modified()).ok()
}

fn activity_of(s: &Session, p: &Pane) -> Option<transcripts::Activity> {
    let (id, _) = transcripts::latest(&s.dir, &s.name, &[])?;
    let since = started(p).unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    Some(transcripts::activity(&transcripts::transcript_path(&s.dir, &id), since))
}

/// Parent of every process, from /proc.
fn parents() -> HashMap<u32, u32> {
    let mut map = HashMap::new();
    let Ok(dir) = std::fs::read_dir("/proc") else { return map };
    for e in dir.flatten() {
        let Some(pid) = e.file_name().to_str().and_then(|s| s.parse::<u32>().ok()) else { continue };
        let Ok(stat) = std::fs::read_to_string(e.path().join("stat")) else { continue };
        // The command name may hold spaces and parentheses: fields resume
        // after the last ')'. They are then: state, ppid, ...
        let Some(rest) = stat.rsplit_once(')').map(|(_, r)| r) else { continue };
        if let Some(ppid) = rest.split_whitespace().nth(1).and_then(|s| s.parse().ok()) {
            map.insert(pid, ppid);
        }
    }
    map
}

/// Resident plus swapped memory of `pid` and all its descendants, in KiB.
/// Swap counts too: what a suspended session gives back is both.
fn tree_kib(pid: u32, parents: &HashMap<u32, u32>) -> u64 {
    let mut total = 0;
    let mut stack = vec![pid];
    while let Some(p) = stack.pop() {
        if let Ok(status) = std::fs::read_to_string(format!("/proc/{}/status", p)) {
            for l in status.lines() {
                if l.starts_with("VmRSS:") || l.starts_with("VmSwap:") {
                    total += l.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                }
            }
        }
        stack.extend(parents.iter().filter(|(_, pp)| **pp == p).map(|(c, _)| *c));
    }
    total
}

fn mib(kib: u64) -> String {
    format!("{} MB", kib / 1024)
}

/// The windows of the configured sessions, with their session.
fn farm_of(cfg: &Config) -> Vec<(Session, Pane)> {
    let panes = tmux::panes_full(&crate::tmux_session());
    cfg.resolve()
        .into_iter()
        .filter_map(|s| {
            let p = panes.iter().find(|p| p.window_name == s.window)?.clone();
            Some((s, p))
        })
        .collect()
}

fn farm() -> Result<Vec<(Session, Pane)>> {
    Ok(farm_of(&Config::load(None)?))
}

fn pick<'a>(farm: &'a [(Session, Pane)], name: &str) -> Result<&'a (Session, Pane)> {
    let w = config::sanitize_window(name);
    farm.iter()
        .find(|(s, _)| s.name == name || s.window == w)
        .ok_or_else(|| anyhow::anyhow!("no running session called '{}'", name))
}

/// Seconds since the conversation's last message. Not the transcript's
/// modification time: resuming a conversation writes to it too, so after a
/// reboot every session would look freshly used.
fn idle_secs(s: &Session, p: &Pane) -> u64 {
    let last = activity_of(s, p)
        .and_then(|a| a.last_message)
        .or_else(|| transcripts::latest(&s.dir, &s.name, &[]).map(|(_, t)| t))
        .or_else(|| started(p));
    last.and_then(|t| t.elapsed().ok()).map_or(0, |d| d.as_secs())
}

/// Seconds since the pane's process started: a session just created or
/// restored gets a grace period before mem-watch may take it again, so an
/// order on its way (`ccfarm send`) is not lost.
fn age_secs(p: &Pane) -> u64 {
    started(p).and_then(|t| t.elapsed().ok()).map_or(0, |d| d.as_secs())
}

fn ago(secs: u64) -> String {
    match secs {
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86400 => format!("{}h{:02}m", s / 3600, s % 3600 / 60),
        s => format!("{}d", s / 86400),
    }
}

pub fn list() -> Result<()> {
    let farm = farm()?;
    if farm.is_empty() {
        println!("No sessions running.");
        return Ok(());
    }
    let parents = parents();
    println!("{:<44} {:>8} {:>7}  {}", "SESSION", "MEMORY", "IDLE", "STATE");
    let mut total = 0;
    for (s, p) in &farm {
        let state = if is_suspended(p) {
            "suspended".to_string()
        } else {
            busy_reason(s, p).map_or("idle".into(), |r| format!("busy: {}", r))
        };
        let kib = tree_kib(p.pid, &parents);
        total += kib;
        println!("{:<44} {:>8} {:>7}  {}", s.name, mib(kib), ago(idle_secs(s, p)), state);
    }
    println!("{:<44} {:>8}", "", mib(total));
    Ok(())
}

pub fn suspend(names: &[String], idle: bool, force: bool) -> Result<()> {
    if names.is_empty() && !idle {
        bail!("name the sessions to suspend, or pass --idle for every idle one");
    }
    let farm = farm()?;
    let caller = tmux::caller_window();
    let targets: Vec<&(Session, Pane)> = if idle {
        farm.iter().collect()
    } else {
        names.iter().map(|n| pick(&farm, n)).collect::<Result<_>>()?
    };

    let parents = parents();
    let mut freed = 0;
    for (s, p) in targets {
        if is_suspended(p) {
            if !idle {
                println!("{}: already suspended.", s.name);
            }
            continue;
        }
        // Suspending the caller would kill the very command doing it.
        if caller.as_deref() == Some(p.window_id.as_str()) {
            if !idle {
                println!("{}: that is this session; not suspending it.", s.name);
            }
            continue;
        }
        if let Some(why) = busy_reason(s, p).filter(|_| !force) {
            println!("{}: busy ({}), left alone (--force to suspend it anyway).", s.name, why);
            continue;
        }
        let kib = park(s, p, &parents)?;
        freed += kib;
        println!("{}: suspended, ~{} freed.", s.name, mib(kib));
    }
    if freed > 0 {
        println!("Total freed: ~{}.", mib(freed));
    }
    Ok(())
}

/// Swaps the window's process for the waiting screen; returns the memory
/// it held.
fn park(s: &Session, p: &Pane, parents: &HashMap<u32, u32>) -> Result<u64> {
    let kib = tree_kib(p.pid, parents);
    tmux::respawn_pane(&p.window_id, &crate::self_command(&["parked", &s.name]))?;
    Ok(kib)
}

pub fn restore(names: &[String], all: bool) -> Result<()> {
    if names.is_empty() && !all {
        bail!("name the sessions to restore, or pass --all");
    }
    let farm = farm()?;
    let targets: Vec<&(Session, Pane)> = if all {
        farm.iter().filter(|(_, p)| is_suspended(p)).collect()
    } else {
        names.iter().map(|n| pick(&farm, n)).collect::<Result<_>>()?
    };
    for (s, p) in targets {
        if !is_suspended(p) {
            println!("{}: not suspended.", s.name);
            continue;
        }
        tmux::respawn_pane(&p.window_id, &crate::keepalive_command(s))?;
        println!("{}: restored.", s.name);
    }
    Ok(())
}

// ===================================================================
// mem-watch: automatic suspension
// ===================================================================

/// PSI "full" avg60 of the user's systemd manager cgroup, the one
/// systemd-oomd watches; the system-wide figure as a fallback. The 60 s
/// average, not the 10 s one: short spikes are routine and suspending on
/// them takes sessions away for nothing.
fn pressure() -> f64 {
    let uid = unsafe { libc::getuid() };
    let cg = format!(
        "/sys/fs/cgroup/user.slice/user-{0}.slice/user@{0}.service/memory.pressure",
        uid
    );
    let text = std::fs::read_to_string(&cg)
        .or_else(|_| std::fs::read_to_string("/proc/pressure/memory"))
        .unwrap_or_default();
    text.lines()
        .find(|l| l.starts_with("full"))
        .and_then(|l| l.split_whitespace().find_map(|f| f.strip_prefix("avg60=")))
        .and_then(|v| v.parse().ok())
        .unwrap_or(0.0)
}

/// MemAvailable + SwapFree, in MB.
fn available_mb() -> u64 {
    let text = std::fs::read_to_string("/proc/meminfo").unwrap_or_default();
    let field = |k: &str| {
        text.lines()
            .find(|l| l.starts_with(k))
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(0)
    };
    (field("MemAvailable:") + field("SwapFree:")) / 1024
}

fn stamp() -> String {
    let out = std::process::Command::new("date").arg("+%H:%M:%S").output();
    out.map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string()).unwrap_or_default()
}

pub fn memwatch() -> ! {
    use std::time::{Duration, Instant};
    println!("[mem-watch] Suspends idle sessions under memory pressure (see [auto_suspend]).");
    let mut last_sweep = Instant::now() - Duration::from_secs(3600);
    loop {
        let cfg = match Config::load(None) {
            Ok(c) => c,
            Err(_) => {
                std::thread::sleep(Duration::from_secs(30));
                continue;
            }
        };
        let a = cfg.auto_suspend.clone();
        if !a.enabled {
            std::thread::sleep(Duration::from_secs(30));
            continue;
        }

        let (psi, avail) = (pressure(), available_mb());
        let pressed = psi > a.max_pressure || avail < a.min_available_mb;
        let sweep = a.idle_minutes > 0 && last_sweep.elapsed() >= Duration::from_secs(60);
        if !pressed && !sweep {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        }

        // Candidates: running for a while, not busy, most idle first.
        let mut idle: Vec<(u64, Session, Pane)> = farm_of(&cfg)
            .into_iter()
            .filter(|(s, p)| s.auto_suspend && !is_suspended(p) && age_secs(p) >= 600 && !is_busy(s, p))
            .map(|(s, p)| (idle_secs(&s, &p), s, p))
            .collect();
        idle.sort_by(|x, y| y.0.cmp(&x.0));
        let parents = parents();

        if pressed {
            // One at a time, most idle first, and never one used recently:
            // freeing a session that was about to be needed is worse than
            // the pressure.
            let floor = a.min_idle_minutes * 60;
            let victim = idle.iter().find(|(t, ..)| *t >= floor);
            match victim {
                Some((t, s, p)) => match park(s, p, &parents) {
                    Ok(kib) => println!(
                        "[mem-watch] {} pressure {:.1}%, {} MB free: suspended '{}' (idle {}, ~{}).",
                        stamp(), psi, avail, s.name, ago(*t), mib(kib)
                    ),
                    Err(e) => println!("[mem-watch] could not suspend '{}': {}", s.name, e),
                },
                None => println!(
                    "[mem-watch] {} pressure {:.1}%, {} MB free, but no session is idle enough.",
                    stamp(), psi, avail
                ),
            }
            std::thread::sleep(Duration::from_secs(20));
            continue;
        }

        last_sweep = Instant::now();
        for (t, s, p) in idle.iter().filter(|(t, ..)| *t >= a.idle_minutes * 60) {
            if let Ok(kib) = park(s, p, &parents) {
                println!("[mem-watch] {} suspended '{}': idle {} (~{}).", stamp(), s.name, ago(*t), mib(kib));
            }
        }
        std::thread::sleep(Duration::from_secs(5));
    }
}

/// Claude Code is up and waiting at its prompt: the input box, a line
/// starting with '❯' between two rules, is on screen and it is not mid-turn.
fn is_ready(target: &str) -> bool {
    let Some(text) = tmux::capture_pane(target, 0) else { return false };
    let lines: Vec<&str> = text.lines().filter(|l| !l.trim().is_empty()).collect();
    !text.contains("esc to interrupt")
        && lines.windows(3).any(|w| w[0].starts_with('─') && w[1].starts_with('❯') && w[2].starts_with('─'))
}

/// Types `text` into the session in `target` once Claude Code is ready for
/// it (a restored session takes a few seconds to load its conversation).
pub fn deliver(target: &str, text: &str) -> Result<()> {
    use std::time::{Duration, Instant};
    let deadline = Instant::now() + Duration::from_secs(120);
    let mut ready_since: Option<Instant> = None;
    loop {
        if Instant::now() > deadline {
            bail!("Claude Code did not reach its prompt in time; message not sent");
        }
        // Ready on two looks in a row, so a half-drawn screen does not count.
        if is_ready(target) {
            let since = *ready_since.get_or_insert_with(Instant::now);
            if since.elapsed() >= Duration::from_millis(1500) {
                break;
            }
        } else {
            ready_since = None;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    tmux::send_literal(target, &text.replace(['\r', '\n'], " "))?;
    std::thread::sleep(Duration::from_millis(300));
    tmux::send_enter(target)
}

/// `ccfarm send`: wakes the session if needed and gives it an order.
pub fn send(name: &str, text: &str) -> Result<()> {
    let farm = farm()?;
    let (s, p) = pick(&farm, name)?;
    if is_suspended(p) {
        tmux::respawn_pane(&p.window_id, &crate::keepalive_command(s))?;
        println!("{}: restored; waiting for it to load...", s.name);
    }
    deliver(&p.window_id, text)?;
    println!("{}: message sent.", s.name);
    Ok(())
}

/// The waiting screen a suspended window shows. Whatever is typed there
/// wakes the session up and is handed to it as its next message; a bare
/// Enter just wakes it. `ccfarm restore` / `ccfarm send` do the same from
/// anywhere else.
pub fn parked(name: &str) -> Result<()> {
    use std::io::BufRead;
    print!("\x1b[2J\x1b[H");
    println!("[ccfarm] '{}' is suspended to free memory; the conversation is safe.", name);
    println!("[ccfarm] Type an order and press Enter: it wakes up and receives it.");
    println!("[ccfarm] (Enter alone just wakes it. Elsewhere: ccfarm send \"{}\" \"...\")", name);
    println!();
    let mut line = String::new();
    loop {
        line.clear();
        match std::io::stdin().lock().read_line(&mut line) {
            Ok(0) | Err(_) => loop {
                // No terminal to read from: wait for `ccfarm restore`.
                std::thread::sleep(std::time::Duration::from_secs(3600));
            },
            Ok(_) => {}
        }
        let order = line.trim();
        if !order.is_empty() {
            // Restoring kills this process, so the delivery runs from the
            // tmux server, which outlives it.
            let target = tmux::caller_window().unwrap_or_default();
            let cmd = crate::self_command(&["deliver", &target, order]);
            if let Err(e) = tmux::run_background(&cmd) {
                eprintln!("[ccfarm] {}", e);
                continue;
            }
            println!("[ccfarm] Waking up; the order follows once it has loaded.");
        }
        if let Err(e) = restore(&[name.to_string()], false) {
            eprintln!("[ccfarm] {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn mib_rounds_down() {
        assert_eq!(super::mib(2048 + 1000), "2 MB");
    }
}
