# Machine setup

Pointers only. The repo creates none of these; each is a machine-level step you
do yourself on the machine that runs dispatch.

## 1. Install dispatch

Needs a stable Rust toolchain, `git`, and the `claude` CLI on `PATH`.

```sh
cd ~/rust-projects/dispatch
cargo install --path .        # installs ~/.cargo/bin/dispatch
dispatch check                # after section 6: validates the config, prints paths and key presence
```

Re-run `cargo install --path .` after pulling. The repo's `plugin/` is loaded
from this checkout unless `plugin_dir` says otherwise.

## 2. Claude Code and auth

- Install Claude Code: <https://code.claude.com/docs/en/setup>.
- Sign in: `claude auth login`; check with `claude auth status`.
- For an unattended machine, create a long-lived token with `claude setup-token`
  and expose it to the service's environment (`CLAUDE_CODE_OAUTH_TOKEN`), not a
  shell profile only interactive shells read.

### Pin or watch the Claude Code version

dispatch runs `claude -p` and relies on it loading your subscription login,
skills, plugins and MCP servers. The headless docs
(<https://code.claude.com/docs/en/headless>) say `--bare` will become the `-p`
default in a future release, and bare mode drops exactly those. On the
dispatch machine:

- **Pin** a known-good version (`claude install <version>`) and stop background
  updates with `"env": {"DISABLE_AUTOUPDATER": "1"}` in `~/.claude/settings.json`
  (`claude doctor` confirms), then update by hand after reading the release
  notes; **or watch**: follow the `stable` channel (`"autoUpdatesChannel":
  "stable"`) and read the release notes for `-p` / `--bare` changes. See
  <https://code.claude.com/docs/en/setup#update-claude-code>.
- **Guard:** list the MCP servers every session needs in
  `[sessions].required_mcp`, named as `claude mcp list` prints them (e.g.
  `"claude.ai Slack"`). A session that starts without them connected is ended
  and recorded as `environment` instead of running without its tools.

## 3. Connectors (Slack, tracker, email)

dispatch uses whatever MCP servers your Claude Code session has; it ships none.
Either:

- enable claude.ai connectors (Slack, Linear, Atlassian/Jira, Gmail) in claude.ai
  settings under Connectors — they load into Claude Code when you are signed in
  with that account; or
- add servers locally: `claude mcp add ...` (see
  <https://code.claude.com/docs/en/mcp>); check with `claude mcp list`.

Connect at least one messaging source and one tracker, and put them in
`required_mcp` (section 2).

## 4. GitHub

- Install the GitHub CLI and run `gh auth login`; check with `gh auth status`.
- Card sessions push branches and open PRs with `gh`; make sure the account can
  push to every workspace repo.

## 5. Workspaces

Clone every repo you list under `[[workspaces]]` to its `path`, with a working
`origin`. Each should have its own typecheck/test/lint entry points (e.g. `make
check`) and ideally a `CLAUDE.md` stating its definition of done; card sessions
run those.

## 6. Config

```sh
mkdir -p ~/.config/dispatch
cp examples/config.example.toml ~/.config/dispatch/config.toml
cp examples/outreach.example.md ~/.config/dispatch/outreach.md
```

Replace every placeholder with real channel/user IDs, people and repos, and
give the runner a `name`. These files stay out of the repo. Then run
`dispatch check`. Override the config path with `--config` or `DISPATCH_CONFIG`.

### Secrets

API keys for the Linear and Jira pollers live in a file outside the repo, never
in `config.toml`:

```sh
cp examples/secrets.env.example ~/.config/dispatch/secrets.env
chmod 600 ~/.config/dispatch/secrets.env
```

The runner refuses a file readable by group or others (the source stays idle
with one stdout line). Override the path with `DISPATCH_SECRETS`. Keys can be
added or rotated while the runner runs.

- **Linear personal API key:** Linear → Settings → Account → Security & access →
  Personal API keys (<https://linear.app/settings/account/security>).
- **Jira API token:** <https://id.atlassian.com/manage-profile/security/api-tokens>;
  `JIRA_EMAIL` is the Atlassian account email that owns it.

Both act as you: the pollers only see work assigned to you and threads you take
part in.

### One runner per project

Run one runner per project, each with its own config file and a unique `name`
(it names the lock, the claims, the `agent:<name>` label and the
`agent/<name>/...` branches):

```sh
dispatch --config ~/.config/dispatch/example-app.toml check
```

All runners on the machine share one store (`~/.local/state/dispatch/dispatch.db`,
override `DISPATCH_DB`), which keeps two of them off the same card or event.
Give each its own service (section 8) with its own `--config` or
`DISPATCH_CONFIG`, and narrow each one's intake (`[intake.notifications].match`,
Linear `projects`, Jira `jql`) to its project.

### Who may be messaged

dispatch does not gate sends; Claude Code does, with your own settings:

- **Permission rules** (<https://code.claude.com/docs/en/permissions>): in
  `~/.claude/settings.json`, `permissions.deny` on send tools (e.g.
  `mcp__*__send_message`, or specific Slack / Gmail send tools) makes every
  message a draft.
- **Hooks** (<https://code.claude.com/docs/en/hooks>): a `PreToolUse` command
  hook can allow sends only to listed channels or domains, or rate-limit them.
- **`outreach.md`**, if you use the starter pack's `outreach` skill: says what
  may be auto-sent to whom; the skill drafts everything else, and turns any
  denied send into a draft.

Start with sends denied and loosen them as you trust the output. `dispatch stop`
creates the kill switch: nothing new starts and running sessions are ended.

## 7. Notification intake (macOS)

The notification watcher reads the macOS notification database read-only, so
Slack messages wake the runner without a model polling. Skip this section if
`[intake.notifications]` is disabled.

- **Full Disk Access:** System Settings → Privacy & Security → Full Disk Access;
  add the `dispatch` binary (`~/.cargo/bin/dispatch`) or, for manual runs, the
  terminal app that starts it. For a service, grant it to the binary the
  service runs. If access disappears after a rebuild, remove and re-add the
  entry. The runner prints one stdout line when the database cannot be read.
- **Slack desktop notifications** (Slack → Settings → Notifications): notify
  about direct messages, mentions and keywords (add your project keywords under
  "My keywords"), and turn on notifying on desktop even when you are active on
  mobile. Allow Slack in System Settings → Notifications.
- **Focus mode:** Focus modes and Do Not Disturb can hold notifications back.
  With the Focus you actually use turned on, send yourself a test DM and check
  that `dispatch` prints the event (or use `dispatch enqueue` to test the rest
  of the path).

## 8. Scheduling

Pick one:

- **Long-running loop (recommended):** `dispatch heartbeat` under a service
  manager that restarts it (launchd `KeepAlive`, systemd `Restart=on-failure`).
  Triage keeps its interval while cards run.
- **Timer calling one tick:** `dispatch heartbeat --once` from launchd
  (`StartInterval` in a LaunchAgent plist, macOS) or a systemd `.timer` +
  `.service` pair (Linux). The interval in the scheduler replaces
  `[triage].interval`. Intake events only start triage while a runner is
  running, so a timer gives up the event-driven wake-up. A tick lasts until its
  cards finish; a tick that starts while the same runner is still going exits
  `1` without doing anything (single-instance lock), so overlapping timers only
  waste a start.

Either way: run as your user (so it sees your Claude, `gh` and connector auth),
set `PATH` to include `claude`, `gh` and `git`, and send stdout to a file or the
journal — it is one line per run. Keep the machine awake (e.g. `caffeinate` on
macOS) if it sleeps.

## 9. Usage limits (Max plan)

Every runner on the machine draws on the same subscription.

- When a session hits a usage limit, dispatch pauses **every** runner on the
  machine until the limit resets; the cut-off attempt is recorded as
  `rate_limited` and retried afterwards.
- Session starts are spaced by `[sessions].start_stagger` plus jitter, because
  many sessions starting at once (typically right after a reset) trip a
  server-side burst limit.
- `max_parallel`, and the subagents each card fans out to, are what spend the
  limit fastest. Start low.

## 10. Inspecting runs

- `dispatch status`: what the runners are doing now.
- `dispatch history [ref]`: past sessions with their outcome (`done`,
  `blocked`, `failed`, `timeout`, `stuck`, `api_error`, `crash`,
  `rate_limited`, `environment`, `needs_human`) and session id.
- `claude --resume <session_id>`, run from the directory the session ran in
  (the card's worktree under `~/.local/state/dispatch/worktrees/`, or the state
  dir for triage), reopens that conversation. Add `--fork-session` to look
  around without appending to it, and do it while the runner is not working
  that card.
