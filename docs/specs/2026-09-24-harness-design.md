# claude-harness — design

A framework around Claude Code for autonomous, long-horizon work on a dedicated
machine. Started from the CLI in one of two modes:

- **heartbeat** — a loop that periodically checks Slack and the issue tracker
  (Linear / Jira / any kanban reachable through MCP), handles quick items, and
  picks up cards to work.
- **card** — work one card whose body is a spec, end to end (branch → code →
  verify → PR → review → report back).

Everything runs headless: each run is one `claude -p` process in **auto**
permission mode. No log subsystem: the runner prints one line per run to stdout; the
tracker, Slack and git are the record.

## Scope

In the repo (gittable):

1. **Runner** — Rust crate at the repo root building the `harness` binary (`src/`).
2. **Skill pack** — a Claude Code plugin named `claude-harness` (`plugin/`).
3. **Pointers** — `docs/machine-setup.md` lists the machine-level customizations
   the user must provide (auth, MCP connectors, service manager, repo checkouts)
   and points to the tools. It creates none of them.

Out of the repo: every user-specific value (people, channels, workspaces,
repos, send policy) lives in `~/.config/claude-harness/`. The repo ships
`examples/` with fictional placeholders only — never real names or IDs.

## Paths

| What | Default | Override |
|---|---|---|
| Config | `~/.config/claude-harness/config.toml` | `--config`, `HARNESS_CONFIG` |
| Outreach directory | `~/.config/claude-harness/outreach.md` | `config.outreach_file` |
| State | `~/.local/state/claude-harness/state.json` | `config.state_dir` |
| Kill switch | `<state_dir>/STOP` (file presence) | — |
| Card worktrees | `<state_dir>/worktrees/<workspace>/<ref-slug>` | — |

## Config (`config.toml`) — serde-validated, unknown keys rejected

```toml
outreach_file = "~/.config/claude-harness/outreach.md"   # optional, default shown

[heartbeat]
interval = "10m"          # parsed duration: s/m/h
model = "sonnet"
effort = "medium"
max_budget_usd = 1.0      # per tick
max_cards_per_tick = 1

[card]
model = "claude-opus-5-5"
effort = "high"
max_budget_usd = 20.0     # per card
max_parallel = 2          # card sessions at once under `heartbeat`

[sources]                 # what the heartbeat watches; free-form strings the skill interprets
slack_channels = ["C0000000000"]
tracker = "linear"        # linear | jira | other
tracker_query = "assignee:me state:Todo label:agent"

[[workspaces]]            # where card work happens; the workflow skill picks by match
name = "example-app"
path = "~/code/example-app"
match = ["EX-", "example-app"]
```

Optional top-level keys: `state_dir` (default `~/.local/state/claude-harness`),
`plugin_dir` (default: the `plugin/` of the checkout the binary was built from).

## Runner

The runner manages the lifecycle of long-running work: when a session starts,
with which prompt, model and budget, in which checkout, how many at once, and
when everything stops. It neither reads nor sends messages and does not police
tool calls; that is Claude Code's job (skills, MCP, auto mode, the user's own
settings and permissions).

CLI (`harness`, global `--config PATH`):

- `harness heartbeat [--interval 10m] [--once]` — loop; `--once` runs one triage
  and the cards it queues, then exits (for cron/launchd/systemd timers).
- `harness card <ref> [--workspace NAME]` — work one card.
- `harness stop` / `harness resume` — create / remove the kill switch.
- `harness check` — validate config, print resolved paths and plugin path. No
  model call.

Exit codes: `0` ok; `1` run failed, kill switch present, or plugin missing
(`check`); `2` bad config, bad arguments or unknown workspace. Output: one line
per run on stdout (`<UTC time> <heartbeat|card> <status> <detail>`); no logging
subsystem.

### Sessions

Each run is one `claude` process in print mode, using documented flags only:

```sh
claude -p <prompt> --output-format json --json-schema <schema> \
  --permission-mode auto --model <model> --effort <effort> \
  --max-budget-usd <budget> --plugin-dir <plugin_dir>
```

- `cwd` = the card's checkout (see Scheduling) or the state dir (heartbeat, or a
  card with no workspace). User/project settings, permission rules, hooks and
  MCP servers (incl. claude.ai connectors) load as in any Claude Code run.
- Prompt is the workflow skill invocation plus a JSON context block:
  - heartbeat: `/claude-harness:workflow heartbeat` + `{now, cursors, sources, workspaces, outreach_file}`
  - card: `/claude-harness:workflow card <ref>` + `{now, ref, workspace, workspaces, outreach_file}`
    (`workspace.path` is the card's checkout; `workspaces` lists the configured paths)
- The runner reads the single JSON result object `claude` prints: `is_error`,
  `subtype` and `errors` for failures, `structured_output` for the result,
  `total_cost_usd` for the output line. A failed run, a missing or non-object
  `structured_output`, or one that does not parse as the result type is a
  failure.
- Structured output (JSON schema) — the contract with the workflow skill:

```jsonc
// HeartbeatResult
{ "cursors": {"<source id>": "<opaque cursor>"},   // persisted verbatim for the next tick
  "handled": [{"source": "...", "item": "...", "action": "replied|drafted|ignored|escalated"}],
  "cards_to_work": [{"ref": "<card ref>", "blocked_by": ["<card ref>"]}],  // blockers not yet done
  "summary": "one line" }

// CardResult
{ "ref": "...", "status": "done|blocked|failed",
  "pr_url": "... | null", "blocked_on": "... | null", "summary": "one line" }
```

### Scheduling

`heartbeat` first releases cards left `in_progress` by a crashed run (they
become `failed`, so triage can queue them again). Then one coordinator loop
starts triage and card sessions as separate tokio tasks, so triage keeps its
interval while cards run:

- **Triage** runs `interval` after the previous triage finished. A failed triage
  retries after 30s, doubling per consecutive failure, capped at the interval.
  Its cursors are persisted as soon as it returns.
- **Queue.** Each triage result replaces the queue: cards it no longer lists are
  dropped, cards already queued keep their place with the fresh `blocked_by`,
  cards running or `in_progress` in state are skipped, and at most
  `max_cards_per_tick` new cards join per triage.
- **Dependencies.** A queued card starts only when every `blocked_by` ref is
  `done` in state, or is unknown to state and not in the current batch (queued
  or running): an external dependency the runner cannot track, so it does not
  hold the card. A blocker that ended `blocked` or `failed` holds the card until
  a later run finishes it.
- **Parallelism.** At most `card.max_parallel` card sessions run at once.
- **Checkouts.** Parallel cards in one workspace must not share a checkout. Every
  card whose workspace is the root of a git checkout runs in its own worktree,
  `git worktree add --detach <state_dir>/worktrees/<workspace>/<ref-slug>`,
  reused when it exists so a resumed card finds its work; the workflow skill
  then branches from the up-to-date default branch as usual. When a card ends
  `done`, the runner runs `git worktree remove` without `--force`, which refuses
  (and keeps the tree) when anything is uncommitted or untracked. A workspace
  that is not a git checkout root is used as is (the skill reports it
  `blocked`). `harness card` uses the same checkout rule.
- `--once`: one triage, then its cards under the same rules; cards whose
  blockers do not finish in this run are left for the next.
- A failed session never stops the loop; a failed card is recorded `failed`.

### Stopping

- **Kill switch** (`harness stop`): checked before every triage and card start
  and polled every 5s while waiting. Once present, nothing new starts and
  running sessions are terminated gracefully (SIGTERM to the `claude` process);
  the loop exits when they have ended.
- **SIGINT / SIGTERM**: the first signal does the same; the second SIGKILLs
  running sessions; a third exits immediately. Terminated cards are recorded
  `failed`.

State (`state.json`): `cursors`, `cards` (`ref → {status, updated_at, pr_url}`,
status `in_progress|done|blocked|failed`). Every read-modify-write holds an
exclusive lock on `<state_dir>/state.lock`, since parallel card tasks and other
`harness` processes share the file; writes are atomic (temp file, fsync,
rename), so readers need no lock.

### Messaging policy lives in Claude Code

The runner does not decide who may be messaged. The user controls that outside
the repo:

- **Claude Code settings:** `permissions.deny` rules, e.g. deny
  `mcp__*__send_message` so every send becomes a draft, or deny specific send
  tools; optionally a `PreToolUse` hook for per-channel allowlists or rate
  limits (see the Claude Code hooks docs).
- **Skills:** the `outreach` skill and the user's `outreach.md` decide whom to
  contact, send vs draft, and follow-ups. A denied send becomes a draft and is
  never retried another way.
- `harness stop` ends every run.

Quality bar: Rust stable; `cargo fmt`, `cargo clippy --all-targets -D warnings`,
`cargo test`, wired as `make lint typecheck test`. The `claude` process sits
behind one `Session` trait, so the scheduler is tested with a scripted fake and
no model calls.

## Skill pack (`plugin/`, plugin name `claude-harness`)

Self-contained — must not depend on skills installed only on the author's
machine. May reference Claude Code built-ins (`/code-review`, `/simplify`,
`/security-review`).

- **workflow** — entry point for both modes. Maps an execution onto other skills
  and subagents:
  - heartbeat: read sources since cursors → classify each item (quick reply /
    needs card / FYI / blocked-on-me) → reply or draft via `outreach` → fill
    each queued card's `blocked_by` from the tracker's relations → return
    `HeartbeatResult`.
  - card: read card → resolve workspace → clarify gaps (via `outreach`, then
    mark blocked) → plan → implement with subagents (parallel where
    independent) → verify (project's typecheck/tests/lint) → `code-review` →
    fix → `pr-description` → open PR → comment on card → return `CardResult`.
  - Decides when to use subagents and which model tier; states budget-awareness
    and stop conditions (two failed fixes → mark blocked and escalate).
- **outreach** — who to contact and how, to get unblocked. Reads the user's
  `outreach.md` (roles, people, channels, preferred medium, hours, escalation
  order, voice). Decides: whom, medium, send vs draft, follow-up cadence,
  when to escalate. Treats a denied send (Claude Code permissions or hooks) as
  "draft instead" — never retries a send another way.
- **pr-description** — generic port of the author's PR-description skill.

`examples/outreach.example.md` and `examples/config.example.toml` use fictional
people and IDs only.

## Non-goals

No log store, no dashboard, no web service, no multi-user support, no
provisioning of the machine.
