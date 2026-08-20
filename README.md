# ccfarm

Orchestrator for Claude Code sessions on top of tmux. It replaces the
`cc-farm.sh`, `cc-keepalive.sh`, `cc-view.sh` and `cc-rc-watch.sh` scripts
and the agent installer: everything in a single binary.

## Installation

```bash
cargo build --release
install -Dm755 target/release/ccfarm ~/.local/bin/ccfarm
```

The bundled `Cargo.lock` pins a set of dependencies that compiles even with
old toolchains (tested with cargo 1.75). Delete it if you would rather let
cargo resolve freely.

## Usage

| Command | What it does |
| --- | --- |
| `ccfarm` | Starts or resumes every session in the file |
| `ccfarm up -f other.toml` | Same, with a different file |
| `ccfarm attach [name]` | Attaches to tmux, **evicting** the previous client |
| `ccfarm list` | State of the sessions |
| `ccfarm stop <name>` | Closes a session and its restart loop |
| `ccfarm kill` | Closes everything |
| `ccfarm edit` | Opens the file in `$EDITOR` |
| `ccfarm doctor` | Full diagnostics |
| `ccfarm agent-install` | Installs the persistent ssh-agent |

Only `up` accepts `-f`; every other command uses the default file.

## Sessions file

`~/.config/ccfarm/sessions.toml` (created on its own the first time):

```toml
tmux_session = "cc"
remote_control_watch = true

[[session]]
path = "~/projects/api"
name = "refactor-auth"

[[session]]
path = "~/projects/api"
name = "bug-webhooks"

[[session]]
path = "~/web"          # no "name": the folder name is used
```

Several sessions per folder, each with its own name. The name is the
identifier the conversation is resumed with, so it must be unique.

## How it works

Claude processes **always** live inside tmux. The interface depends on where
`ccfarm` is run from:

- **SSH connection or no graphical environment** → it attaches to tmux in the
  current terminal, with `attach -d`, which evicts any previous client.
- **Local graphical session** → it opens one tab per session. Each tab is a
  *view* (a grouped tmux session) onto its window: closing it kills nothing,
  and the work stays reachable over SSH from somewhere else.

Forceable with `CCFARM_UI=tmux|gui`. Supported emulators: gnome-terminal,
konsole, xfce4-terminal, kitty (`CCFARM_TERM` to impose one).

Running it twice duplicates nothing: there is a file lock, only the missing
windows are created, and no tabs are opened onto sessions that already have
a client.

### Resuming sessions

The first start uses `claude -n <name>`; from then on,
`claude --resume <name>`. The "already exists" mark is written once the
session has been alive for ten seconds, not when it ends, so that an abrupt
machine reboot does not cause a new session.  If the `--resume` fails
outright — purged transcript, ambiguous name — a new session is created.

### ssh-agent

The persistent agent at `$XDG_RUNTIME_DIR/ssh-agent.socket` is **always**
preferred, because it is the only one visible both from the graphical
session and from an incoming SSH connection. The socket forwarded over SSH
is only used as a last resort, with a warning: it dies when that connection
hangs up. `ccfarm agent-install` creates the systemd user service, enables
`linger`, exports the variable in `.bashrc`/`.profile` and adds
`AddKeysToAgent yes`.

### Remote Control

Set `"remoteControlAtStartup": true` in `~/.claude/settings.json`: it applies
to new and resumed sessions alike. `ccfarm doctor` checks it, along with the
environment variables that prevent it.

The `rc-watch` watcher reactivates Remote Control when it detects a failure.
**It is a heuristic**: Claude Code does not publish that state in a way that
is readable from outside, so the pane text is read with `capture-pane`. If
Anthropic changes the wording of its messages, it will stop detecting them.
It does not reconnect when the message says another device took over the
session, so as not to steal it back.

## Environment variables

| Variable | Effect |
| --- | --- |
| `CCFARM_CONF` | Sessions file |
| `CCFARM_SESSION` | Name of the tmux session |
| `CCFARM_UI` | `tmux` or `gui` |
| `CCFARM_TERM` | Preferred terminal emulator |
| `CCFARM_RC_INTERVAL` | Seconds between watcher checks (90) |
| `CCFARM_RC_COOLDOWN` | Minimum wait between retries per pane (300) |

## Known limitations

- gnome-terminal adds the tab to the most recently focused window; there is a
  400 ms pause between openings, but if you switch windows during the process
  some tab may end up where it should not. With kitty, `kitty @ launch` is
  used, which is deterministic.
- If a session name turns out to be ambiguous, `claude --resume` opens the
  interactive picker and that window sits waiting for you to press Esc.
- Remote Control failure detection is textual, as explained above.
