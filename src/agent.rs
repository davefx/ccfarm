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

pub struct Resolved {
    /// Path the sessions must use (always the stable link).
    pub link: PathBuf,
    /// The real agent the link points at.
    pub target: PathBuf,
    pub persistent: bool,
}

/// ALWAYS prefers the persistent agent: it is the only one visible both
/// from the graphical session and from an incoming SSH connection. The
/// socket forwarded over SSH is only a last resort, because it dies when
/// that connection is closed.
pub fn resolve() -> Option<Resolved> {
    let link = paths::agent_link();
    let fixed = paths::agent_fixed();

    let mut candidates: Vec<PathBuf> = vec![fixed.clone()];
    if let Ok(s) = std::env::var("SSH_AUTH_SOCK") {
        let p = PathBuf::from(s);
        if p != link {
            candidates.push(p);
        }
    }
    candidates.push(link.clone());

    let target = candidates.into_iter().find(|c| alive(c))?;

    if target != link {
        if let Some(parent) = link.parent() {
            std::fs::create_dir_all(parent).ok();
        }
        let _ = std::fs::remove_file(&link);
        let _ = std::os::unix::fs::symlink(&target, &link);
    }
    std::env::set_var("SSH_AUTH_SOCK", &link);

    let persistent = target == fixed;
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

const UNIT: &str = r#"[Unit]
Description=Persistent user SSH agent

[Service]
Type=simple
Environment=SSH_AUTH_SOCK=%t/ssh-agent.socket
ExecStart=/usr/bin/ssh-agent -D -a $SSH_AUTH_SOCK

[Install]
WantedBy=default.target
"#;

const SHELL_LINE: &str =
    r#"export SSH_AUTH_SOCK="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}/ssh-agent.socket""#;

pub fn install() -> Result<()> {
    let home = paths::home();
    let unit_dir = home.join(".config/systemd/user");
    std::fs::create_dir_all(&unit_dir)?;
    std::fs::write(unit_dir.join("ssh-agent.service"), UNIT)
        .context("cannot write the systemd unit")?;

    let run = |args: &[&str]| {
        Command::new(args[0])
            .args(&args[1..])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };

    run(&["systemctl", "--user", "daemon-reload"]);
    if !run(&["systemctl", "--user", "enable", "--now", "ssh-agent.service"]) {
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
        if !current.contains("ssh-agent.socket") {
            use std::io::Write;
            let mut fh = std::fs::OpenOptions::new().append(true).open(&f)?;
            writeln!(fh, "\n# persistent ssh-agent (ccfarm)\n{}", SHELL_LINE)?;
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
