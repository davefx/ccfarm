use crate::paths;
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// `ssh-add -l` returns 2 when it cannot talk to the agent, 1 when the
/// agent answers but holds no keys, and 0 when it holds some.
fn ssh_add_code(sock: &Path) -> i32 {
    Command::new("ssh-add")
        .arg("-l")
        .env("SSH_AUTH_SOCK", sock)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()
        .and_then(|s| s.code())
        .unwrap_or(2)
}

pub fn alive(sock: &Path) -> bool {
    sock.exists() && ssh_add_code(sock) != 2
}

pub fn has_keys(sock: &Path) -> bool {
    ssh_add_code(sock) == 0
}

pub fn key_count(sock: &Path) -> usize {
    Command::new("ssh-add")
        .arg("-l")
        .env("SSH_AUTH_SOCK", sock)
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).lines().count())
        .unwrap_or(0)
}

/// The best persistent agent on this machine, if there is one.
///
/// One that holds keys beats one that merely answers: a desktop commonly
/// runs several at once (gcr, OpenSSH, gpg-agent) and only one of them
/// carries the identities.
pub fn best_persistent() -> Option<PathBuf> {
    let candidates = paths::agent_candidates();
    candidates
        .iter()
        .find(|c| alive(c) && has_keys(c))
        .or_else(|| candidates.iter().find(|c| alive(c)))
        .cloned()
}

pub struct Resolved {
    /// Path the sessions must use (always the stable link).
    pub link: PathBuf,
    /// The real agent the link points at.
    pub target: PathBuf,
    pub persistent: bool,
}

/// ALWAYS prefers a persistent agent: it is the only one visible both from
/// the graphical session and from an incoming SSH connection. The socket
/// forwarded over SSH is only a last resort, because it dies when that
/// connection is closed.
pub fn resolve() -> Option<Resolved> {
    let link = paths::agent_link();

    let (target, persistent) = match best_persistent() {
        Some(t) => (t, true),
        None => {
            // Nothing persistent: fall back to whatever this shell was given,
            // then to the link itself.
            let env_sock = std::env::var("SSH_AUTH_SOCK")
                .ok()
                .map(PathBuf::from)
                .filter(|p| *p != link && alive(p));
            let t = env_sock.or_else(|| if alive(&link) { Some(link.clone()) } else { None })?;
            (t, false)
        }
    };

    if target != link {
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let _ = std::fs::remove_file(&link);
        let _ = std::os::unix::fs::symlink(&target, &link);
    }
    std::env::set_var("SSH_AUTH_SOCK", &link);

    Some(Resolved { link, target, persistent })
}

/// On-screen warnings after resolving the agent.
pub fn report(r: &Option<Resolved>) {
    match r {
        None => eprintln!("Warning: no ssh-agent is reachable (ccfarm agent-install)."),
        Some(r) => {
            if !r.persistent {
                eprintln!(
                    "Warning: ephemeral agent ({}); it will die when this session closes.",
                    r.target.display()
                );
                eprintln!("         Run 'ccfarm agent-install' to get a persistent one.");
            }
            if !has_keys(&r.link) {
                eprintln!("Warning: the agent has no keys loaded (ssh-add).");
            }
        }
    }
}

/// Deliberately NOT called `ssh-agent.service`: that is the name distros use
/// for their own socket-activated agent, and a file with that name under
/// ~/.config/systemd/user shadows it, breaking socket activation.
const UNIT_NAME: &str = "ccfarm-ssh-agent.service";

const UNIT: &str = r#"[Unit]
Description=Persistent user SSH agent (ccfarm)

[Service]
Type=simple
Environment=SSH_AUTH_SOCK=%t/ccfarm-ssh-agent.socket
ExecStartPre=-/bin/rm -f %t/ccfarm-ssh-agent.socket
ExecStart=/usr/bin/ssh-agent -D -a $SSH_AUTH_SOCK

[Install]
WantedBy=default.target
"#;

/// Never clobbers a working SSH_AUTH_SOCK, and never exports a path that does
/// not exist: a dead socket in SSH_AUTH_SOCK breaks ssh for the whole shell.
const SHELL_SNIPPET: &str = r#"# persistent ssh-agent (ccfarm)
if [ -z "$SSH_AUTH_SOCK" ] || [ ! -S "$SSH_AUTH_SOCK" ]; then
    _ccfarm_sock="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/ccfarm-ssh-agent.socket"
    [ -S "$_ccfarm_sock" ] && export SSH_AUTH_SOCK="$_ccfarm_sock"
    unset _ccfarm_sock
fi"#;

pub fn install() -> Result<()> {
    // Most desktops already ship a persistent, socket-activated agent
    // (gcr-ssh-agent.socket, ssh-agent.socket, gpg-agent-ssh.socket). When one
    // is there, ccfarm uses it as-is: installing a second one on top is what
    // breaks the standard environment.
    if let Some(found) = best_persistent() {
        println!("A persistent agent is already running:");
        println!("  {}", found.display());
        println!("ccfarm will use it. Nothing to install, nothing changed.");
        if has_keys(&found) {
            println!("Keys loaded: {}", key_count(&found));
        } else {
            println!("It holds no keys yet:  ssh-add ~/.ssh/id_ed25519");
        }
        return Ok(());
    }

    println!("No persistent agent found; installing ccfarm's own.");

    let home = paths::home();
    let unit_dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    std::fs::write(unit_dir.join(UNIT_NAME), UNIT).context("cannot write the systemd unit")?;

    let run = |args: &[&str]| {
        Command::new(args[0])
            .args(&args[1..])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };

    run(&["systemctl", "--user", "daemon-reload"]);
    if !run(&["systemctl", "--user", "enable", "--now", UNIT_NAME]) {
        eprintln!("Warning: systemctl --user failed; is there a systemd user session?");
    }
    let user = std::env::var("USER").unwrap_or_default();
    if !run(&["loginctl", "enable-linger", &user]) {
        eprintln!("Warning: enable 'linger' as root: loginctl enable-linger {}", user);
    }

    for f in [home.join(".bashrc"), home.join(".profile")] {
        if !f.exists() {
            continue;
        }
        let current = std::fs::read_to_string(&f).unwrap_or_default();
        if !current.contains("ccfarm-ssh-agent.socket") {
            use std::io::Write;
            let mut fh = std::fs::OpenOptions::new().append(true).open(&f)?;
            writeln!(fh, "\n{}", SHELL_SNIPPET)?;
        }
    }

    let sshcfg = home.join(".ssh/config");
    if sshcfg.exists() {
        let current = std::fs::read_to_string(&sshcfg).unwrap_or_default();
        if !current.to_lowercase().contains("addkeystoagent") {
            use std::io::Write;
            let mut fh = std::fs::OpenOptions::new().append(true).open(&sshcfg)?;
            writeln!(fh, "\nHost *\n    AddKeysToAgent yes")?;
        }
    }

    println!("Done. Open a NEW terminal and run:");
    println!("  ssh-add ~/.ssh/id_ed25519 && ccfarm doctor");
    Ok(())
}

/// Undoes `install()`. Only ever touches ccfarm's own unit and its own shell
/// snippet, so it cannot damage a system-provided agent.
pub fn uninstall() -> Result<()> {
    let home = paths::home();
    let unit = home.join(".config/systemd/user").join(UNIT_NAME);
    let mut touched = false;

    if unit.exists() {
        let run = |args: &[&str]| {
            Command::new(args[0])
                .args(&args[1..])
                .status()
                .map(|s| s.success())
                .unwrap_or(false)
        };
        run(&["systemctl", "--user", "disable", "--now", UNIT_NAME]);
        std::fs::remove_file(&unit).ok();
        run(&["systemctl", "--user", "daemon-reload"]);
        println!("Removed {}", unit.display());
        touched = true;
    }

    for f in [home.join(".bashrc"), home.join(".profile")] {
        let Ok(current) = std::fs::read_to_string(&f) else {
            continue;
        };
        if !current.contains("ccfarm-ssh-agent.socket") {
            continue;
        }
        let cleaned = current.replace(&format!("\n{}\n", SHELL_SNIPPET), "");
        if cleaned != current {
            std::fs::write(&f, cleaned)?;
            println!("Cleaned {}", f.display());
            touched = true;
        } else {
            eprintln!(
                "Warning: {} mentions ccfarm-ssh-agent.socket but not in the exact\n         \
                 block that was written; remove it by hand.",
                f.display()
            );
        }
    }

    let link = paths::agent_link();
    if link.symlink_metadata().is_ok() {
        std::fs::remove_file(&link).ok();
        println!("Removed {}", link.display());
        touched = true;
    }

    if !touched {
        println!("Nothing to undo: ccfarm had installed no agent.");
    } else {
        println!("Note: 'loginctl enable-linger' is left as it was (needs root to undo).");
        println!("      The AddKeysToAgent line in ~/.ssh/config, if any, is left alone.");
    }
    Ok(())
}
