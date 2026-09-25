# Setup

dispatch is one Rust binary. It starts `claude` and `git` from your `PATH`, and
uses whatever Claude Code setup the machine already has: login, connectors,
skills, permission rules. Preparing the machine and its accounts is your part;
this page covers building dispatch and pointing it at your work.

## 1. Rust

Install the stable toolchain with [rustup](https://rustup.rs). dispatch needs
Rust 1.89 or newer (edition 2024).

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
rustc --version
```

## 2. Build and install

```sh
git clone git@github.com:Jpgomesf/dispatch.git ~/rust-projects/dispatch
cd ~/rust-projects/dispatch
cargo install --path .        # installs ~/.cargo/bin/dispatch
```

To update later: `git pull && cargo install --path .`, then restart the running
`dispatch heartbeat`.

## 3. Configure

```sh
mkdir -p ~/.config/dispatch
cp examples/config.example.toml ~/.config/dispatch/config.toml
cp examples/secrets.env.example ~/.config/dispatch/secrets.env
cp examples/outreach.example.md ~/.config/dispatch/outreach.md   # used by the starter pack
chmod 600 ~/.config/dispatch/secrets.env
```

| Key | What to set |
|---|---|
| `name` | Unique per runner on the machine, e.g. the project name |
| `[[workspaces]]` | Local checkouts where cards run, and the ref prefixes that map to them |
| `[intake.notifications]` | macOS apps to watch (Slack is `com.tinyspeck.slackmacgap`) and `match` substrings that route a notification to this runner |
| `[intake] mention_names` | Your name and handles, so notifications that mention you are flagged |
| `[intake.linear]` / `[intake.jira]` | Enable the tracker you use; config can only narrow the query, never widen it past your own work |
| `[sessions] required_mcp` | MCP servers every session must have connected; a session missing one stops with `environment` |
| `secrets.env` | The API keys named by `*_env` keys. Reread on every poll; refused unless `0600` |

Every key and its default is in `examples/config.example.toml`. Run one
dispatch per project, each with its own config and `name`; they share
`~/.local/state/dispatch/dispatch.db` and never take the same card or message.

## 4. First run

```sh
dispatch check                                  # config, store, sources, plugin, required MCP
dispatch enqueue manual "Setup test: report what you would do, act on nothing."
dispatch heartbeat --once                       # one triage and its sessions
dispatch history                                # outcome, cost and session id per attempt
claude --resume <session_id>                    # see exactly what a session did
```

Then run it for real:

```sh
dispatch heartbeat          # long-running: intake, triage, cards, discussions
dispatch status             # what it is doing now
dispatch stop               # kill switch; running sessions end cleanly
dispatch resume
```

How you keep `dispatch heartbeat` running (a login item, launchd, tmux) is up
to you. Only one `heartbeat` or `card` per runner `name` runs at a time; a
second one exits with a message.

## How it works

See [How a run works](../README.md#how-a-run-works) in the README: events, triage,
cards and discussions, attempts and retries, usage-limit pauses and
escalation. Design notes are in `docs/specs/`.
