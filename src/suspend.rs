//! Suspending and restoring sessions to free memory.
//!
//! Suspending replaces the window's process (the restart loop and its
//! `claude`) with a small waiting screen, through `respawn-pane -k`. The
//! conversation is already on disk, so nothing is lost but what was running
//! at that moment, which is why busy sessions are refused. Restoring puts
//! the restart loop back, and it resumes the conversation by id. Both work
//! on windows started by older ccfarm builds, since only tmux is involved.

use crate::config::{self, Config, Session};
use crate::outbox;
use crate::transcripts;
use crate::tmux::{self, Pane};
use anyhow::{bail, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant};

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
    // A shell or monitor that died or timed out may never report back:
    // only one with a live process counts.
    if !a.pending_tasks.is_empty() && tool_shells(p.pid) > 0 {
        return Some(format!("{} background task(s)", a.pending_tasks.len()));
    }
    if !a.pending_agents.is_empty() {
        return Some(format!("{} background agent(s)", a.pending_agents.len()));
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

/// Shells Claude Code's Bash tool runs (they source its shell snapshot)
/// still alive under `pid`, background tasks and monitors included.
fn tool_shells(pid: u32) -> usize {
    let parents = parents();
    let mut count = 0;
    let mut stack = vec![pid];
    while let Some(p) = stack.pop() {
        let cmd = std::fs::read(format!("/proc/{}/cmdline", p)).unwrap_or_default();
        if String::from_utf8_lossy(&cmd).contains("/shell-snapshots/") {
            count += 1;
        }
        stack.extend(parents.iter().filter(|(_, pp)| **pp == p).map(|(c, _)| *c));
    }
    count
}

/// Whether the session was used in the last `secs`, judged cheaply: its
/// claude started then, a key was pressed in a tab showing it, or one of its
/// folder's transcripts was written then. Any of these means it cannot have
/// been idle that long (a write without a message only errs on the side of
/// keeping the session).
fn touched_within(s: &Session, p: &Pane, secs: u64) -> bool {
    let recent = |t: std::time::SystemTime| t.elapsed().map_or(true, |d| d.as_secs() < secs);
    started(p).map_or(false, recent)
        || tmux::last_input(&p.window_id).map_or(false, recent)
        || transcripts::newest_write(&s.dir).map_or(false, recent)
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

/// Seconds since the session was last used: the latest of its last
/// message, its start (a session just restored is in use, however old its
/// last message), and the last key pressed in a tab showing it (reading,
/// scrolling or typing a prompt not sent yet). Not the transcript's
/// modification time: resuming a conversation writes to it too, so after a
/// reboot every session would look freshly used.
fn idle_secs(s: &Session, p: &Pane) -> u64 {
    let message = activity_of(s, p)
        .and_then(|a| a.last_message)
        .or_else(|| transcripts::latest(&s.dir, &s.name, &[]).map(|(_, t)| t));
    // A suspended window shows the waiting screen: its start and its tabs
    // say when it was suspended or attached, not used.
    let live = if is_suspended(p) { None } else { Some(p) };
    let start = live.and_then(started);
    let input = live.and_then(|p| tmux::last_input(&p.window_id));
    let last = [message, start, input].into_iter().flatten().max();
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
    mark(&s.window, true);
    // Its tab goes too: `ccfarm view` quits when its view session is
    // killed, and the terminal closes the tab. Waking it opens it again.
    crate::ui::close_view(&s.name, &s.window);
    Ok(kib)
}

/// Suspended sessions are remembered on disk, so that after a reboot `up`
/// starts them suspended again instead of loading every one.
fn mark_path(window: &str) -> std::path::PathBuf {
    crate::paths::state_dir().join("suspended").join(window)
}

pub fn mark(window: &str, suspended: bool) {
    let path = mark_path(window);
    if suspended {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let _ = std::fs::write(&path, "");
    } else {
        let _ = std::fs::remove_file(&path);
    }
}

pub fn was_suspended(window: &str) -> bool {
    mark_path(window).exists()
}

/// Windows of the farm that are suspended now, for `up` to open no tab.
pub fn suspended_windows() -> Vec<String> {
    tmux::panes_full(&crate::tmux_session())
        .into_iter()
        .filter(is_suspended)
        .map(|p| p.window_name)
        .collect()
}

pub fn restore(names: &[String], all: bool, tabs: bool) -> Result<()> {
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
        mark(&s.window, false);
        println!("{}: restored.", s.name);
        // Its tab was closed on suspending: back it comes, at the end of the
        // terminal window (no emulator takes a position for a new tab).
        if tabs && crate::ui::desktop() {
            if let Some(term) = crate::ui::detect_terminal() {
                crate::ui::open_tabs(&term, &[crate::ui::Tab { title: s.name.clone(), window: s.window.clone() }]);
            }
        }
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

        // Candidates: running for a while, not busy, most idle first. A
        // session touched more recently than any suspension could need is
        // left out on a stat alone, before the costly checks, which read
        // transcripts of hundreds of MB: done on every session every minute,
        // they kept a core busy, and under memory pressure they hit the disk.
        let need = if pressed { a.min_idle_minutes * 60 } else { a.idle_minutes * 60 };
        let mut idle: Vec<(u64, Session, Pane)> = farm_of(&cfg)
            .into_iter()
            .filter(|(s, p)| s.auto_suspend && !is_suspended(p) && age_secs(p) >= 600)
            .filter(|(s, p)| !touched_within(s, p, need))
            .filter(|(s, p)| !is_busy(s, p))
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
    type_in(target, text)
}

/// The farm session whose window this process runs in, if any: a Claude
/// Code tool call inherits TMUX_PANE from the window ccfarm started it in.
fn caller_session(farm: &[(Session, Pane)]) -> Option<&Session> {
    let w = tmux::caller_window()?;
    farm.iter().find(|(_, p)| p.window_id == w).map(|(s, _)| s)
}

/// What a message from another session starts with, so the receiver
/// knows it is not its user talking and how to answer.
fn from_header(sender: &str) -> String {
    format!(
        "[ccfarm: message from the session \"{0}\", not from the user; reply with: ccfarm send \"{0}\" \"...\"]",
        sender
    )
}

/// How long `send` waits to report a delivery before leaving the message
/// queued: a sender must never be held up by a busy session.
const SEND_WAIT: Duration = Duration::from_secs(4);

/// `ccfarm send`: wakes the session if needed and gives it an order. Sent
/// from inside another farm session, the message says who it comes from,
/// unless `as_user` (the sender relays its user's own words). The message
/// goes through the session's outbox: if the session is busy or still
/// loading, it stays queued and is typed in as soon as the session is free.
pub fn send(name: &str, text: &str, as_user: bool) -> Result<()> {
    let farm = farm()?;
    let (s, p) = pick(&farm, name)?;
    let sender = caller_session(&farm);
    if sender.map_or(false, |c| c.window == s.window) {
        bail!("'{}' is this very session", s.name);
    }
    let text = match sender {
        Some(c) if !as_user => format!("{} {}", from_header(&c.name), text),
        _ => text.to_string(),
    };
    if is_suspended(p) {
        tmux::respawn_pane(&p.window_id, &crate::keepalive_command(s))?;
        println!("{}: restored.", s.name);
    }
    let msg = outbox::enqueue(&s.window, &text)?;
    start_flush(&s.window)?;
    let t = Instant::now();
    while t.elapsed() < SEND_WAIT {
        if !msg.exists() {
            println!("{}: message sent.", s.name);
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    println!(
        "{}: not free yet; message queued, it will be typed in as soon as the session is free.",
        s.name
    );
    Ok(())
}

/// Starts the outbox worker of `window` from the tmux server, so it
/// outlives the sender (and a parked window that restoring kills).
fn start_flush(window: &str) -> Result<()> {
    tmux::run_background(&crate::self_command(&["flush", window]))
}

/// `ccfarm flush <window>`: types the session's queued messages in, oldest
/// first, each once the session is free; wakes it up if it is suspended.
/// Runs detached, so it never fails out loud: tmux would show the error on
/// whatever screen is active. It logs instead.
pub fn flush(window: &str) -> Result<()> {
    let Some(mut lock) = outbox::lock(window) else { return Ok(()) }; // another worker has it
    let mut ready_since: Option<(String, Instant)> = None;
    loop {
        let queue = outbox::pending(window);
        let Some(msg) = queue.first() else {
            drop(lock);
            // Something queued between the look and the unlock would wait
            // for a worker that is gone: look once more.
            if outbox::pending(window).is_empty() {
                return Ok(());
            }
            match outbox::lock(window) {
                Some(l) => lock = l,
                None => return Ok(()),
            }
            continue;
        };
        if outbox::age(msg) > outbox::MAX_AGE {
            outbox::log(&format!("{}: dropped a message not delivered in 24 h: {}", window, msg.display()));
            let _ = std::fs::remove_file(msg);
            continue;
        }
        // The window can be gone for a moment (restarting, being renamed)
        // or for good; the age limit covers the second case.
        let farm = farm().unwrap_or_default();
        let Some((s, p)) = farm.iter().find(|(s, _)| s.window == window) else {
            std::thread::sleep(Duration::from_secs(5));
            continue;
        };
        if is_suspended(p) {
            if tmux::respawn_pane(&p.window_id, &crate::keepalive_command(s)).is_ok() {
                outbox::log(&format!("{}: restored to deliver a queued message", s.name));
            }
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }
        // Ready on looks 1.5 s apart, so a half-drawn screen does not count.
        if !is_ready(&p.window_id) {
            ready_since = None;
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let since = match &ready_since {
            Some((w, t)) if *w == p.window_id => *t,
            _ => {
                let now = Instant::now();
                ready_since = Some((p.window_id.clone(), now));
                now
            }
        };
        if since.elapsed() < Duration::from_millis(1500) {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        // Not while the user is typing there: keys would interleave.
        if tmux::last_input(&p.window_id)
            .and_then(|t| t.elapsed().ok())
            .map_or(false, |e| e < Duration::from_secs(4))
        {
            std::thread::sleep(Duration::from_millis(500));
            continue;
        }
        let Ok(text) = std::fs::read_to_string(msg) else {
            let _ = std::fs::remove_file(msg);
            continue;
        };
        if type_in(&p.window_id, &text).is_ok() {
            let _ = std::fs::remove_file(msg);
            outbox::log(&format!("{}: delivered a queued message", s.name));
        }
        // Give Claude Code time to show it is busy before the next one.
        ready_since = None;
        std::thread::sleep(Duration::from_secs(3));
    }
}

/// Whether `text`, typed into the session, is still sitting in its input
/// box instead of having been submitted: the input box is the last block
/// framed by two '─' rules whose first line starts with '❯'. A long text
/// may show there collapsed, as "[Pasted text …]".
fn unsent(screen: &str, text: &str) -> bool {
    let lines: Vec<&str> = screen.lines().collect();
    let Some(top) = (0..lines.len().saturating_sub(1))
        .rev()
        .find(|&i| lines[i].starts_with('─') && lines[i + 1].starts_with('❯'))
    else {
        return false;
    };
    let input: String = lines[top + 1..]
        .iter()
        .take_while(|l| !l.starts_with('─'))
        .map(|l| l.trim_start_matches('❯').trim())
        .collect::<Vec<_>>()
        .join(" ");
    if input.contains("[Pasted text") {
        return true;
    }
    // Its first words are enough, and survive the box's line wrapping.
    let head: String = text.split_whitespace().take(6).collect::<Vec<_>>().join(" ");
    !head.is_empty() && input.split_whitespace().collect::<Vec<_>>().join(" ").contains(&head)
}

/// A line of `capture-pane -e` output as (all of its visible text, the
/// part of it not drawn dim). Claude Code draws its placeholder dim.
fn visible(line: &str) -> (String, String) {
    let (mut all, mut bright, mut dim) = (String::new(), String::new(), false);
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            all.push(c);
            if !dim {
                bright.push(c);
            }
            continue;
        }
        if chars.next() != Some('[') {
            continue;
        }
        let mut params = String::new();
        for c in chars.by_ref() {
            if c.is_ascii_alphabetic() {
                if c == 'm' {
                    for p in params.split(';') {
                        match p {
                            "2" => dim = true,
                            "" | "0" | "22" => dim = false,
                            _ => {}
                        }
                    }
                }
                break;
            }
            params.push(c);
        }
    }
    (all, bright)
}

/// Whether the user has a draft in the input box: anything typed there,
/// not counting the dim placeholder. `screen` is `capture-pane -e` output.
fn draft(screen: &str) -> bool {
    let lines: Vec<(String, String)> = screen.lines().map(visible).collect();
    let Some(top) = (0..lines.len().saturating_sub(1))
        .rev()
        .find(|&i| lines[i].0.starts_with('─') && lines[i + 1].0.starts_with('❯'))
    else {
        return false;
    };
    lines[top + 1..]
        .iter()
        .take_while(|(all, _)| !all.starts_with('─'))
        .any(|(_, bright)| !bright.trim_start_matches('❯').trim().is_empty())
}

/// Types `text` and submits it. Claude Code takes a moment to absorb a
/// long text (it treats it as a paste), and an Enter sent meanwhile is
/// lost: so Enter is sent again until the text has left the input box.
///
/// A draft the user has in the box is set aside first with Claude Code's
/// stash (Ctrl+S), which puts it back by itself once the message is sent;
/// otherwise the message would be typed into the middle of it.
fn type_in(target: &str, text: &str) -> Result<()> {
    if tmux::capture_pane_escaped(target).map_or(false, |s| draft(&s)) {
        tmux::send_key(target, "C-s")?;
        let deadline = Instant::now() + Duration::from_secs(3);
        while tmux::capture_pane_escaped(target).map_or(true, |s| draft(&s)) {
            if Instant::now() > deadline {
                bail!("could not set the user's draft aside");
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }
    tmux::send_literal(target, &text.replace(['\r', '\n'], " "))?;
    std::thread::sleep(Duration::from_millis(300 + (text.len() as u64 / 10).min(1200)));
    tmux::send_enter(target)?;
    // Enter again only when the text is plainly still waiting: in the box
    // on two looks in a row. A stray Enter after the message went through
    // would send the user's draft, which Claude Code puts back at once.
    let mut seen = 0;
    for _ in 0..12 {
        std::thread::sleep(Duration::from_millis(500));
        match tmux::capture_pane(target, 0) {
            Some(screen) if unsent(&screen, text) => seen += 1,
            _ => return Ok(()),
        }
        if seen == 2 {
            tmux::send_enter(target)?;
            seen = 0;
        }
    }
    Ok(())
}

/// `ccfarm hook session-start`: a Claude Code SessionStart hook. In a farm
/// session it tells Claude which other sessions exist and how to reach
/// them; elsewhere it prints nothing. A hook, not --append-system-prompt:
/// Claude Code records a conversation's system prompt once and ignores a
/// different one on later resumes, while SessionStart runs on every start,
/// resume, /clear and compaction.
pub fn session_start_hook() -> Result<()> {
    // Claude Code waits on the hook: keep it to config and one tmux call.
    let Ok(cfg) = Config::load(None) else { return Ok(()) };
    let farm = farm_of(&cfg);
    let Some(me) = caller_session(&farm) else { return Ok(()) };
    let others: Vec<String> = cfg
        .resolve()
        .into_iter()
        .filter(|s| s.window != me.window)
        .map(|s| format!("\"{}\"", s.name))
        .collect();
    let context = format!(
        "You run inside ccfarm as the session \"{me}\". The other Claude Code sessions on this machine \
         are ccfarm sessions, not agents: SendMessage and ListAgents do not reach them. To talk to one, \
         run in Bash: ccfarm send \"<session name>\" \"<message>\" (it wakes the session up if it is \
         suspended, queues the message if the session is busy and returns at once, and the message tells \
         it you sent it). Do not wait or retry after a \"queued\" answer: it arrives when the session is free. Sessions: {list}. A message that begins with \
         \"[ccfarm: message from the session ...]\" comes from another session, not from your user: \
         answer it with ccfarm send to that session.",
        me = me.name,
        list = others.join(", "),
    );
    // A SessionStart hook's plain stdout becomes context for Claude.
    println!("{}", context);
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
            // Queued: restoring kills this process, and the outbox worker
            // runs from the tmux server, which outlives it.
            let window = config::sanitize_window(name);
            if let Err(e) = outbox::enqueue(&window, order).and_then(|_| start_flush(&window)) {
                eprintln!("[ccfarm] {}", e);
                continue;
            }
            println!("[ccfarm] Waking up; the order follows once it has loaded.");
        }
        if let Err(e) = restore(&[name.to_string()], false, true) {
            eprintln!("[ccfarm] {}", e);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::unsent;

    #[test]
    fn mib_rounds_down() {
        assert_eq!(super::mib(2048 + 1000), "2 MB");
    }

    const BAR: &str = "────────────────────────────────────────";

    fn screen(input: &[&str]) -> String {
        let mut v = vec!["● Hecho.", "", "✻ Brewed for 3s", "", BAR];
        v.extend_from_slice(input);
        v.extend_from_slice(&[BAR, "  ⏵⏵ auto mode on (shift+tab to cycle)"]);
        v.join("\n")
    }

    #[test]
    fn typed_message_left_in_the_box_is_unsent() {
        let msg = "[ccfarm: message from the session \"Wareferry\", not from the user] hola qué tal";
        let s = screen(&["❯ [ccfarm: message from the session \"Wareferry\", not from the user] hola", "  qué tal"]);
        assert!(unsent(&s, msg));
    }

    #[test]
    fn collapsed_paste_left_in_the_box_is_unsent() {
        let s = screen(&["❯ [Pasted text #1 +12 lines]"]);
        assert!(unsent(&s, "un mensaje muy largo"));
    }

    #[test]
    fn empty_box_or_placeholder_is_sent() {
        assert!(!unsent(&screen(&["❯ "]), "hola qué tal"));
        assert!(!unsent(&screen(&["❯ Try \"create a util logging.py that…\""]), "hola qué tal"));
    }

    // Input boxes as `capture-pane -e` shows them: the placeholder is dim.
    fn boxed(line: &str) -> String {
        format!("● Hecho.\n{}\n{}\n{}\n  ⏵⏵ auto mode on", BAR, line, BAR)
    }

    #[test]
    fn typed_text_is_a_draft() {
        assert!(super::draft(&boxed("\x1b[39m❯\u{a0}\x1b[39mborrador del usuario a medias")));
    }

    #[test]
    fn empty_box_or_dim_placeholder_is_no_draft() {
        assert!(!super::draft(&boxed("\x1b[39m❯\u{a0}")));
        assert!(!super::draft(&boxed("\x1b[39m❯\u{a0}\x1b[2mTry \"fix typecheck errors\"\x1b[0m")));
    }

    #[test]
    fn draft_on_a_second_line_counts() {
        let s = format!("{}\n\x1b[39m❯\u{a0}\n  segunda línea\n{}\n", BAR, BAR);
        assert!(super::draft(&s));
    }

    #[test]
    fn message_in_the_history_above_is_sent() {
        let msg = "hola qué tal, esto ya se envió";
        let s = format!("❯ {}\n\n● Respuesta\n{}", msg, screen(&["❯ "]));
        assert!(!unsent(&s, msg));
    }
}
