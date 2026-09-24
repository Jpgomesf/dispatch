# claude-harness — design

A framework around Claude Code for autonomous, long-horizon work on a dedicated
machine. Started from the CLI in one of two modes:

- **heartbeat** — a loop that periodically checks Slack and the issue tracker
  (Linear / Jira / any kanban reachable through MCP), handles quick items, and
  picks up cards to work.
- **card** — work one card whose body is a spec, end to end (branch → code →
  verify → PR → review → report back).

Everything runs headless through the Claude Agent SDK in **auto** permission
mode. No log subsystem: the runner prints one line per run to stdout; the
tracker, Slack and git are the record.

## Scope

In the repo (gittable):

1. **Runner** — Python package `harness` + `harness` CLI (`src/harness/`, `tests/`).
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

## Config (`config.toml`) — pydantic-validated

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

[sources]                 # what the heartbeat watches; free-form strings the skill interprets
slack_channels = ["C0000000000"]
tracker = "linear"        # linear | jira | other
tracker_query = "assignee:me state:Todo label:agent"

[[workspaces]]            # where card work happens; the workflow skill picks by match
name = "example-app"
path = "~/code/example-app"
match = ["EX-", "example-app"]

[send]
mode = "allowlist"        # all | allowlist | draft_only
slack_channels = ["C0000000000"]
slack_users = ["U0000000000"]
email_domains = []        # e.g. ["example.com"]
max_per_hour = 6

[tools]
deny = []                 # extra tool names/prefixes the runner always denies
```

## Runner

CLI (`harness`):

- `harness heartbeat [--interval 10m] [--once]` — loop; `--once` runs one tick
  (for cron/launchd/systemd timers).
- `harness card <ref> [--workspace NAME]` — work one card.
- `harness stop` / `harness resume` — create / remove the kill switch.
- `harness check` — validate config, print resolved paths and plugin path. No
  model call.

Each run is one Agent SDK session:

- `permission_mode="auto"`, model/effort/budget from config, `cwd` = workspace
  path (card) or state dir (heartbeat).
- Loads the repo's `plugin/` as a local plugin; user/project settings and the
  user's MCP servers (incl. claude.ai connectors) load as normal.
- Prompt is the workflow skill invocation plus a JSON context block:
  - heartbeat: `/claude-harness:workflow heartbeat` + `{now, cursors, sources, workspaces, outreach_file}`
  - card: `/claude-harness:workflow card <ref>` + `{now, ref, workspace, outreach_file}`
- Structured output (JSON schema) — the contract with the workflow skill:

```jsonc
// HeartbeatResult
{ "cursors": {"<source id>": "<opaque cursor>"},   // persisted verbatim for the next tick
  "handled": [{"source": "...", "item": "...", "action": "replied|drafted|ignored|escalated"}],
  "cards_to_work": ["<card ref>"],
  "summary": "one line" }

// CardResult
{ "ref": "...", "status": "done|blocked|failed",
  "pr_url": "... | null", "blocked_on": "... | null", "summary": "one line" }
```

Heartbeat tick: check kill switch → run triage session → persist cursors →
for up to `max_cards_per_tick` refs not already in progress, run card sessions
sequentially → persist card status. A failed session does not kill the loop;
the next tick retries with exponential backoff capped at the interval. SIGINT /
SIGTERM / kill switch exit cleanly between runs.

State (`state.json`, pydantic): `cursors`, `cards` (`ref → {status, updated_at,
pr_url}`), `sends` (timestamps of the last hour, for rate limiting). Atomic
write (temp file + rename).

### Send policy (enforced by the runner, not the model)

A PreToolUse hook classifies every tool call, subagents included (`can_use_tool` only fires for calls auto mode would ask about, so it would miss allowed sends):

- **Send tools** (Slack `send_message` / `schedule_message`; Gmail
  `send_message` / `reply` / `forward`; matched by suffix so the server prefix
  does not matter): allowed only if `send.mode == "all"`, or `allowlist` and the
  target channel/user/email domain is listed; and under `max_per_hour`.
  Otherwise denied with the message *"Not allowed to send here — create a draft
  instead."* so the agent falls back to a draft.
- **Draft tools**: always allowed.
- **`tools.deny`** entries and destructive messaging tools (trash / delete):
  denied.
- Kill switch present: every tool denied, session ends.
- Everything else: allowed (auto mode's classifier still applies).

Pure function `decide(tool_name, tool_input, policy, state, now) -> Decision`
so it is unit-tested without the SDK.

Quality bar: Python 3.12+, uv, type hints, pydantic, pathlib, ruff + mypy
(strict) + pytest, `make lint typecheck test`. SDK calls sit behind one small
module so everything else is tested without network.

## Skill pack (`plugin/`, plugin name `claude-harness`)

Self-contained — must not depend on skills installed only on the author's
machine. May reference Claude Code built-ins (`/code-review`, `/simplify`,
`/security-review`).

- **workflow** — entry point for both modes. Maps an execution onto other skills
  and subagents:
  - heartbeat: read sources since cursors → classify each item (quick reply /
    needs card / FYI / blocked-on-me) → reply or draft via `outreach` → return
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
  when to escalate. Treats a runner denial as "draft instead" — never retries a
  send another way.
- **pr-description** — generic port of the author's PR-description skill.

`examples/outreach.example.md` and `examples/config.example.toml` use fictional
people and IDs only.

## Non-goals

No log store, no dashboard, no web service, no multi-user support, no
provisioning of the machine.
