# claude-harness

A framework around Claude Code for autonomous, long-horizon work on a dedicated machine.
See `docs/specs/2026-09-24-harness-design.md` and, for phase 2,
`docs/specs/2026-09-24-phase2-intake-design.md`.

## Runner

A single Rust binary, `harness`. Requires a stable Rust toolchain, `git`, and the `claude`
CLI on `PATH`. Claude Code auth and MCP connectors come from the machine (see
`docs/machine-setup.md`).

```sh
cargo install --path .                  # installs ~/.cargo/bin/harness
harness check                           # validate config, print resolved paths and source keys
harness heartbeat --once                # one triage and its cards (launchd / systemd timer)
harness heartbeat                       # long-running loop: intake events + fallback sweep
harness card EX-123 --workspace example-app
harness enqueue manual "test event"     # inject an event by hand
harness stop                            # create the kill switch; `resume` removes it
```

- Config: `--config`, else `$HARNESS_CONFIG`, else `~/.config/claude-harness/config.toml`.
  Secrets: `~/.config/claude-harness/secrets.env` (`0600`), else `$HARNESS_SECRETS`.
- Store: `~/.local/state/claude-harness/harness.db` (SQLite, shared by every runner on the
  machine; `$HARNESS_DB`): intake events, claims, cards, cursors. Kill switch
  `<state_dir>/STOP`; per-card git worktrees under `<state_dir>/worktrees/`.
- Plugin: the repo's `plugin/` (the checkout `cargo install` built from) is loaded with
  `--plugin-dir`; override with `plugin_dir` in config.
- Every run is one `claude -p` process in `auto` permission mode with JSON structured output:
  `/claude-harness:workflow triage`, `card <ref>` or `discussion <ref>`, each with a JSON
  context block. Cards and discussions run up to `[card].max_parallel` at once, each card
  waiting for its `blocked_by` cards.
- Who may be messaged is up to your Claude Code permission rules and `outreach.md`, not the
  runner; a denied send becomes a draft.
- Output: one line per run on stdout. Exit codes: `0` ok, `1` run failed / kill switch,
  `2` bad config.

## Phase 2: event intake

The runner wakes on events instead of polling with a model, and several runners (one per
project, each with a unique `name`) share a machine without stepping on each other.

- **Intake:** a macOS notification watcher (Slack desktop notifications), Linear and Jira
  pollers using your personal keys, and `harness enqueue`. Events land in `harness.db`,
  are batched for `[intake].batch_window`, and start one **triage** session. With no
  events, triage still runs every `[triage].interval` (default `30m`) as a fallback sweep.
- **Personal scope:** pollers only see tickets assigned to you (`work` events, which may
  become cards) and threads you take part in (`discussion` events, which never do).
  Mentions of you and replies to you bypass every filter.
- **Discussions:** a mention that needs investigation starts a discussion session in a
  detached worktree: it may read, run and test code, never branches, commits or opens a
  PR, and ends by replying or drafting.
- **Claims:** runners on one machine hold hard claims in `harness.db`; across machines and
  people the skill posts `agent:<name>` claim comments and labels, branches as
  `agent/<name>/<ref>`, and names the runner in PR bodies.

Setup: `examples/config.example.toml`, `examples/secrets.env.example`,
`docs/machine-setup.md`.

## Recommended: Slack Socket Mode

*Future implementation target; not built yet.*

If you can install a Slack app in your company workspace, a Socket Mode app is the better
Slack source: Slack pushes real-time events (DMs, mentions, channel messages) over a
WebSocket the runner opens, so there is no notification scraping, no Full Disk Access, no
dependence on desktop notification or Focus settings, and no public URL to expose. Events
would arrive with their real channel, thread and message ids instead of a truncated
preview. The notification watcher stays for workspaces where you cannot install apps.

## Roadmap

- **Postgres "second brain".** A long-term memory the skills can query: people, cards,
  decisions and the relations between them, in Postgres with Apache AGE for the graph and
  pgvector for similarity search. Only the database runs in Docker; the runner stays a
  native binary. SQLite (`harness.db`) remains the intake queue and claim store.
- Slack Socket Mode source (above).

Development: `make lint typecheck test`.
