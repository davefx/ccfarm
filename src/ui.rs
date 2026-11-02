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

/// A desktop to open tabs on, whoever asks: a session woken up from an
/// SSH connection (or by mem-watch) gets its tab back all the same.
pub fn desktop() -> bool {
    std::env::var("DISPLAY").is_ok() || std::env::var("WAYLAND_DISPLAY").is_ok()
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
    for t in ["kgx", "gnome-terminal", "konsole", "xfce4-terminal", "kitty"] {
        cands.push(t.to_string());
    }
    cands.into_iter().find(|t| exists(t))
}

fn running(prog: &str) -> bool {
    Command::new("pgrep")
        .args(["-x", "-u", &unsafe { libc::getuid() }.to_string(), prog])
        .stdout(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Clicks the close button of the GNOME Console tabs titled `title`,
/// through AT-SPI (python3-gi). kgx keeps a tab open, showing "process
/// exited", when its command ends, and has no option or command to close
/// one. Only called once that tab's `ccfarm view` has quit: on a live tab
/// kgx would ask for confirmation.
const CLOSE_TAB: &str = r#"
import sys, gi
gi.require_version('Atspi', '2.0')
from gi.repository import Atspi
def find(o, role, name=None):
    out = []
    def walk(o):
        try:
            r, n = o.get_role_name(), o.get_name()
        except Exception:
            return
        if r == role and (name is None or n == name):
            out.append(o)
            return
        for i in range(o.get_child_count()):
            c = o.get_child_at_index(i)
            if c:
                walk(c)
    walk(o)
    return out
d = Atspi.get_desktop(0)
for i in range(d.get_child_count()):
    app = d.get_child_at_index(i)
    if app and app.get_name() == 'kgx':
        for tab in find(app, 'page tab', sys.argv[1]):
            for b in find(tab, 'button'):
                a = b.get_action_iface()
                if a:
                    a.do_action(0)
                    break
"#;

/// Closes the tab that showed `window` once its `ccfarm view` is gone.
pub fn close_tab(title: &str, window: &str) {
    if !desktop() || detect_terminal().as_deref() != Some("kgx") || !running("kgx") {
        return;
    }
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while view_running(window) {
        if std::time::Instant::now() > deadline {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    let _ = Command::new("timeout")
        .args(["20", "python3", "-I", "-c", CLOSE_TAB, title])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

/// Some `ccfarm view <window>` is still running.
fn view_running(window: &str) -> bool {
    let Ok(dir) = std::fs::read_dir("/proc") else { return false };
    dir.flatten().any(|e| {
        let cmd = std::fs::read(e.path().join("cmdline")).unwrap_or_default();
        let args: Vec<&[u8]> = cmd.split(|b| *b == 0).filter(|a| !a.is_empty()).collect();
        args.len() >= 3 && args[args.len() - 2] == b"view" && args[args.len() - 1] == window.as_bytes()
    })
}

fn launch(cmd: &mut Command) {
    let _ = cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn();
}

/// A tab to open. `title` is what the user sees on the tab — the human
/// session name, spaces and international characters intact. `window` is the
/// sanitized tmux window the tab is a view onto, and the identifier the
/// `view` subcommand needs; it must stay ASCII.
pub struct Tab {
    pub title: String,
    pub window: String,
}

/// Single-quote a token for a POSIX shell, so gnome-terminal's `-e` (which
/// parses its argument with g_shell_parse_argv) receives it as one word.
fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn sh_join(parts: &[&str]) -> String {
    parts.iter().map(|p| sh_quote(p)).collect::<Vec<_>>().join(" ")
}

/// Opens one tab per session. Each tab is only a VIEW onto its tmux
/// window: closing it kills nothing, and the work stays reachable over
/// SSH from anywhere else.
pub fn open_tabs(term: &str, tabs: &[Tab]) {
    let exe = paths::self_exe();
    let exe = exe.to_string_lossy().to_string();

    // Skip sessions that already have a tab open, so re-running `up` does not
    // pile up duplicates.
    let pending: Vec<&Tab> = tabs
        .iter()
        .filter(|t| {
            let open = tmux::has_clients(&format!("view-{}", t.window));
            if open {
                eprintln!("Session '{}' already has a tab open.", t.title);
            }
            !open
        })
        .collect();
    if pending.is_empty() {
        return;
    }

    // gnome-terminal is a single-instance app: a separate `--tab` invocation
    // lands the tab in whichever window has focus — usually the one ccfarm was
    // launched from, not the new window the first tab created. Building the
    // whole window in ONE invocation (one `--window` group followed by N
    // `--tab` groups) puts every tab in that new window, deterministically and
    // with no focus race, so the 400 ms pause below is not needed here either.
    if term == "gnome-terminal" {
        let mut c = Command::new("gnome-terminal");
        for (i, t) in pending.iter().enumerate() {
            // gnome-terminal 3.x cannot chain several `-- COMMAND` groups in
            // one invocation: the first `--` swallows the rest of the command
            // line, so only one tab is created and it runs garbage. A
            // multi-tab window must give each tab its command through `-e`.
            // `-e` is deprecated (the notice goes to stderr, which launch()
            // discards) but it is the only per-tab command option that works.
            c.arg(if i == 0 { "--window" } else { "--tab" })
                .arg(format!("--title={}", t.title))
                .arg("-e")
                .arg(sh_join(&[exe.as_str(), "view", t.window.as_str()]));
        }
        launch(&mut c);
        return;
    }

    // With kgx already open, even the first tab joins its window.
    let mut first = !(term == "kgx" && running("kgx"));
    for t in &pending {
        let w = t.window.as_str();
        let title = t.title.as_str();
        match term {
            // GNOME Console: the first call opens a new window and `--tab`
            // adds to the focused one, which by then is that new window.
            "kgx" => {
                let mut c = Command::new("kgx");
                if !first {
                    c.arg("--tab");
                }
                c.args(["-T", title, "--", &exe, "view", w]);
                launch(&mut c);
                if first {
                    // Let the new window appear and take focus.
                    std::thread::sleep(std::time::Duration::from_millis(1000));
                }
            }
            "konsole" => {
                let mut c = Command::new("konsole");
                if !first {
                    c.arg("--new-tab");
                }
                c.args(["-p", &format!("tabtitle={}", title), "-e", &exe, "view", w]);
                launch(&mut c);
            }
            "xfce4-terminal" => {
                let mut c = Command::new("xfce4-terminal");
                if !first {
                    c.arg("--tab");
                }
                c.args(["-T", title, "-x", &exe, "view", w]);
                launch(&mut c);
            }
            "kitty" => {
                if first {
                    let mut c = Command::new("kitty");
                    c.args(["--title", title, &exe, "view", w]);
                    launch(&mut c);
                } else {
                    let remote = Command::new("kitty")
                        .args(["@", "launch", "--type=tab", "--tab-title", title, &exe, "view", w])
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if !remote {
                        let mut c = Command::new("kitty");
                        c.args(["--title", title, &exe, "view", w]);
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
        // kgx/konsole/xfce4 add the tab to the most recently focused window;
        // the pause reduces the races.
        std::thread::sleep(std::time::Duration::from_millis(400));
    }
}
