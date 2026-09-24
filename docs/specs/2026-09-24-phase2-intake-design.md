# Phase 2 — event intake, shared store, cross-runner claims

Builds on `2026-09-24-harness-design.md`. Goal: wake the agent when something
happens instead of polling with a model, spend tokens only on real work, and let
several runners (one per project) share a machine without stepping on each other.

## Shape

```
notification watcher ─┐
Linear poller ────────┤→ route + filter → harness.db (SQLite) → batch → triage session
Jira poller ──────────┤                      events, claims,              ↓
harness enqueue ──────┘                      cards, cursors        parallel card sessions
```

The heartbeat stays, as a slow fallback sweep (default `30m`) when no events arrive.

## Config additions

```toml
name = "example-app"            # required; unique per machine; used in claims, branches, labels

[intake]
batch_window = "60s"            # collect events this long before one triage session
allow_senders = []              # optional sender allowlist (display names / emails); empty = no filter

[intake.notifications]          # macOS only; needs Full Disk Access
enabled = true
poll = "5s"
apps = ["com.tinyspeck.slackmacgap"]
match = ["#example-channel", "Example Co"]   # substrings of title/subtitle routing a notification
                                             # to this runner; empty = every notification of `apps`

[intake.linear]
enabled = false
api_key_env = "LINEAR_API_KEY"   # personal API key; name of the variable, never the key
poll = "60s"
projects = ["Example App"]       # optional narrowing; assignee = me is always enforced
teams = []
labels = []

[intake.jira]
enabled = false
base_url = "https://example.atlassian.net"
email_env = "JIRA_EMAIL"
token_env = "JIRA_API_TOKEN"
poll = "60s"
jql = "project = EX"             # optional narrowing; runner always ANDs assignee = currentUser()
```

`[heartbeat]` is renamed `[triage]` (it is now started by events as well as the
timer); `interval` default becomes `30m` (the fallback sweep). Model/effort
defaults stay `sonnet` / `medium`. Unknown keys are still rejected.

## Secrets

- File: `~/.config/claude-harness/secrets.env` (`KEY=value` lines, `#` comments),
  override with `HARNESS_SECRETS`. Outside the repo; never logged or put in a
  session context.
- Re-read on every poll, so keys can be added or rotated while the runner runs.
  A source whose key is missing stays idle with one stdout line (once per state
  change) and starts by itself when the key appears.
- Refused (source idle, one line) if the file is group/world readable (not `0600`).
- A real environment variable of the same name wins over the file.
- `harness check` reports which sources have their keys, never the values.

## Personal scope (enforced, not configurable)

- Linear: every query includes `assignee: { isMe: { eq: true } }`; `projects`,
  `teams`, `labels` only narrow further.
- Jira: the query is always `assignee = currentUser() AND (<jql>)`; `jql` only
  narrows further.
- Both authenticate as the key's owner, so "me" is the person whose key it is.

## Work vs discussion events

Every tracker event carries `kind`:

- `work` — the ticket is assigned to me (queries above). May become a card.
- `discussion` — activity on a ticket I take part in but am not assigned:
  - Linear: the personal `notifications` feed (mentions, new comments on
    subscribed issues, replies to my comments), polled with a cursor.
  - Jira: `watcher = currentUser() AND assignee != currentUser() AND updated > <cursor>`
    (commenting, reporting or being mentioned makes you a watcher); only comments
    newer than the cursor, excluding my own.
  - Triage only: reply, draft or ignore. Never a card.

Enforcement:

- Runner (hard): before claiming a card, it re-checks through the tracker API that
  the card's assignee is me; otherwise the card is refused with one stdout line,
  whatever triage returned. Refs from other trackers or without a key are refused.
- Skill (soft): `discussion` events never go into `cards_to_work`; the skill may
  offer to take the ticket if it gets assigned.

## Store: `harness.db` (SQLite, machine-wide)

One file shared by every runner on the machine: `~/.local/state/claude-harness/harness.db`
(overridable with `HARNESS_DB`). Opened with WAL, `busy_timeout = 5000`,
`foreign_keys = on`; every write is a transaction; claims use `BEGIN IMMEDIATE`.
Crate: `rusqlite` (bundled). All DB work runs on the blocking pool.

Per-runner instance lock stays a file lock: `<state_dir>/<name>.lock`.

Tables (all runner-scoped rows carry `runner`):

| Table | Key | Purpose |
|---|---|---|
| `events` | `id`; `UNIQUE(source, external_id)` | intake queue: `runner`, `source`, `external_id`, `payload` JSON, `status` (`new`/`batched`/`done`), `created_at` |
| `claims` | `key` (PK) | cross-runner exclusivity: `runner`, `lease_until`, `claimed_at` |
| `cards` | `(runner, ref)` | replaces `state.json` cards: `status`, `blocked_by` JSON, `pr_url`, `updated_at` |
| `cursors` | `(runner, key)` | replaces `state.json` cursors (skill cursors and poller cursors) |
| `schema_version` | — | migrations, applied in order at open |

Migration: on first open a runner imports its `state.json` (if present) into the
DB and renames it `state.json.migrated`. Nothing is deleted.

## Level 1 — hard claims (runner, same machine)

- Claim keys: `card:<ref>`, `event:<source>:<external_id>`.
- `claim(key, runner, lease)` inserts, or takes over a row whose `lease_until` is
  past; otherwise returns "held by <runner>". Atomic under `BEGIN IMMEDIATE`.
- Cards: claimed before the session starts; lease `10m`, renewed every `1m` while
  the session runs; released on finish. A crashed runner's lease simply expires,
  so another runner (or its own restart) can take the card.
- Events: routing decides which runner a shared event belongs to (`match`); the
  claim guarantees only one runner inserts/handles it even if filters overlap.
- A card held by another runner is skipped with one stdout line, never started.

## Level 2 — soft claims (skills, across machines and people)

Runner passes `runner` (its `name`) in every JSON context. The `workflow` skill:

- **Card start:** read the card first. If another assignee/agent label, an open
  branch or PR mentioning the ref, or a claim comment newer than 1 h by another
  runner exists → return `blocked`, `blocked_on: "claimed by <who>"`. Otherwise
  post a claim comment (`agent:<runner> working on this`) and add label
  `agent:<runner>` when the tracker supports labels.
- **Slack reply:** re-read the thread right before replying; skip if you, or any
  message tagged by another runner, already answered after the triggering message.
- **Branches / PRs:** branch `agent/<runner>/<ref-slug>`; PR body names the runner.
- **Card finish:** remove the label / post the result comment.

## Intake sources

- **Notification watcher.** Reads the macOS notification DB read-only
  (`~/Library/Group Containers/group.com.apple.usernoted/db2/db`, `record` ⋈ `app`),
  rows with `rec_id` greater than the cursor. Decodes the `data` binary plist
  (`plist` crate) to title / subtitle / body / date. Narrow parser module with
  fixture tests; an unexpected shape is a loud error for that record (skipped,
  cursor advances, one stdout line), never a silent drop of the whole source.
  External id = `rec_id`. Payload carries only title, subtitle, body preview, app,
  delivered date.
- **Linear poller.** GraphQL over HTTPS (`reqwest`, rustls) with the personal key:
  issues matching `filter` with `updatedAt > cursor`. External id =
  `<issue id>@<updatedAt>`.
- **Jira poller.** REST search with `jql AND updated > cursor`. External id =
  `<key>@<updated>`.
- **`harness enqueue <source> <text>`**: manual event, for testing and scripts.

Pollers are independent tokio tasks; a source failing backs off on its own and
never stops the others.

## Dispatch

- Slack (via notifications) and other message events are not acted on by the
  runner: they only start triage. The triage session (`[triage]` model/effort,
  default sonnet/medium) reads the full thread through the connector, decides
  relevance, and per event ignores, sends an initial response via `outreach`,
  drafts, or turns it into a card. Whether an initial response is sent or
  drafted is decided by Claude Code permissions.
- First `new` event for a runner opens a batch window; when it closes, all `new`
  events become `batched` and go to one triage session as `events: [...]` in the
  heartbeat context (in addition to cursors, sources, workspaces). On success they
  become `done`; on failure they return to `new` (retry with backoff).
- `allow_senders`, when set, drops events whose sender is not listed before they
  are stored.
- The triage result and card scheduling are unchanged (`cards_to_work` with
  `blocked_by`, `max_parallel`, per-card worktrees), plus Level 1 card claims.

## Out of scope

Postgres "second brain" (roadmap), Slack Socket Mode (README target), a model in
the watcher loop, non-macOS notification sources.
