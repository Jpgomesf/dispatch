# Phase 2 — event intake, shared store, cross-runner claims

Builds on `2026-09-24-harness-design.md`. Goal: wake the agent when something
happens instead of polling with a model, spend tokens only on real work, and let
several runners (one per project) share a machine without stepping on each other.

## Shape

```
notification watcher ─┐
Linear poller ────────┤→ route + filter → dispatch.db (SQLite) → batch → triage session
Jira poller ──────────┤                      events, claims,              ↓
dispatch enqueue ─────┘                      cards, cursors        parallel card sessions
```

The heartbeat stays, as a slow fallback sweep (default `30m`) when no events arrive.

## Config additions

```toml
name = "example-app"            # required; unique per machine; used in claims, branches, labels

[intake]
batch_window = "60s"            # collect events this long before one triage session
allow_senders = []              # optional sender allowlist (display names / emails); empty = no filter
mention_names = []              # how I appear in notification text, e.g. "@Example User"

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
defaults stay `sonnet` / `medium`. Unknown keys are still rejected. The CLI
command stays `dispatch heartbeat [--interval] [--once]`. Sessions are pointed at
an objective, never at a skill by name; objectives, timeouts, `[discussion]` and
`[sessions]` are in the main spec.

As implemented: every source defaults to `enabled = false`; `teams` accepts team
keys or names; `jql` is validated at load (balanced parentheses outside quotes, no
`ORDER BY`, no `\` outside quotes) so `AND (<jql>)` cannot widen the scope; `allow_senders` entries match
case-insensitively as substrings of the sender. `mention_names` is an addition: a
notification whose title, subtitle or body contains one is a mention of me
(`mentions_me`); macOS notifications carry no structured mention flag.

## Secrets

- File: `~/.config/dispatch/secrets.env` (`KEY=value` lines, `#` comments),
  override with `DISPATCH_SECRETS`. Outside the repo; never logged or put in a
  session context.
- Re-read on every poll, so keys can be added or rotated while the runner runs.
  A source whose key is missing stays idle with one stdout line (once per state
  change) and starts by itself when the key appears.
- Refused (source idle, one line) if the file is group/world readable (not `0600`).
- A real environment variable of the same name wins over the file.
- `dispatch check` reports which sources have their keys, never the values.

## Personal scope (enforced, not configurable)

- Linear: every query includes `assignee: { isMe: { eq: true } }`; `projects`,
  `teams`, `labels` only narrow further (`project: {name: {in}}`,
  `team: {or: [{key: {in}}, {name: {in}}]}`, `labels: {some: {name: {in}}}`; config
  values only fill `in` lists, so they cannot replace the assignee clause).
- Jira: the query is always `assignee = currentUser() AND (<jql>)`; `jql` only
  narrows further (validated as above).
- Both authenticate as the key's owner, so "me" is the person whose key it is.

## Work vs discussion events

Every tracker event carries `kind`:

- `work` — the ticket is assigned to me (queries above). May become a card.
- `discussion` — activity on a ticket I take part in but am not assigned:
  - Linear: the personal `notifications` feed (mentions, new comments on
    subscribed issues, replies to my comments), polled with a cursor.
  - Jira: `watcher = currentUser() AND (assignee != currentUser() OR assignee is EMPTY)
    AND (<jql>) AND updated >= -<n>m` (commenting, reporting or being mentioned
    makes you a watcher); only comments newer than the cursor, excluding my own.
    Corrected from `assignee != currentUser()` alone, which in JQL never matches
    unassigned issues.
  - Linear discussions skip issues assigned to me (those are `work`), my own
    actions, and reactions; `projects` / `teams` / `labels` narrow them on the
    client, so each runner only takes its own project's discussions.
  - Triage only: reply, draft or ignore. Never a card.

Enforcement:

- Runner (hard): before claiming a card, it re-checks through the tracker API that
  the card's assignee is me; otherwise the card is refused with one stdout line,
  whatever triage returned (a retry scheduled for it is dropped). Refs from other
  trackers or without a key are refused.
  As implemented it asks every enabled tracker (`issue(id: <ref>) { assignee { isMe } }`;
  Jira `GET /rest/api/3/issue/<key>?fields=assignee` against `/myself`) and fails
  closed on a missing key, an API error or an unknown ref. **Deviation:** with no
  tracker intake enabled the runner has no API to ask, so the check is skipped
  and only the skill's soft check applies (phase 1 configs keep working).
- Session rules (soft): the runner rules appended to every session say to work
  only cards assigned to the user; `discussion` events are expected never to go
  into `cards_to_work`, and the skills may offer to take the ticket if it gets
  assigned.

## Taking part in discussions (dispatch must not get in the way)

The assignee rule gates *doing work* (branch, code, PR), never *talking*.

- Mentions of me and direct replies to me bypass `allow_senders` and every
  other intake filter.
- The runner never gates replies or comments; only the skills and the user's
  Claude Code permissions decide whether a reply is sent or drafted.
- Discussion events are keyed per comment/message, so follow-ups in the same
  thread are new events, never deduplicated away.
- Triage may return `discussions_to_run: [{ "ref": str, "thread": str, "question": str }]`
  for mentions that need investigation. The runner starts a **discussion session**
  per item: `[card]` model/effort/budget, `[discussion]` timeout and objective
  ("You were mentioned in a discussion about {ref}. Investigate the question and
  respond in the thread."), a detached worktree of the matching workspace (read,
  run, test), JSON context `{now, runner, ref, thread, question, workspace,
  workspaces, outreach_file}`. The runner rules forbid it to create a branch,
  commit, push or open a PR; it ends by replying (or drafting) and returns
  `DiscussionResult { ref, status: replied|drafted|skipped|failed, summary }`.
  It follows the retry rules of the main spec, with in-memory retries.
- Discussion sessions share `max_parallel` with cards (queued discussions start
  before ready cards) and are claimed with `discussion:<thread>`, or
  `discussion:<ref>` when `thread` is empty (changed from
  `discussion:<source>:<comment id>`: `DiscussionToRun` carries no source or
  comment id, and `thread` is the triggering comment's permalink). `ref` may be an
  event id when the thread has no ticket; the workspace is picked by `match`
  against `ref`, then `thread`, else none (the session runs in the state dir). The
  detached worktree is `git worktree remove`d (without `--force`) afterwards.

## Store: `dispatch.db` (SQLite, machine-wide)

One file shared by every runner on the machine: `~/.local/state/dispatch/dispatch.db`
(overridable with `DISPATCH_DB`). Opened with WAL, `busy_timeout = 5000`,
`foreign_keys = on`; every write is a transaction; claims use `BEGIN IMMEDIATE`.
Crate: `rusqlite` (bundled). All DB work runs on the blocking pool.

Per-runner instance lock stays a file lock: `<state_dir>/<name>.lock`.

Tables (all runner-scoped rows carry `runner`):

| Table | Key | Purpose |
|---|---|---|
| `events` | `id`; `UNIQUE(source, external_id)` | intake queue: `runner`, `source`, `external_id`, `payload` JSON, `status` (`new`/`batched`/`done`), `created_at` |
| `claims` | `key` (PK) | cross-runner exclusivity: `runner`, `lease_until`, `claimed_at` |
| `cards` | `(runner, ref)` | per-runner card state: `status` (`in_progress`/`done`/`blocked`/`failed`/`needs_human`), `blocked_by` JSON, `pr_url`, `updated_at`, `attempts` (counted in the current run), `retry_at`, `reason` |
| `cursors` | `(runner, key)` | skill cursors and poller cursors |
| `attempts` | `id` | one row per session: `runner`, `mode`, `ref` (empty for triage), `attempt` (per runner, mode and ref), `cwd`, `started_at`, `ended_at`, `outcome`, `summary`, `blocked_on`, `session_id`, `cost_usd`, `new_commits` |
| `pause` | single row | machine-wide pause after a usage limit: `until`, `reason`, `runner`, `set_at` |
| `schema_version` | — | migrations, applied in order at open |

As implemented: the default path is fixed (not under `state_dir`) so runners
with different state dirs still share it; `dispatch check`, `status` and
`history` never create it (`status` and `history` open it read-only and refuse a
schema version other than their own). `events` also stores `kind`,
`mentions_me`, `sender`, `occurred_at`; its integer `id`, as a string, is the
event `id` triage sees. Poller cursors live in `cursors` under
`intake:<source>:<stream>` and are never shown to the skills.
`cards.blocked_by` records the triage's list when the card starts. At startup
(`heartbeat` and `card`, under the instance lock) a runner closes its open
attempts as `crash`, settles its `in_progress` cards by the retry rules, releases
its own claims and returns its `batched` events to `new`.

## Level 1 — hard claims (runner, same machine)

- Claim keys: `card:<ref>`, `event:<source>:<external_id>`.
- `claim(key, runner, lease)` inserts, or takes over a row whose `lease_until` is
  past; otherwise returns "held by <runner>". Atomic under `BEGIN IMMEDIATE`.
- Cards: claimed before the session starts; lease `10m`, renewed every `1m` while
  the session runs; released on finish. A crashed runner's lease simply expires,
  so another runner (or its own restart) can take the card.
- Events: routing decides which runner a shared event belongs to (`match`); the
  claim guarantees only one runner inserts/handles it even if filters overlap.
  The event claim is taken in the insert's `BEGIN IMMEDIATE` transaction and
  released when its triage succeeds; `UNIQUE(source, external_id)` keeps it
  deduplicated after that.
- A claim is not re-entrant: a key held by the same runner name is "held" too.
  Losing a claim (lease expired and taken over) prints one `claim lost` line.
- A card held by another runner is skipped with one stdout line, never started.

## Level 2 — soft claims (skills, across machines and people)

The runner passes `runner` (its `name`) in every JSON context and enforces
nothing here; these are the conventions the skills follow:

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
  rows with `delivered_date` greater than the cursor. Decodes the `data` binary
  plist (`plist` crate) to title / subtitle / body / date. Narrow parser module
  with fixture tests; an unexpected shape is a loud error for that record
  (skipped, cursor advances, one stdout line), never a silent drop of the whole
  source. External id = the record's `uuid` (hex). Payload carries only title,
  subtitle, body preview, app, delivered date.
  **Changed from `rec_id`** as cursor and external id: `rec_id` is a plain
  `INTEGER PRIMARY KEY` without AUTOINCREMENT, so when the newest records are
  cleared (read notifications) SQLite reuses their ids, and a `rec_id` cursor would
  silently drop the next notifications. Every record has a unique 16-byte `uuid`
  and a `delivered_date` (verified with counts only).
  Verified schema (macOS, read-only `.schema` only): `record(rec_id, app_id, uuid,
  data, request_date, request_last_date, delivered_date, presented, style,
  snooze_fire_date)`, `app(app_id, identifier, badge)`. The plist is
  `{app, date, req: {titl, subt?, body, iden, thre, cate, ...}, ...}`; dates are
  seconds since 2001-01-01. `sender` is the title; `kind` is `message`.
- **Linear poller.** GraphQL over HTTPS (`reqwest`, rustls) with the personal key
  (`Authorization: <key>`, no `Bearer`): `issues(filter:, orderBy: updatedAt)`
  with `updatedAt: {gt: cursor}` for work; `notifications(filter: {createdAt:
  {gt: cursor}})` with `... on IssueNotification { issue comment actor type }`
  for discussion. External ids: `<issue id>@<updatedAt>`; per discussion the
  comment id (else the notification id). Notification `type` containing
  `Mention` sets `mentions_me`.
- **Jira poller.** `POST /rest/api/3/search/jql` (`jql`, `fields`, `maxResults`,
  `nextPageToken`; the old `/rest/api/3/search` is removed), Basic auth
  email:token. JQL dates are minute-resolution in the user's time zone, so the
  window is relative (`updated >= -<n>m`, `n` = minutes since the cursor + 2) and
  exact filtering uses the returned timestamps. External ids: `<key>@<updated>`
  for work, `<key>#<comment id>` for discussion; an ADF `mention` of my
  `accountId` (`/rest/api/3/myself`) sets `mentions_me`.
- **Cursors.** A source's first poll only sets its cursor (now, or the newest
  `delivered_date`): history is never replayed; the fallback sweep covers what is
  already open. A cursor moves to the newest timestamp fetched (including filtered
  items) and only after the events are stored; dedup absorbs any overlap. Triage
  results cannot overwrite these `intake:*` cursors.
- **`dispatch enqueue <source> <text>`**: manual `message` event (payload
  `{body}`) for this runner, for testing and scripts.

Pollers are independent tokio tasks; a source failing backs off on its own
(poll interval doubling, capped at 10 min) and never stops the others. Each
prints one `intake <source> <state>` line when its state changes (`ready`,
`idle — <KEY> missing`, `failed — <error>`), plus `queued N event(s)` and one line
per skipped notification record. Pollers run only in the long-lived loop, not
under `--once`.

## Coordinator

- Slack (via notifications) and other message events are not acted on by the
  runner: they only start triage. The triage session (`[triage]` model/effort,
  default sonnet/medium) reads the full thread through the connector, decides
  relevance, and per event ignores, sends an initial response, drafts, or turns
  it into a card, using the user's skills. Whether an initial response is sent or
  drafted is decided by Claude Code permissions.
- First `new` event for a runner opens a batch window; when it closes, all `new`
  events become `batched` and go to one triage session as `events: [...]` in the
  triage context (with cursors, sources, workspaces and escalations). On success
  they become `done`; on failure they return to `new` (retry with backoff).
- A batched event whose payload or sender mentions a `needs_human` card of this
  runner (the ref as a whole token) resets that card, with one `card reopened`
  line: the external change it was waiting for.
- Nothing starts while the machine-wide pause holds, and session starts are
  staggered (main spec).
- On success every batched event becomes `done`, whether or not `handled` names
  it (an event may only have produced a `discussions_to_run` entry). A failed
  batch (including a panicked triage task) waits for the later of the
  30s-doubling backoff and a new batch window. Cursors the session returns are
  saved best effort: a store failure there prints one line but does not fail
  the batch, which would repeat replies already sent.
- A queued card that ends without a recorded status (refused by the assignee
  check, or held by another runner) keeps holding cards `blocked_by` it for the
  rest of the loop, instead of looking like an external dependency.
- The fallback sweep (`events: []`) has its own timer: `[triage] interval` after
  the previous sweep, independent of event triages (an event triage handles only
  its events, so it does not replace a sweep). One triage runs at a time.
- `--once`: one triage over the pending events right away (no window), else a
  sweep; then its sessions and any retry already due.
- `allow_senders`, when set, drops events whose sender is not listed before they
  are stored; it never applies to `work` events or to `mentions_me` events.
- The triage result and card scheduling follow the main spec (`cards_to_work`
  with `blocked_by`, `max_parallel`, per-card worktrees, retries), plus Level 1
  card claims.

## Out of scope

Postgres "second brain" (roadmap), Slack Socket Mode (README target), a model in
the watcher loop, non-macOS notification sources.
