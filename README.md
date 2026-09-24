# dispatch

Claude Code is already the harness: it plans, loads the right skills from
context, uses your MCP connectors and follows your permission rules. dispatch
does the rest on a dedicated machine: it watches your notifications and
trackers, points Claude at the right objective, and keeps each session alive and
recoverable. What Claude does with an objective is up to your own skills.

One Rust binary. Needs a stable Rust toolchain, `git`, and the `claude` CLI on
`PATH`; Claude Code auth and connectors come from the machine
(`docs/machine-setup.md`).

## How a run works

1. **Event.** A macOS notification (Slack), a Linear or Jira change on work
   assigned to you or in a thread you take part in, or `dispatch enqueue` lands
   in the shared store. Events are batched for `[intake].batch_window`; with
   none, a fallback sweep runs every `[triage].interval`.
2. **Triage.** One `claude -p` session gets the triage objective and the events
   as JSON, and decides what deserves attention: respond, draft, ignore, pick up
   assigned work as cards, or investigate mentions.
3. **Cards and discussions.** Each runs in its own git worktree, up to
   `[card].max_parallel` at once; a card waits for its `blocked_by` cards and is
   worked only if it is assigned to you. A discussion investigates a mention and
   answers in the thread, and never branches, commits or opens a PR.
4. **Attempts.** Every session has a wall-clock `timeout` (triage 20m,
   discussion 1h, card 3h) and is ended early when its output goes quiet
   (`idle_timeout`), it repeats the same tool call (`loop_threshold`), or it
   starts without its `required_mcp` servers. Each attempt is recorded with an
   outcome: `done`, `blocked`, `failed`, `timeout`, `stuck`, `api_error`,
   `crash`, `rate_limited`, `environment` or `needs_human`. A card cut short is
   retried in a fresh session whose context carries `previous_attempts`
   (outcome, summary, session id, new commits), so it starts where the last one
   stopped.
5. **Usage limits.** Hitting the plan's usage limit pauses every runner on the
   machine until it resets. Session starts are staggered (`start_stagger`).
6. **Escalation.** A card that reaches `[card].max_attempts` or stops making
   progress is not retried again; the next triage gets it under `escalations`,
   for Claude to tell you about.

Each prompt is one objective sentence plus that JSON context; override the
objective per mode with `objective` in config. A few fixed runner rules go in
the system prompt: return the result JSON, discussions never branch or commit,
only work cards assigned to you, the session runs unattended.

## Starter pack (optional)

`plugin/` is a Claude Code plugin, `dispatch`, with three skills Claude picks
from context: `workflow` (triage, working a card to completion, answering a
mention, using `previous_attempts` and `escalations`), `outreach` (whom to
contact and send vs draft, from your `outreach.md`) and `pr-description`.
dispatch loads it with `--plugin-dir` when it is present (the repo's `plugin/`,
or `plugin_dir` in config). Use it, fork it, or replace it with your own
skills: the runner only relies on the JSON context it sends and the result
shapes it reads back (TriageResult, CardResult, DiscussionResult).

Who may be messaged is decided by your Claude Code permission rules and hooks
(and `outreach.md` with the starter pack), not by dispatch; a denied send
becomes a draft.

## Commands

```sh
cargo install --path .              # installs ~/.cargo/bin/dispatch
dispatch check                      # validate config; print paths, plugin and source keys
dispatch heartbeat                  # long-running loop: intake events + fallback sweep
dispatch heartbeat --once           # one triage and its sessions (launchd / systemd timer)
dispatch card EX-123                # work one card now
dispatch enqueue manual "test event"
dispatch status                     # what the runners are doing now
dispatch history [EX-123]           # past sessions and their outcomes
dispatch stop                       # kill switch; `dispatch resume` removes it
```

- Config: `--config`, else `$DISPATCH_CONFIG`, else `~/.config/dispatch/config.toml`
  (`examples/config.example.toml`). Secrets: `~/.config/dispatch/secrets.env`
  (`0600`), else `$DISPATCH_SECRETS`.
- Store: `~/.local/state/dispatch/dispatch.db` (SQLite, shared by every runner on
  the machine; `$DISPATCH_DB`): events, claims, cards and their attempts, cursors. Kill
  switch `<state_dir>/STOP`; worktrees under `<state_dir>/worktrees/`.
- One runner per project, each with a unique `name`. Claims in the store keep
  runners on one machine apart; `agent:<name>` comments, labels and
  `agent/<name>/<ref>` branches do the same across machines and people.
- Output: one line per run on stdout. Exit codes: `0` ok, `1` run failed / kill
  switch, `2` bad config.
- Inspect a run with `claude --resume <session_id>` (`docs/machine-setup.md`).

Background: the practices behind timeouts, attempts and escalation are in
`~/research/2026-09-24-long-horizon-agent-runner-practices.md`; design notes in
`docs/specs/`.

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
  native binary. SQLite (`dispatch.db`) remains the intake queue and claim store.
- Slack Socket Mode source (above).

Development: `make lint typecheck test`.
