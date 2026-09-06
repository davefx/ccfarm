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
| `ccfarm agent-install` | Adopts a persistent ssh-agent, installing one only if needed |
| `ccfarm agent-uninstall` | Undoes `agent-install` |

Only `up` accepts `-f`; every other command uses the default file.

## Sessions file

`~/.config/ccfarm/sessions.toml` (created on its own the first time):

```toml
tmux_session = "cc"
remote_control_watch = true
model = "claude-opus-4-8"   # default model, optional

[[session]]
path = "~/projects/api"
name = "refactor-auth"

[[session]]
path = "~/projects/api"
name = "bug-webhooks"
model = "sonnet"        # overrides the default above

[[session]]
path = "~/web"          # no "name": the folder name is used
```

Several sessions per folder, each with its own name. The name is the
identifier the conversation is resumed with, so it must be unique.

The name is free text: spaces and international characters are fine
(`"Cuéntamo"`, `"Cool Candle & Co"`), and that is what the terminal tab
shows. Internally each session also gets a sanitized ASCII **window name** —
the identifier tmux and the `view` subcommand use — where anything outside
`[A-Za-z0-9_-]` becomes `_` and the result is capped at 40 characters. So two
names that differ only in punctuation can collide on the same window; ccfarm
warns and skips the repeat when they do. `ccfarm list` and `ccfarm attach
<name>` take the real name.

### Models

`model` is passed straight to `claude --model`, which takes an alias for the
latest of a family (`opus`, `sonnet`, `fable`), a full id (`claude-opus-4-8`,
`claude-sonnet-5`), or a full id with a **`[1m]` suffix** for the 1M-token
context window (`claude-opus-4-8[1m]`, `claude-sonnet-5[1m]`). ccfarm does not
check the value: which names are valid is Claude Code's business, and
hardcoding a list here would only go stale.

The square brackets are safe. Every argument ccfarm hands to tmux is
single-quoted, so `[1m]` reaches `claude` as written instead of being eaten by
shell globbing.

Three levels, most specific first:

| Where | Scope |
| --- | --- |
| `model` inside a `[[session]]` | that session |
| `CCFARM_MODEL` | the file default, for this run |
| `model` at the top of the file | every session without its own |

A session that ends up with no model at any level is started without the
flag, so Claude Code picks its own default. `CCFARM_MODEL` replaces the file
default rather than forcing every window: a session that names its own model
keeps it, because pinning one is a deliberate act.

The model applies to `claude -n` and `claude --resume` alike, so changing it
in the file and restarting the window moves an existing conversation onto
another model.

**The model is fixed when the window is created.** `ccfarm up` bakes
`--model` into the command tmux runs, and the restart loop reuses it for the
life of the window. Editing the file does not reach a session that is already
running: close it (`ccfarm stop <name>`, or `ccfarm kill` for all) and bring
it back up.

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

A *persistent* agent is *always* preferred, because it is the only one
visible both from the graphical session and from an incoming SSH connection.
The socket forwarded over SSH is only used as a last resort, with a warning:
it dies when that connection hangs up.

Persistent means any of the socket-activated agents a current distro already
ships, probed in this order:

| Socket | Agent |
| --- | --- |
| `$CCFARM_AGENT_SOCK` | whatever you point it at |
| `$XDG_RUNTIME_DIR/gcr/ssh` | gnome-keyring (`gcr-ssh-agent.socket`) |
| `$XDG_RUNTIME_DIR/keyring/ssh` | gnome-keyring, older layout |
| `$XDG_RUNTIME_DIR/openssh_agent` | OpenSSH (`ssh-agent.socket`) |
| `$XDG_RUNTIME_DIR/ccfarm-ssh-agent.socket` | ccfarm's own |
| `$XDG_RUNTIME_DIR/gnupg/S.gpg-agent.ssh` | gpg-agent ssh emulation |

One that holds keys beats one that merely answers, so on a desktop running
several at once ccfarm lands on the one actually carrying the identities.

**`ccfarm agent-install` changes nothing when it finds one of these** — it
just reports it. Only when there is no persistent agent at all does it
install its own: a `ccfarm-ssh-agent.service` user unit, `linger`, a guarded
line in `.bashrc`/`.profile` and `AddKeysToAgent yes`.

The unit is deliberately *not* called `ssh-agent.service`. A file with that
name under `~/.config/systemd/user` shadows the distro unit of the same name
and breaks its socket activation. The shell line is guarded too: it only
sets `SSH_AUTH_SOCK` when the socket really exists and the variable is not
already pointing at a working agent — exporting a dead socket path breaks
`ssh` for the whole shell.

`ccfarm agent-uninstall` reverses all of that. It only ever touches what
ccfarm itself wrote, so it cannot damage a system-provided agent. Undoing
`linger` needs root (`sudo loginctl disable-linger $USER`) and is left to
you.

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
| `CCFARM_MODEL` | Default model, overriding the file's |
| `CCFARM_UI` | `tmux` or `gui` |
| `CCFARM_TERM` | Preferred terminal emulator |
| `CCFARM_AGENT_SOCK` | ssh-agent socket to prefer over the probed ones |
| `CCFARM_RC_INTERVAL` | Seconds between watcher checks (90) |
| `CCFARM_RC_COOLDOWN` | Minimum wait between retries per pane (300) |

## Known limitations

- konsole and xfce4-terminal add each tab to the most recently focused
  window; there is a 400 ms pause between openings, but if you switch windows
  during the process some tab may end up where it should not. gnome-terminal
  and kitty are deterministic: gnome-terminal builds the whole window in a
  single invocation (one `--window` group plus one `--tab` group per session),
  and kitty uses `kitty @ launch`. A caveat for gnome-terminal: because a
  window is built in one shot, re-running `up` after some tabs are already
  open puts the *newly added* sessions in a second window — CLI gnome-terminal
  cannot inject a tab into a specific pre-existing window.
- If a session name turns out to be ambiguous, `claude --resume` opens the
  interactive picker and that window sits waiting for you to press Esc.
- Remote Control failure detection is textual, as explained above.
