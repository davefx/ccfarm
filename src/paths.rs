use std::path::PathBuf;

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").unwrap_or_else(|_| "/root".into()))
}

pub fn runtime_dir() -> PathBuf {
    std::env::var("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(format!("/run/user/{}", unsafe { libc::getuid() })))
}

/// Socket of the persistent agent (systemd user service).
pub fn agent_fixed() -> PathBuf {
    runtime_dir().join("ssh-agent.socket")
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
