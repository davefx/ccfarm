//! Messages waiting for a session that cannot take them yet: busy, still
//! loading, or suspended. `ccfarm send` leaves each message here, one file
//! per message, and a background worker (`ccfarm flush`) types them in, in
//! order, as soon as the session is free. The sender never waits on that.

use crate::paths;
use anyhow::Result;
use fs2::FileExt;
use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// A message nobody could deliver in this long is dropped (and logged):
/// by then it is stale, and the session it was for is probably gone.
pub const MAX_AGE: Duration = Duration::from_secs(24 * 3600);

fn dir(window: &str) -> PathBuf {
    paths::state_dir().join("outbox").join(window)
}

/// Queues `text` for the session in tmux window `window`. Returns the
/// message's file: it disappears once the message has been delivered.
pub fn enqueue(window: &str, text: &str) -> Result<PathBuf> {
    let d = dir(window);
    std::fs::create_dir_all(&d)?;
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let name = format!("{:024}-{}", nanos, std::process::id());
    // Written aside and renamed, so the worker never reads half a message.
    let tmp = d.join(format!(".{}", name));
    std::fs::write(&tmp, text)?;
    let path = d.join(&name);
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Queued messages for `window`, oldest first.
pub fn pending(window: &str) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(dir(window)) else { return vec![] };
    let mut v: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.file_name().and_then(|n| n.to_str()).map_or(false, |n| !n.starts_with('.')))
        .collect();
    v.sort();
    v
}

/// How long ago a message was queued.
pub fn age(msg: &PathBuf) -> Duration {
    std::fs::metadata(msg)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.elapsed().ok())
        .unwrap_or_default()
}

/// The worker's lock for `window`: one worker per session, so messages keep
/// their order. None if another worker holds it.
pub fn lock(window: &str) -> Option<File> {
    let d = dir(window);
    std::fs::create_dir_all(&d).ok()?;
    let f = std::fs::OpenOptions::new().create(true).write(true).open(d.join(".lock")).ok()?;
    f.try_lock_exclusive().ok()?;
    Some(f)
}

/// One line in the outbox log: the worker runs detached, with nowhere else
/// to report to.
pub fn log(line: &str) {
    let path = paths::state_dir().join("outbox.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let stamp = std::process::Command::new("date")
            .arg("+%F %T")
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .unwrap_or_default();
        let _ = writeln!(f, "{} {}", stamp, line);
    }
}
