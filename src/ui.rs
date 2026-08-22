use crate::{paths, tmux};
use std::process::{Command, Stdio};

#[derive(PartialEq, Debug, Clone, Copy)]
pub enum Mode {
    Tmux,
    Gui,
}

/// An incoming SSH connection never opens graphical windows, even if there
/// is a DISPLAY through X11 forwarding.
pub fn mode() -> Mode {
    match std::env::var("CCFARM_UI").as_deref() {
        Ok("gui") => return Mode::Gui,
        Ok("tmux") => return Mode::Tmux,
        _ => {}
    }
    let ssh = std::env::var("SSH_CONNECTION").is_ok() || std::env::var("SSH_TTY").is_ok();
    let graphical =
        std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok();
    if ssh || !graphical {
        Mode::Tmux
    } else {
        Mode::Gui
    }
}

fn exists(prog: &str) -> bool {
    Command::new("sh")
        .args(["-c", &format!("command -v {}", prog)])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn detect_terminal() -> Option<String> {
    let mut cands: Vec<String> = Vec::new();
    if let Ok(t) = std::env::var("CCFARM_TERM") {
        cands.push(t);
    }
    for t in ["gnome-terminal", "konsole", "xfce4-terminal", "kitty"] {
        cands.push(t.to_string());
    }
    cands.into_iter().find(|t| exists(t))
}

fn launch(cmd: &mut Command) {
    let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

/// Opens one tab per session. Each tab is only a VIEW onto its tmux
/// window: closing it kills nothing, and the work stays reachable over
/// SSH from anywhere else.
pub fn open_tabs(term: &str, windows: &[String]) {
    let exe = paths::self_exe();
    let exe = exe.to_string_lossy().to_string();
    let mut first = true;

    for w in windows {
        if tmux::has_clients(&format!("view-{}", w)) {
            eprintln!("Session '{}' already has a tab open.", w);
            first = false;
            continue;
        }
        match term {
            "gnome-terminal" => {
                let mut c = Command::new("gnome-terminal");
                c.arg(if first { "--window" } else { "--tab" })
                    .arg(format!("--title={}", w))
                    .arg("--")
                    .args([&exe, "view", w]);
                launch(&mut c);
            }
            "konsole" => {
                let mut c = Command::new("konsole");
                if !first {
                    c.arg("--new-tab");
                }
                c.args(["-p", &format!("tabtitle={}", w), "-e", &exe, "view", w]);
                launch(&mut c);
            }
            "xfce4-terminal" => {
                let mut c = Command::new("xfce4-terminal");
                if !first {
                    c.arg("--tab");
                }
                c.args(["-T", w, "-x", &exe, "view", w]);
                launch(&mut c);
            }
            "kitty" => {
                if first {
                    let mut c = Command::new("kitty");
                    c.args(["--title", w, &exe, "view", w]);
                    launch(&mut c);
                } else {
                    let remote = Command::new("kitty")
                        .args(["@", "launch", "--type=tab", "--tab-title", w, &exe, "view", w])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if !remote {
                        let mut c = Command::new("kitty");
                        c.args(["--title", w, &exe, "view", w]);
                        launch(&mut c);
                    }
                }
            }
            other => {
                eprintln!("Emulator '{}' is not supported; use CCFARM_UI=tmux.", other);
                return;
            }
        }
        first = false;
        // gnome-terminal attaches the tab to the most recently focused
        // window; the pause reduces the races.
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}
