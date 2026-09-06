use crate::paths;
use anyhow::Result;
use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Mark meaning "this session already exists on disk", so we know whether
/// to create it (`claude -n`) or resume it (`claude --resume`).
fn marker(dir: &Path, name: &str) -> PathBuf {
    let key: String = format!("{}#{}", dir.display(), name)
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    paths::state_dir().join(key)
}

/// Waits `seconds` while watching the keyboard; returns true if 'q' was
/// pressed. poll(2) is used so no thread is left blocked on stdin, which
/// would then compete with Claude for the input.
fn wait_for_key(seconds: u64) -> bool {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }
        let mut pfd = libc::pollfd { fd: 0, events: libc::POLLIN, revents: 0 };
        let ms = remaining.as_millis().min(i32::MAX as u128) as i32;
        let r = unsafe { libc::poll(&mut pfd, 1, ms) };
        if r <= 0 {
            return false;
        }
        let mut buf = [0u8; 1];
        if std::io::stdin().read(&mut buf).unwrap_or(0) == 0 {
            return false;
        }
        if buf[0] == b'q' || buf[0] == b'Q' {
            return true;
        }
    }
}

fn shell() -> ! {
    let sh = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".into());
    let err = Command::new(&sh).arg("-i").exec();
    eprintln!("Could not open the shell: {}", err);
    std::process::exit(1);
}

pub fn run(dir: &Path, name: &str, model: Option<&str>) -> Result<()> {
    std::env::set_current_dir(dir)?;
    std::fs::create_dir_all(paths::state_dir()).ok();

    let link = paths::agent_link();
    if link.exists() {
        std::env::set_var("SSH_AUTH_SOCK", &link);
    }

    let marker = marker(dir, name);
    let mut delay: u64 = 2;

    loop {
        let resume = marker.exists();
        let mut args: Vec<String> = if resume {
            vec!["--resume".into(), name.into()]
        } else {
            vec!["-n".into(), name.into()]
        };
        // Passed through untouched: `claude --model` takes an alias ("opus")
        // or a full id ("claude-opus-4-8"), and which names are valid is for
        // Claude Code to decide. Absent means Claude Code picks its default.
        if let Some(m) = model {
            args.push("--model".into());
            args.push(m.into());
        }

        let start = Instant::now();
        let rc = match Command::new("claude").args(&args).spawn() {
            Err(e) => {
                eprintln!("[ccfarm] could not run 'claude': {}", e);
                -1
            }
            Ok(mut child) => {
                // The mark is written as soon as the session has been alive
                // for a while, not when it ends: if the machine reboots
                // abruptly, next time it must be resumed, not recreated.
                let mut marked = marker.exists();
                loop {
                    match child.try_wait() {
                        Ok(Some(st)) => break st.code().unwrap_or(-1),
                        Ok(None) => {
                            if !marked && start.elapsed() >= Duration::from_secs(10) {
                                let _ = std::fs::write(&marker, "");
                                marked = true;
                            }
                            std::thread::sleep(Duration::from_millis(500));
                        }
                        Err(e) => {
                            eprintln!("[ccfarm] error waiting for claude: {}", e);
                            break -1;
                        }
                    }
                }
            }
        };
        let dur = start.elapsed().as_secs();

        // A --resume that fails outright: purged transcript or ambiguous
        // name. Start over instead of insisting on nothing.
        if resume && rc != 0 && dur < 15 {
            println!("[ccfarm] '{}': could not resume; a new session will be created.", name);
            let _ = std::fs::remove_file(&marker);
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }

        if dur >= 10 {
            delay = 2;
        } else {
            delay = (delay * 2).min(60); // brake against restart loops
        }

        println!();
        println!("[ccfarm] {}: exited with code {} after {}s.", name, rc, dur);
        println!("[ccfarm] Restarting in {}s — press 'q' + Enter to stay in the shell.", delay);
        if wait_for_key(delay) {
            println!("[ccfarm] Stopped. Run 'exit' to close the window.");
            shell();
        }
    }
}
