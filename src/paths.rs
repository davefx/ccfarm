use std::path::PathBuf;

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
}

pub fn runtime_dir() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
}

/// Socket of ccfarm's own agent, installed only when the system provides
/// none. Not named `ssh-agent.socket`: see UNIT_NAME in agent.rs.
pub fn agent_own() -> PathBuf {
    runtime_dir().join("ccfarm-ssh-agent.socket")
}

/// Known persistent agent sockets, best first.
///
/// All of these are socket-activated systemd user units on current distros,
/// so they outlive the graphical session and are reachable from an incoming
/// SSH connection alike — which is all ccfarm asks of an agent. Probing for
/// them is what keeps ccfarm from installing a second agent on top of a
/// perfectly good one.
pub fn agent_candidates() -> Vec<PathBuf> {
    let rt = runtime_dir();
    let mut v = Vec::new();
    if let Ok(s) = std::env::var("CCFARM_AGENT_SOCK") {
        v.push(PathBuf::from(s));
    }
    v.push(rt.join("gcr/ssh")); // gnome-keyring, gcr-ssh-agent.socket
    v.push(rt.join("keyring/ssh")); // gnome-keyring, older layout
    v.push(rt.join("openssh_agent")); // OpenSSH, ssh-agent.socket
    v.push(agent_own()); // ours
    v.push(rt.join("ssh-agent.socket")); // ours, name used before 0.2
    v.push(rt.join("gnupg/S.gpg-agent.ssh")); // gpg-agent ssh emulation
    v
}

/// Stable link that every tmux session sees.
pub fn agent_link() -> PathBuf {
    home().join(".ssh/agent.sock")
}

pub fn config_file() -> PathBuf {
    if let Ok(p) = std::env::var("CCFARM_CONF") {
        return PathBuf::from(p);
    }
    let base = std::env::var("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".config"));
    base.join("ccfarm/sessions.toml")
}

pub fn state_dir() -> PathBuf {
    let base = std::env::var("XDG_STATE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| home().join(".local/state"));
    base.join("ccfarm")
}

/// Expands a leading `~` in a path.
pub fn expand_tilde(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        home().join(rest)
    } else if s == "~" {
        home()
    } else {
        PathBuf::from(s)
    }
}

/// Absolute path of this very executable, to re-invoke itself inside tmux.
pub fn self_exe() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("ccfarm"))
}
