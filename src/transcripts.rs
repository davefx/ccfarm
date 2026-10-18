use crate::paths;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Where Claude Code keeps the transcripts of the conversations started in
/// `dir`: `~/.claude/projects/<dir with every non-alphanumeric char as '-'>`.
fn project_dir(dir: &Path) -> PathBuf {
    let base = std::env::var("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| paths::home().join(".claude"));
    let key: String = dir
        .display()
        .to_string()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    base.join("projects").join(key)
}

/// Decodes the JSON string that starts right after an opening quote.
fn json_string(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => return Some(out),
            '\\' => match chars.next()? {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'u' => {
                    let hex: String = chars.by_ref().take(4).collect();
                    let mut cp = u32::from_str_radix(&hex, 16).ok()?;
                    // Surrogate pair: a second \uXXXX must follow.
                    if (0xD800..0xDC00).contains(&cp) {
                        if chars.next()? != '\\' || chars.next()? != 'u' {
                            return None;
                        }
                        let hex: String = chars.by_ref().take(4).collect();
                        let lo = u32::from_str_radix(&hex, 16).ok()?;
                        cp = 0x10000 + ((cp - 0xD800) << 10) + (lo - 0xDC00);
                    }
                    out.push(char::from_u32(cp)?);
                }
                other => out.push(other),
            },
            c => out.push(c),
        }
    }
    None
}

/// The current name of a conversation: the last `custom-title` record in
/// its transcript, since a `/rename` appends a new one.
fn title_of(file: &Path) -> Option<String> {
    const KEY: &str = "\"customTitle\":\"";
    let f = std::fs::File::open(file).ok()?;
    let mut title = None;
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { continue };
        if !line.contains("\"type\":\"custom-title\"") {
            continue;
        }
        if let Some(i) = line.find(KEY) {
            if let Some(t) = json_string(&line[i + KEY.len()..]) {
                title = Some(t);
            }
        }
    }
    title
}

/// Id of the most recently written conversation in `dir` named exactly
/// `name`, so `claude --resume <id>` never has to open its picker. An exact
/// match wins; failing that, one that differs only in case. Ids in `skip`
/// are ignored (they already failed to resume).
pub fn find_session(dir: &Path, name: &str, skip: &[String]) -> Option<String> {
    latest(dir, name, skip).map(|(id, _)| id)
}

/// Same lookup, also returning when that transcript was last written: the
/// moment the conversation last did anything.
pub fn latest(dir: &Path, name: &str, skip: &[String]) -> Option<(String, SystemTime)> {
    let entries = std::fs::read_dir(project_dir(dir)).ok()?;
    let mut exact: Option<(SystemTime, String)> = None;
    let mut loose: Option<(SystemTime, String)> = None;
    for e in entries.flatten() {
        let path = e.path();
        if path.extension().and_then(|x| x.to_str()) != Some("jsonl") {
            continue;
        }
        let Some(id) = path.file_stem().and_then(|s| s.to_str()).map(String::from) else {
            continue;
        };
        if skip.contains(&id) {
            continue;
        }
        let Some(title) = title_of(&path) else { continue };
        let slot = if title == name {
            &mut exact
        } else if title.to_lowercase() == name.to_lowercase() {
            &mut loose
        } else {
            continue;
        };
        let mtime = e.metadata().and_then(|m| m.modified()).unwrap_or(SystemTime::UNIX_EPOCH);
        if slot.as_ref().map_or(true, |(t, _)| mtime > *t) {
            *slot = Some((mtime, id));
        }
    }
    exact.or(loose).map(|(t, id)| (id, t))
}

/// What a conversation's transcript says about its current activity.
#[derive(Debug, Default)]
pub struct Activity {
    /// Last message from either side (tool calls and results included).
    pub last_message: Option<SystemTime>,
    /// Background tasks (shells, monitors, async agents) started at or after
    /// `since` that have not reported back yet.
    pub pending_tasks: Vec<String>,
    /// The last message came from the user side (a prompt, a tool result,
    /// a task notification) and Claude has not answered it.
    pub turn_open: bool,
    /// A `ScheduleWakeup` that has not fired yet.
    pub wakeup_pending: bool,
}

/// Seconds since the epoch of an RFC 3339 UTC stamp ("2026-10-18T04:17:56.123Z").
fn parse_stamp(s: &str) -> Option<SystemTime> {
    let n = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, m, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (hh, mm, ss) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    // Days from civil (Howard Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    let secs = days * 86400 + hh * 3600 + mm * 60 + ss;
    Some(SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(u64::try_from(secs).ok()?))
}

/// The id that follows `marker` in `line`: `[a-z0-9]+`.
fn id_after<'a>(line: &'a str, marker: &str) -> Option<&'a str> {
    let rest = &line[line.find(marker)? + marker.len()..];
    let end = rest.find(|c: char| !c.is_ascii_alphanumeric()).unwrap_or(rest.len());
    Some(&rest[..end]).filter(|id| !id.is_empty())
}

/// Reads the activity of the transcript `file`. Background tasks only count
/// when started at or after `since` (the start of the current `claude`):
/// older ones died with the process that ran them.
pub fn activity(file: &Path, since: SystemTime) -> Activity {
    let mut a = Activity::default();
    let Ok(f) = std::fs::File::open(file) else { return a };
    let mut started: Vec<String> = Vec::new();
    let mut finished: Vec<String> = Vec::new();
    let mut wake_at: Option<SystemTime> = None;
    for line in BufReader::new(f).lines() {
        let Ok(line) = line else { continue };
        let user = line.contains("\"type\":\"user\"");
        if !user && !line.contains("\"type\":\"assistant\"") {
            continue;
        }
        let ts = line
            .find("\"timestamp\":\"")
            .and_then(|i| parse_stamp(&line[i + 13..]));
        // Slash commands and other local bookkeeping get no answer.
        let local = line.contains("<command-name>")
            || line.contains("<local-command-stdout>")
            || line.contains("\"isMeta\":true")
            || line.contains("[Request interrupted by user");
        if !local {
            a.last_message = ts.or(a.last_message);
            a.turn_open = user;
        }

        if ts.map_or(false, |t| t >= since) {
            for marker in ["Command running in background with ID: ", "Monitor started (task "] {
                if let Some(id) = id_after(&line, marker) {
                    started.push(id.to_string());
                }
            }
            if line.contains("Async agent launched") {
                if let Some(id) = id_after(&line, "agentId: ") {
                    started.push(id.to_string());
                }
            }
        }
        for marker in ["<task-id>", "stopped task: "] {
            let mut rest = line.as_str();
            while let Some(id) = id_after(rest, marker) {
                finished.push(id.to_string());
                rest = &rest[rest.find(marker).unwrap() + marker.len()..];
            }
        }
        if let Some(i) = line.find("\"name\":\"ScheduleWakeup\",\"input\":{") {
            let input = &line[i..];
            let input = &input[..input.find('}').unwrap_or(input.len())];
            wake_at = if input.contains("\"stop\":true") {
                None
            } else {
                let delay = id_after(input, "\"delaySeconds\":").and_then(|d| d.parse::<u64>().ok());
                match (ts, delay) {
                    (Some(t), Some(d)) => Some(t + std::time::Duration::from_secs(d)),
                    _ => wake_at,
                }
            };
        }
    }
    a.pending_tasks = started.into_iter().filter(|id| !finished.contains(id)).collect();
    a.wakeup_pending = wake_at.map_or(false, |t| t > SystemTime::now()) ;
    a
}

/// Path of the transcript of conversation `id` started in `dir`.
pub fn transcript_path(dir: &Path, id: &str) -> PathBuf {
    project_dir(dir).join(format!("{}.jsonl", id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stamps() {
        let t = parse_stamp("2026-10-18T04:17:56.123Z").unwrap();
        assert_eq!(t.duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs(), 1792297076);
    }

    #[test]
    fn background_tasks() {
        let dir = std::env::temp_dir().join(format!("ccfarm-act-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("t.jsonl");
        let lines = [
            r#"{"type":"user","timestamp":"2026-10-18T04:00:00Z","message":{"content":"go"}}"#,
            r#"{"type":"user","timestamp":"2026-10-18T04:17:56Z","message":{"content":[{"type":"tool_result","content":"Command running in background with ID: bi2ws6te4. Output"}]}}"#,
            r#"{"type":"user","timestamp":"2026-10-18T04:18:00Z","message":{"content":[{"type":"tool_result","content":"Command running in background with ID: bdone1234. Output"}]}}"#,
            r#"{"type":"user","timestamp":"2026-10-18T04:20:00Z","message":{"content":"<task-notification> <task-id>bdone1234</task-id> <status>completed</status>"}}"#,
            r#"{"type":"assistant","timestamp":"2026-10-18T04:21:00Z","message":{"content":[{"type":"text","text":"ok"}]}}"#,
        ];
        std::fs::write(&f, lines.join("
")).unwrap();
        let a = activity(&f, SystemTime::UNIX_EPOCH);
        assert_eq!(a.pending_tasks, vec!["bi2ws6te4".to_string()]);
        assert!(!a.turn_open);
        let late = parse_stamp("2026-10-18T05:00:00Z").unwrap();
        assert!(activity(&f, late).pending_tasks.is_empty());
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn decodes_escapes() {
        assert_eq!(json_string(r#"Cuéntamo + \"x\"" rest"#).unwrap(), "Cuéntamo + \"x\"");
        assert_eq!(json_string("Emaús - App\",").unwrap(), "Emaús - App");
        assert_eq!(json_string(r#"😀""#).unwrap(), "😀");
        assert!(json_string("unterminated").is_none());
    }

    #[test]
    fn project_key() {
        assert!(project_dir(Path::new("/home/x/proyectos/sacatema.com"))
            .ends_with("projects/-home-x-proyectos-sacatema-com"));
    }
}
