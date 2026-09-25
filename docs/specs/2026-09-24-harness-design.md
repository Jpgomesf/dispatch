# dispatch — design

A runner around Claude Code for autonomous, long-horizon work on a dedicated
machine. Started from the CLI in one of two modes:

- **heartbeat** — a loop that watches for work (intake events, and a periodic
  sweep of Slack and the issue tracker), points a triage session at it, and runs
  the card and discussion sessions triage asks for.
- **card** — work one card whose body is a spec, end to end.

dispatch is lifecycle only. It watches for work, points Claude Code at an
objective, and keeps sessions alive, bounded and recoverable. How the work gets
done is decided by Claude with the user's own skills, MCP servers and settings;
the runner carries no messaging policy and no workflow logic.

Everything runs headless: each session is one `claude -p` process streaming JSON
events, in **auto** permission mode with nobody to answer permission prompts. No
log subsystem: the runner prints one line per session to stdout and records each
session attempt in its store; the tracker, Slack and git are the record of the
work itself.

> **Phase 2** (`2026-09-24-phase2-intake-design.md`) supersedes parts of this
> document: the heartbeat session mode is now `triage` (config `[triage]`, default
> interval `30m`, started by intake events and as a fallback sweep), a third mode
> `discussion <ref>` exists, state lives in the machine-wide `dispatch.db`, and
> the instance lock is per runner name. The sections below describe the result.

## Scope

In the repo (gittable):

1. **Runner** — Rust crate at the repo root building the `dispatch` binary (`src/`).
2. **Skill pack** — an optional Claude Code plugin named `dispatch` (`plugin/`).
3. **Setup doc** — `docs/setup.md` covers the Rust toolchain, building, configuring
   and a first run. Machine-level setup (accounts, auth, connectors, keeping the
   process running) is the user's and is not documented here.

Out of the repo: every user-specific value (people, channels, workspaces,
repos, send policy) lives in `~/.config/dispatch/`. The repo ships
`examples/` with fictional placeholders only — never real names or IDs.

## Paths

| What | Default | Override |
|---|---|---|
| Config | `~/.config/dispatch/config.toml` | `--config`, `DISPATCH_CONFIG` |
| Outreach directory | `~/.config/dispatch/outreach.md` | `config.outreach_file` |
| State dir | `~/.local/state/dispatch` | `config.state_dir` |
| Store (machine-wide) | `~/.local/state/dispatch/dispatch.db` | `DISPATCH_DB` |
| Secrets | `~/.config/dispatch/secrets.env` | `DISPATCH_SECRETS` |
| Plugin | the build checkout's `plugin/` when it exists, else none | `config.plugin_dir` |
| Instance lock | `<state_dir>/<name>.lock` | — |
| Kill switch | `<state_dir>/STOP` (file presence) | — |
| Card worktrees | `<state_dir>/worktrees/<workspace>/<ref-slug>` | — |

## Config (`config.toml`) — serde-validated, unknown keys rejected

```toml
name = "example-app"      # required; unique per machine; used in claims, branches, labels
outreach_file = "~/.config/dispatch/outreach.md"   # optional, default shown
# state_dir = "~/.local/state/dispatch"           # optional, default shown
# plugin_dir = "~/code/dispatch/plugin"           # optional, see Paths

[triage]
interval = "30m"          # fallback sweep; parsed duration: s/m/h
model = "sonnet"
effort = "medium"
max_budget_usd = 1.0      # per triage session
max_cards_per_tick = 1    # new cards queued per triage
timeout = "20m"           # wall clock per session
# objective = "..."       # default under Sessions

[card]                    # model, effort and budget also used by discussion sessions
model = "claude-opus-5-5"
effort = "high"
max_budget_usd = 20.0     # per session: one card attempt or one discussion
max_parallel = 2          # card + discussion sessions at once under `heartbeat`
timeout = "3h"            # wall clock per attempt
max_attempts = 3          # counted attempts before a card needs a person
# objective = "..."       # `{ref}` is replaced by the card ref

[discussion]
timeout = "1h"
# objective = "..."       # `{ref}` is replaced by the discussion ref

[sessions]                # shared by every session
idle_timeout = "15m"      # no stream event this long: stuck
start_stagger = "30s"     # between session starts, plus up to 50% jitter; "0s" = off
loop_threshold = 10       # identical tool calls within the last 2x: stuck; 0 = off
required_mcp = []         # MCP servers (as Claude Code names them) that must be connected

[sources]                 # what the fallback sweep reads; free-form strings the skills interpret
slack_channels = ["C0000000000"]
tracker = "linear"        # linear | jira | other
tracker_query = "assignee:me state:Todo label:agent"

# [intake] ...            # event sources: see the phase 2 spec

[[workspaces]]            # where card and discussion work happens, picked by match
name = "example-app"
path = "~/code/example-app"
match = ["EX-", "example-app"]
```

Validation: `name` matches `[a-z0-9-]+`; budgets are positive; `max_parallel`
and `max_attempts` are at least 1; objectives are not empty; durations are
positive except `start_stagger`, which may be `0s`.

## Runner

The runner manages the lifecycle of long-running work: when a session starts,
with which objective, model, budget and limits, in which checkout, how many at
once, what follows when it ends, and when everything stops. It neither reads nor
sends messages and does not police tool calls; that is Claude Code's job
(skills, MCP, auto mode, the user's own settings and permissions).

CLI (`dispatch`, global `--config PATH`):

- `dispatch heartbeat [--interval 30m] [--once]` — loop; `--once` runs one triage
  and the sessions it asks for, then exits (for cron/launchd/systemd timers).
- `dispatch card <ref> [--workspace NAME]` — work one card: a person asking for
  it, so its attempt count starts over (a `needs_human` card becomes workable).
- `dispatch stop` / `dispatch resume` — create / remove the kill switch.
- `dispatch check` — validate config, print resolved paths, the plugin in use (or
  none), the required MCP servers, the store, the secrets file and each enabled
  source's key presence (never values). No model call.
- `dispatch enqueue <source> <text>` — queue a manual event.
- `dispatch status` — read-only: this runner's running sessions (with their
  session ids), queued events, cards by status (attempts, retry time, reason),
  the machine-wide pause and the claims it holds.
- `dispatch history [ref]` — read-only: session attempts, oldest first (all of
  `ref`'s, else the last 20), each with outcome, start and end time, cost,
  `new_commits`, summary, `blocked_on`, and the command that resumes it:
  `cd '<cwd>' && claude --resume <session_id>`.

`status` and `history` open the store read-only, never create it, and take no
lock. Exit codes: `0` ok; `1` run failed, kill switch present, machine paused
(`card`), a configured plugin dir without `.claude-plugin/plugin.json`
(`check`), or an unreadable store (`status`, `history`); `2` bad config, bad
arguments or unknown workspace. Output: one line per event on stdout,
`<UTC time> <kind> <status> <detail>`, where `kind` is `triage`, `card`,
`discussion`, `intake`, `claim`, `pause` or `store` and a session's status is its
outcome (below); no logging subsystem.

### Sessions

Each session is one `claude` process in print mode, using documented flags only:

```sh
claude -p <prompt> --output-format stream-json --verbose --json-schema <schema> \
  --permission-mode auto --permission-prompts none \
  --append-system-prompt <runner rules> \
  --model <model> --effort <effort> --max-budget-usd <budget> \
  [--plugin-dir <plugin_dir>]
```

- `cwd` = the card's checkout (see Scheduling) or the state dir (triage, or a
  card with no workspace). User/project settings, permission rules, hooks,
  skills and MCP servers (incl. claude.ai connectors) load as in any Claude Code
  run. `--plugin-dir` is passed only when there is a plugin.
- `--permission-prompts none`: anything that would prompt is denied and Claude is
  told nobody can approve it, so it does not retry another way.
- **Prompt** = the mode's objective, a blank line, then the JSON context block
  (fenced as `json`). No skill is invoked by name. Objectives are configurable;
  the defaults:
  - triage: "Check the new activity below (or sweep your sources if `events` is
    empty) and decide what deserves attention: respond, draft, ignore, pick up
    assigned work as cards, or investigate mentions."
  - card: "Work card {ref} to completion in this workspace."
  - discussion: "You were mentioned in a discussion about {ref}. Investigate the
    question and respond in the thread."
- **Context** (every one carries `runner`):
  - triage: `{now, runner, cursors, sources, workspaces, outreach_file, events,
    escalations}`; `escalations: [{ref, reason, attempts, last_summary}]` lists
    the cards that need a person (`reason`: `max_attempts`, `no_progress`,
    `failed` or `environment`).
  - card: `{now, runner, ref, workspace, workspaces, outreach_file,
    previous_attempts}` (`workspace.path` is the card's checkout; `workspaces`
    lists the configured paths); `previous_attempts: [{attempt, outcome,
    summary, blocked_on, session_id, new_commits}]` holds the card's last three
    attempts, oldest first. `new_commits` is the number of commits the attempt
    added to the worktree's HEAD, `null` when the card has no worktree. A retry
    is a fresh session seeded with this, never a resumed transcript.
  - discussion: `{now, runner, ref, thread, question, workspace, workspaces,
    outreach_file}`.
- **Runner rules** (`--append-system-prompt`, fixed in code, and stated to be
  the only non-negotiable rules): return the result as the JSON the output
  schema requires; discussion sessions never create branches, commit, push or
  open pull requests; only work cards assigned to the user; the session runs
  unattended and nobody can answer questions.
- **Structured output** (JSON schema) — the contract with whatever skills the
  session uses:

```jsonc
// TriageResult
{ "cursors": {"<source id>": "<opaque cursor>"},   // persisted verbatim for the next sweep
  "handled": [{"source": "...", "item": "<event id>", "action": "replied|drafted|ignored|escalated"}],
  "cards_to_work": [{"ref": "<card ref>", "blocked_by": ["<card ref>"]}],  // blockers not yet done
  "discussions_to_run": [{"ref": "...", "thread": "<permalink>", "question": "..."}],
  "summary": "one line" }

// CardResult
{ "ref": "...", "status": "done|blocked|failed",
  "pr_url": "... | null", "blocked_on": "... | null", "summary": "one line" }

// DiscussionResult
{ "ref": "...", "status": "replied|drafted|skipped|failed", "summary": "one line" }
```

### Reading the stream

The runner reads `claude`'s stdout line by line (field names checked against
Claude Code 2.1.282 with one short haiku run; lines that are not JSON are
ignored):

- `system/init` — `session_id`, `mcp_servers: [{name, status}]` (`connected`,
  `failed`, `needs-auth`, `pending`, `disabled`), `plugins: [{name, path}]` and,
  only when a plugin failed to load, `plugin_errors: [{plugin, type, message}]`.
- `assistant` — `message.content[]` blocks (`tool_use` with `name` and `input`),
  `parent_tool_use_id` (set inside a subagent), and `error` (e.g. `rate_limit`)
  on a synthetic error message.
- `system/api_retry` — `attempt`, `max_retries`, `retry_delay_ms`,
  `error_status`, `error` (`rate_limit`, `overloaded`, ...).
- `rate_limit_event` — `rate_limit_info: {status: allowed | allowed_warning |
  rejected, resetsAt (unix seconds), rateLimitType, ...}`; emitted whenever the
  limit information changes, so only `rejected` means a limit was hit.
- `result` — `subtype` (`success`, `error_max_turns`, `error_max_budget_usd`,
  `error_during_execution`, `error_max_structured_output_retries`), `is_error`,
  `structured_output`, `total_cost_usd`, `session_id`, `api_error_status`,
  `result`.

The session id is recorded on the attempt as soon as the stream names it, so a
running or crashed session can be resumed by hand.

### Limits: keeping a session bounded

- **Wall clock** (`timeout` per mode, like Temporal's start-to-close): past it the
  session is stopped; outcome `timeout`.
- **Inactivity watchdog** (`idle_timeout`): no stream event for that long means
  stuck; the session is stopped; outcome `stuck`.
- **Loop detection** (`loop_threshold`): each `tool_use` is fingerprinted by name
  and input (object keys sorted), per agent context (the main thread, or one
  subagent by `parent_tool_use_id`); `loop_threshold` identical calls within the
  last `2 * loop_threshold` of that context stop the session; outcome `stuck`.
- **Environment guard**: at the first `system/init`, any `plugin_errors`, or a
  `required_mcp` server that is missing or not `connected`, stops the session;
  outcome `environment`, with one line naming what is wrong. This protects
  against `--bare` becoming the `-p` default, which would drop the login, skills
  and MCP servers; it catches missing MCP servers only for the servers listed in
  `required_mcp`.
- **Stop sequence** (for all of the above and for a graceful shutdown): SIGINT to
  the `claude` process (it ends the turn cleanly), after 20s SIGTERM to its
  process group, after another 20s SIGKILL to the group. As soon as `claude`
  exits during a stop, whatever it left in its group is SIGKILLed; after that
  nothing is sent to its pid. A second signal to the runner SIGKILLs the group at
  once.

A session that hits a usage limit while running is not stopped: Claude Code
retries on its own, until it ends or reaches its timeout.

### Outcomes and what follows

Every session ends with one outcome, judged from the stream and the process,
never from what the agent says about its own work. A valid structured output
always wins over a stop that came too late to matter.

| Outcome | Meaning | What follows (cards) |
|---|---|---|
| `done` | CardResult `done` | finished; the attempt count resets |
| `blocked` | CardResult `blocked` | none: eligible again when triage lists it |
| `failed` | CardResult `failed` | one retry; a second `failed` in the run: `needs_human` (`failed`) |
| `timeout` | wall clock passed | retry |
| `stuck` | watchdog or loop detection | retry |
| `api_error` | an error result (API error, budget, turn limit) | retry |
| `crash` | ended without a result, or the runner stopped mid-session | retry |
| `invalid_output` | success without a valid structured output (incl. one that does not parse as the result type) | retry |
| `rate_limited` | usage or rate limit | retry when the pause ends; not counted |
| `environment` | plugin errors, required MCP server not connected | `needs_human` (`environment`), never retried |
| `interrupted` | kill switch or signal | retry when the runner runs again; not counted |

Triage reports `ok`; discussions report `replied`, `drafted`, `skipped` or
`failed`. Rules for cards:

- **Retries** run in a fresh session after an exponential backoff: 1m for the
  first counted attempt, doubling, capped at 30m.
- **Cap**: counted attempts in the current run (since the card was last done or
  reset) stop at `max_attempts`; the attempt that reaches it makes the card
  `needs_human` (`max_attempts`). `rate_limited` and `interrupted` do not count.
  The card's own count is authoritative; the rules look back at the recorded
  attempts of the run (never past the last `done`), and an attempt that fails
  before its session starts (a worktree that cannot be created, a runner stopped
  in between) is recorded as a `crash` so the two agree.
- **No progress**: the runner records the worktree's `HEAD` before and after each
  card attempt; `new_commits` is `git rev-list --count before..after`. Two
  counted attempts in a row that end unfinished with zero new commits make the
  card `needs_human` (`no_progress`). Cards without a worktree (no workspace, or
  one that is not a git checkout root) are not measured (`null`). The first
  attempt's count includes commits the default branch gained since the worktree
  was created, when the skill branches from it.
- **`needs_human`** cards are never started again by the runner or by triage;
  triage sees them in `escalations`. A person resets one with `dispatch card
  <ref>`; a new `message` or `discussion` intake event (a person writing) that
  occurred after the card stopped and whose payload or sender mentions the ref
  (as a whole token) also resets it (status `failed`, count 0, no retry
  scheduled), so triage may list it again. `work` events do not: an assigned
  card updates on the agent's own comments and on the escalation itself, and
  the pollers report a comment on my own ticket only as a `work` event, so an
  answer there needs a mention elsewhere or `dispatch card`.
- A retry that cannot start (the card is no longer assigned to me, or another
  runner holds its claim) is dropped; the card stays `failed` for triage.
- **Discussions** follow the same rules with `[card] max_attempts`, but their
  retries live in the coordinator's queue (a discussion is a one-off request, not
  tracker state): lost when the runner stops, and not waited for by `--once`.
  A `needs_human` discussion only prints its line.
- **Triage** is not retried by these rules: a failed triage returns its events to
  `new` and backs off (see Scheduling).

### Usage limits

A usage or rate limit shows in the stream as `system/api_retry` with `error:
"rate_limit"`, a `rate_limit_event` with status `rejected`, an `assistant`
message with `error: "rate_limit"`, or a result with `api_error_status: 429`
(including a limit stop that otherwise looks like a clean `success` without
structured output). On the first sign, and again when a later reset time is
learned, the runner sets a **machine-wide pause** in the store: until the
`resetsAt` a `rejected` event gave, else for 15 minutes. A pause only ever grows.
While paused no runner starts a triage, card or discussion session (running ones
carry on); `dispatch card` refuses; `--once` exits without starting anything;
`status` shows the pause with its reason and the runner that set it. The attempt
ends `rate_limited` unless it finished anyway, hit its own budget or turn limit
(`api_error`), or got a normal answer from the API after the limit (then its own
ending counts, e.g. `timeout`). Its card is retried when the pause ends; the
attempt is not counted. One `pause set` line when a runner sets or extends the pause, one
`pause waiting` line when a runner first sees another runner's pause, one `pause
lifted` line when it ends.

### Scheduling

`heartbeat` first recovers what a stopped process of this runner left behind:
attempts left open are closed as `crash`, cards left `in_progress` follow the
rules for a `crash` (usually a retry after 1m), and its claims and `batched`
events are released. Then one coordinator loop starts triage, card and
discussion sessions as separate tokio tasks, so triage keeps its interval while
cards run:

- **Triage** (the fallback sweep) runs `interval` after the previous sweep
  finished. A failed triage retries after 30s, doubling per consecutive failure,
  capped at the interval. Its cursors are persisted as soon as it returns. Phase
  2 adds event triages started by a closed batch window (see its Coordinator
  section).
- **Queue.** Each triage result replaces the queue: cards it no longer lists are
  dropped, cards already queued keep their place with the fresh `blocked_by`,
  cards running, `in_progress`, `needs_human` or waiting for a scheduled retry are
  skipped, and at most `max_cards_per_tick` new cards join per triage.
- **Retries.** Cards whose retry is due join the queue with the `blocked_by` they
  last started with, whatever triage lists, and do not count against
  `max_cards_per_tick`. Retry times live in the store, so they survive restarts
  and `--once` runs pick up the ones already due.
- **Dependencies.** A queued card starts only when every `blocked_by` ref is
  `done` in state, or is unknown to state and not in the current batch (queued
  or running): an external dependency the runner cannot track, so it does not
  hold the card. A blocker that is not `done` holds the card until a later run
  finishes it.
- **Parallelism.** At most `card.max_parallel` card and discussion sessions run
  at once; queued discussions start before ready cards.
- **Stagger.** Session starts are at least `start_stagger` plus up to 50% random
  jitter apart, and when a pause lifts each runner waits its own jittered stagger
  before starting again, so runners do not all start at once after a reset.
- **Pause.** Nothing starts while the machine-wide pause holds (see Usage limits).
- **Checkouts.** Parallel cards in one workspace must not share a checkout. Every
  card whose workspace is the root of a git checkout runs in its own worktree,
  `git worktree add --detach <state_dir>/worktrees/<workspace>/<ref-slug>`,
  reused when it exists so a retried card finds its work (each slug carries 8 hex
  characters of an FNV-1a hash of the original name, so refs that normalise
  alike never share a directory); the skills then branch from the up-to-date
  default branch as usual. When a card ends `done`, the runner runs `git worktree
  remove` without `--force`, which refuses (and keeps the tree) when anything is
  uncommitted or untracked. A workspace that is not a git checkout root is used
  as is. A worktree that cannot be created counts as a `crash`. `dispatch card`
  uses the same checkout rule.
- `--once`: one triage, then its sessions under the same rules, plus retries
  already due; cards whose blockers do not finish in this run, and retries not
  yet due, are left for the next.
- A failed session never stops the loop.

### Stopping

- **Kill switch** (`dispatch stop`): checked before every start and polled every
  5s while waiting. Once present, nothing new starts and running sessions are
  ended with the stop sequence; the loop exits when they have ended.
- **SIGINT / SIGTERM**: the first signal does the same; the second SIGKILLs
  running sessions; a third exits immediately. Sessions ended this way are
  `interrupted` and retried when the runner runs again.
- Each `claude` process leads its own process group, so signals reach the
  subprocesses it started. Once `claude` exits, its output pipes get 5s to close;
  then whatever is left in the group is killed, so a stray subprocess never
  holds a card slot or blocks shutdown.

### Single instance and state

`heartbeat` and `card` take an exclusive, non-blocking lock on
`<state_dir>/<name>.lock` for the life of the process.
A second one of the same runner exits `1` with a one-line message, so two
processes never run the same card in one worktree and a starting heartbeat never
recovers another process's live cards. `check`, `stop`, `resume`, `enqueue`,
`status` and `history` do not take the lock.

State lives in `dispatch.db` (tables in the phase 2 spec): per runner `cursors`,
`cards` (`ref → {status, blocked_by, pr_url, updated_at, attempts, retry_at,
reason}`, status `in_progress|done|blocked|failed|needs_human`) and `attempts`
(one row per session), plus the machine-wide `pause`. Every write is a SQLite
transaction and all store I/O runs on the blocking thread pool. A card starts
only after its `card:<ref>` claim (`BEGIN IMMEDIATE`, 10 min lease renewed every
minute, released at the end): a card held by any runner, this one included, is
skipped with one line. If the store cannot be read, the scheduler prints one line
and skips scheduling until it can. It never assumes an empty state. A session
starts only once its attempt row is written.

### Messaging policy lives in Claude Code

The runner does not decide who may be messaged. The user controls that outside
the repo:

- **Claude Code settings:** `permissions.deny` rules, e.g. deny
  `mcp__*__send_message` so every send becomes a draft, or deny specific send
  tools; optionally a `PreToolUse` hook for per-channel allowlists or rate
  limits (see the Claude Code hooks docs).
- **Skills:** the user's skills and `outreach.md` decide whom to contact, send vs
  draft, and follow-ups. A denied send becomes a draft and is never retried
  another way.
- `dispatch stop` ends every run.

Quality bar: Rust stable; `cargo fmt`, `cargo clippy --all-targets -D warnings`,
`cargo test`, wired as `make lint typecheck test`. The `claude` process sits
behind one `Session` trait, so the scheduler is tested with a scripted fake and
the process handling with fake `claude` scripts printing scripted stream lines;
no model calls.

## Skill pack (`plugin/`, plugin name `dispatch`)

Optional. The runner never invokes a skill by name: each session gets an
objective, a context and the runner rules, and Claude decides which skills and
subagents to use, from this plugin (loaded with `--plugin-dir` when present) and
from the user's own ecosystem. The plugin must be self-contained (no dependency
on skills installed only on the author's machine) and may reference Claude Code
built-ins (`/code-review`, `/simplify`, `/security-review`). Its contents are
described in the plugin itself.

`examples/outreach.example.md` and `examples/config.example.toml` use fictional
people and IDs only.

## Non-goals

No log store, no dashboard, no web service, no multi-user support, no
provisioning of the machine, no messaging policy or workflow logic in the
runner.
