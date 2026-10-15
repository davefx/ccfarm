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
    exact.or(loose).map(|(_, id)| id)
}

#[cfg(test)]
mod tests {
    use super::*;

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
