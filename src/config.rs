use crate::paths;
use anyhow::{bail, Context, Result};
use serde::Deserialize;
use std::path::{Path, PathBuf};

pub const TEMPLATE: &str = r#"# Claude Code sessions orchestrated by ccfarm.
#
# tmux_session = "cc"        # name of the tmux session (optional)
# remote_control_watch = true
# model = "opus"             # default model for every session (optional)

[[session]]
path = "~/projects/api"
name = "refactor-auth"

[[session]]
path = "~/projects/api"
name = "bug-webhooks"
model = "claude-opus-4-8"    # overrides the default above

[[session]]
path = "~/web"
# no "name": the folder name is used
# no "model": the default above, or Claude Code's own if there is none
"#;

#[derive(Debug, Deserialize)]
pub struct Config {
    #[serde(default = "default_tmux_session")]
    pub tmux_session: String,
    #[serde(default = "default_true")]
    pub remote_control_watch: bool,
    /// Default model for every session that does not name its own.
    /// Passed to `claude --model` verbatim: an alias ("opus", "sonnet") or a
    /// full id ("claude-opus-4-8"). Not validated here — the list of valid
    /// names belongs to Claude Code, not to ccfarm.
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default, rename = "session")]
    pub sessions: Vec<SessionCfg>,
}

fn default_tmux_session() -> String {
    "cc".to_string()
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct SessionCfg {
    pub path: String,
    pub name: Option<String>,
    /// Overrides the top-level `model` for this session alone.
    pub model: Option<String>,
}

/// A resolved session: absolute folder, name and tmux window.
#[derive(Debug, Clone)]
pub struct Session {
    pub dir: PathBuf,
    pub name: String,
    pub window: String,
    /// Already resolved: session override, else the file default, else None.
    pub model: Option<String>,
}

impl Config {
    pub fn load(file: Option<&Path>) -> Result<Config> {
        let path = file.map(|p| p.to_path_buf()).unwrap_or_else(paths::config_file);

        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            std::fs::write(&path, TEMPLATE)
                .with_context(|| format!("cannot create {}", path.display()))?;
            bail!(
                "Created the template {}. Add your sessions and run me again.",
                path.display()
            );
        }

        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        let mut cfg: Config = toml::from_str(&text)
            .with_context(|| format!("syntax error in {}", path.display()))?;

        if let Ok(s) = std::env::var("CCFARM_SESSION") {
            cfg.tmux_session = s;
        }
        if let Ok(m) = std::env::var("CCFARM_MODEL") {
            cfg.model = Some(m);
        }
        if cfg.sessions.is_empty() {
            bail!("no session is defined in {}", path.display());
        }
        Ok(cfg)
    }

    /// Resolves paths and names, warning about folders that do not exist.
    pub fn resolve(&self) -> Vec<Session> {
        let mut out: Vec<Session> = Vec::new();
        for s in &self.sessions {
            let dir = paths::expand_tilde(&s.path);
            let dir = match dir.canonicalize() {
                Ok(d) if d.is_dir() => d,
                _ => {
                    eprintln!("Warning: '{}' does not exist, skipping.", dir.display());
                    continue;
                }
            };
            let name = s.name.clone().unwrap_or_else(|| {
                dir.file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_else(|| "session".into())
            });
            let window = sanitize_window(&name);

            if out.iter().any(|o| o.window == window) {
                eprintln!("Warning: duplicated name '{}', skipping the repeat.", name);
                continue;
            }
            let model = s.model.clone().or_else(|| self.model.clone());
            out.push(Session { dir, name, window, model });
        }
        out
    }
}

/// tmux allows neither '.' nor ':' in window names.
pub fn sanitize_window(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .take(40)
        .collect();
    if s.is_empty() {
        "session".into()
    } else {
        s
    }
}
