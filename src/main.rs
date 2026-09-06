//! ccfarm — orchestrator for Claude Code sessions on top of tmux.
//!
//! Claude processes ALWAYS live inside tmux. The interface depends on where
//! it is run from: over SSH it attaches in the current terminal; in a local
//! graphical session it opens one tab per session, each one a view onto its
//! tmux window.

mod agent;
mod config;
mod keepalive;
mod paths;
mod rcwatch;
mod tmux;
mod ui;

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use config::{Config, Session};
use fs2::FileExt;
use std::path::PathBuf;
use std::process::Command;

#[derive(Parser)]
#[command(
    name = "ccfarm",
    version,
    about = "Orchestrator for Claude Code sessions on top of tmux",
    disable_help_subcommand = true
)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start (or resume) every session in the file
    Up {
        /// Alternative sessions file
        #[arg(short, long)]
        file: Option<PathBuf>,
    },
    /// Attach to tmux, evicting the previous client
    #[command(alias = "a")]
    Attach {
        /// Specific session to jump to
        name: Option<String>,
    },
    /// Show the state of the sessions
    #[command(alias = "ls")]
    List,
    /// Close a session and its restart loop
    Stop { name: String },
    /// Close everything
    Kill,
    /// Open the sessions file in $EDITOR
    Edit,
    /// Diagnostics: tmux, claude, ssh-agent, Remote Control
    Doctor,
    /// Install the persistent ssh-agent as a user service
    AgentInstall,
    /// Undo agent-install (only ever touches what ccfarm itself wrote)
    AgentUninstall,
    /// (internal) View of one window, used by the tabs
    #[command(hide = true)]
    View { window: String },
    /// (internal) Loop that keeps a session alive
    #[command(hide = true)]
    Keepalive { dir: PathBuf, name: String },
    /// (internal) Remote Control watcher
    #[command(hide = true)]
    Rcwatch,
}

fn tmux_session() -> String {
    std::env::var("CCFARM_SESSION").unwrap_or_else(|_| {
        Config::load(None)
            .map(|c| c.tmux_session)
            .unwrap_or_else(|_| "cc".into())
    })
}

fn main() {
    let cli = Cli::parse();
    let cmd = cli.cmd.unwrap_or(Cmd::Up { file: None });
    if let Err(e) = execute(cmd) {
        eprintln!("{}", e);
        std::process::exit(1);
    }
}

fn execute(cmd: Cmd) -> Result<()> {
    match cmd {
        Cmd::Up { file } => up(file.as_deref()),
        Cmd::Attach { name } => attach(name.as_deref()),
        Cmd::List => list(),
        Cmd::Stop { name } => stop(&name),
        Cmd::Kill => kill(),
        Cmd::Edit => edit(),
        Cmd::Doctor => doctor(),
        Cmd::AgentInstall => agent::install(),
        Cmd::AgentUninstall => agent::uninstall(),
        Cmd::View { window } => view(&window),
        Cmd::Keepalive { dir, name } => keepalive::run(&dir, &name),
        Cmd::Rcwatch => {
            rcwatch::run(&tmux_session());
            Ok(())
        }
    }
}

/// Command tmux will run in the window: this very binary, quoted.
fn self_command(args: &[&str]) -> String {
    let exe = paths::self_exe();
    let mut parts = vec![shell_quote(&exe.to_string_lossy())];
    parts.extend(args.iter().map(|a| shell_quote(a)));
    parts.join(" ")
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

// ===================================================================
// up
// ===================================================================
fn up(file: Option<&std::path::Path>) -> Result<()> {
    if !tmux::available() {
        bail!("tmux is not installed");
    }
    let cfg = Config::load(file)?;
    let sessions: Vec<Session> = cfg.resolve();
    if sessions.is_empty() {
        bail!("none of the configured folders exists");
    }
    let session = cfg.tmux_session.clone();

    // Lock: two simultaneous starts must not create the windows at once.
    std::fs::create_dir_all(paths::state_dir())?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .open(paths::state_dir().join("farm.lock"))?;
    if lock.try_lock_exclusive().is_err() {
        bail!("another ccfarm instance is starting up; try again in a moment");
    }

    let ag = agent::resolve();
    agent::report(&ag);

    let mut existing = tmux::windows(&session);
    let mut fresh = !tmux::has_session(&session);

    for s in &sessions {
        if existing.contains(&s.window) {
            continue; // already running: left alone
        }
        let command = self_command(&["keepalive", &s.dir.to_string_lossy(), &s.name]);
        if fresh {
            tmux::new_session_detached(&session, &s.window, &s.dir, &command)?;
            fresh = false;
        } else {
            tmux::new_window(&session, &s.window, &s.dir, &command)?;
        }
        existing.push(s.window.clone());
    }

    if cfg.remote_control_watch && !existing.iter().any(|w| w == "rc-watch") {
        let command = self_command(&["rcwatch"]);
        let dir = paths::home();
        tmux::new_window(&session, "rc-watch", &dir, &command)?;
    }

    if let Some(a) = &ag {
        let l = a.link.to_string_lossy().to_string();
        tmux::setenv(Some(&session), "SSH_AUTH_SOCK", &l);
        tmux::setenv(None, "SSH_AUTH_SOCK", &l);
    }
    drop(lock);

    let tabs: Vec<ui::Tab> = sessions
        .iter()
        .map(|s| ui::Tab { title: s.name.clone(), window: s.window.clone() })
        .collect();
    if ui::mode() == ui::Mode::Gui {
        match ui::detect_terminal() {
            Some(term) => {
                ui::open_tabs(&term, &tabs);
                println!("Sessions running in tmux '{}'. From elsewhere: ccfarm attach", session);
                return Ok(());
            }
            None => eprintln!("No known terminal emulator; opening tmux here."),
        }
    }
    tmux::attach_exec(&session, true);
}

// ===================================================================
// remaining subcommands
// ===================================================================
fn attach(name: Option<&str>) -> Result<()> {
    let session = tmux_session();
    if !tmux::has_session(&session) {
        bail!("there is no '{}' session running", session);
    }
    if let Some(n) = name {
        let w = config::sanitize_window(n);
        if ui::mode() == ui::Mode::Gui {
            if let Some(term) = ui::detect_terminal() {
                let tab = ui::Tab { title: n.to_string(), window: w.clone() };
                ui::open_tabs(&term, &[tab]);
                return Ok(());
            }
        }
        tmux::select_window(&format!("{}:{}", session, w));
    }
    // -d evicts the previous client: it takes over the session
    tmux::attach_exec(&session, true);
}

fn view(window: &str) -> Result<()> {
    let session = tmux_session();
    if !tmux::has_session(&session) {
        eprintln!("Session {} does not exist", session);
        std::thread::sleep(std::time::Duration::from_secs(5));
        std::process::exit(1);
    }
    let alias = format!("view-{}", window);
    if !tmux::has_session(&alias) {
        tmux::new_grouped_session(&session, &alias)?;
    }
    tmux::select_window(&format!("{}:{}", alias, window));

    // exec is not used: on exit the view session must be cleaned up.
    let _ = Command::new("tmux")
        .args(["attach", "-t", &format!("={}", alias)])
        .status();
    tmux::kill_session(&alias);
    Ok(())
}

fn list() -> Result<()> {
    let session = tmux_session();
    let rows = tmux::window_info(&session);
    if rows.is_empty() {
        println!("No sessions running.");
        return Ok(());
    }
    println!("{:<24} {:<10} {}", "WINDOW", "PROCESS", "FOLDER");
    for f in rows {
        println!("{:<24} {:<10} {}", f.name, f.command, f.path);
    }
    Ok(())
}

fn stop(name: &str) -> Result<()> {
    let session = tmux_session();
    let w = config::sanitize_window(name);
    if tmux::kill_window(&session, &w) {
        println!("Closed '{}'.", w);
        Ok(())
    } else {
        bail!("window '{}' does not exist", w)
    }
}

fn kill() -> Result<()> {
    let session = tmux_session();
    for s in tmux::list_sessions() {
        if s.starts_with("view-") {
            tmux::kill_session(&s);
        }
    }
    if tmux::kill_session(&session) {
        println!("Everything closed.");
    } else {
        println!("There was nothing open.");
    }
    Ok(())
}

fn edit() -> Result<()> {
    let f = paths::config_file();
    if !f.exists() {
        if let Some(p) = f.parent() {
            std::fs::create_dir_all(p)?;
        }
        std::fs::write(&f, config::TEMPLATE)?;
    }
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "nano".into());
    Command::new(editor).arg(&f).status()?;
    Ok(())
}

fn doctor() -> Result<()> {
    let ok = "  [ok]";
    let ko = "  [!!]";

    println!("== Tools");
    match tmux::version() {
        Some(v) => println!("{} {}", ok, v),
        None => println!("{} tmux is missing", ko),
    }
    match Command::new("claude").arg("--version").output() {
        Ok(o) if o.status.success() => println!(
            "{} claude {}",
            ok,
            String::from_utf8_lossy(&o.stdout).lines().next().unwrap_or("")
        ),
        _ => println!("{} claude is missing (or does not answer)", ko),
    }

    println!("== ssh-agent");
    println!(
        "  SSH_AUTH_SOCK={}",
        std::env::var("SSH_AUTH_SOCK").unwrap_or_else(|_| "<empty>".into())
    );
    match agent::best_persistent() {
        Some(f) => println!("{} persistent agent at {}", ok, f.display()),
        None => println!("{} no persistent agent — run 'ccfarm agent-install'", ko),
    }
    match agent::resolve() {
        Some(r) => {
            if agent::has_keys(&r.link) {
                println!("{} keys loaded: {}", ok, agent::key_count(&r.link));
            } else {
                println!("{} no keys loaded (ssh-add ~/.ssh/id_ed25519)", ko);
            }
            if !r.persistent {
                println!("{} using an ephemeral agent: {}", ko, r.target.display());
            }
        }
        None => println!("{} no agent is reachable", ko),
    }
    if std::env::var("SSH_CONNECTION").is_ok() {
        println!("  (this shell is an SSH connection)");
    }

    println!("== Remote Control");
    let settings = paths::home().join(".claude/settings.json");
    let txt = std::fs::read_to_string(&settings).unwrap_or_default();
    if txt.replace(' ', "").contains("\"remoteControlAtStartup\":true") {
        println!("{} remoteControlAtStartup is on", ok);
    } else {
        println!("{} set \"remoteControlAtStartup\": true in {}", ko, settings.display());
    }
    for v in [
        "DISABLE_TELEMETRY",
        "DO_NOT_TRACK",
        "CLAUDE_CODE_DISABLE_NONESSENTIAL_TRAFFIC",
        "DISABLE_GROWTHBOOK",
        "ANTHROPIC_BASE_URL",
        "ANTHROPIC_API_KEY",
    ] {
        if std::env::var(v).is_ok() {
            println!("{} {} is set: it prevents Remote Control", ko, v);
        }
    }

    println!("== Sessions");
    println!("  file: {}", paths::config_file().display());
    match Config::load(None) {
        Ok(c) => {
            println!("  tmux: {}", c.tmux_session);
            for s in c.resolve() {
                println!("  - {:<24} {}", s.name, s.dir.display());
            }
        }
        Err(e) => println!("{} {}", ko, e),
    }
    println!("== Running");
    list()?;
    Ok(())
}
