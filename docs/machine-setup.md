# Machine setup

Pointers only. The repo creates none of these; each is a machine-level step you
do yourself on the machine that runs the harness.

## 1. Claude Code and auth

- Install Claude Code: <https://code.claude.com/docs/en/setup>.
- Sign in: `claude auth login`; check with `claude auth status`.
- For an unattended machine, create a long-lived token with `claude setup-token`
  and expose it to the service's environment (`CLAUDE_CODE_OAUTH_TOKEN`), not a
  shell profile only interactive shells read.

## 2. Connectors (Slack, tracker, email)

The harness uses whatever MCP servers your Claude Code session has; it ships
none. Either:

- enable claude.ai connectors (Slack, Linear, Atlassian/Jira, Gmail) in claude.ai
  settings under Connectors — they load into Claude Code when you are signed in
  with that account; or
- add servers locally: `claude mcp add ...` (see
  <https://code.claude.com/docs/en/mcp>); check with `claude mcp list`.

Connect at least one messaging source and one tracker. The workflow skill
discovers tools by name at run time.

## 3. GitHub

- Install the GitHub CLI and run `gh auth login`; check with `gh auth status`.
- The harness pushes branches and opens PRs with `gh`; make sure the account can
  push to every workspace repo.

## 4. Workspaces

Clone every repo you list under `[[workspaces]]` to its `path`, with a working
`origin`. Each should have its own typecheck/test/lint entry points (e.g. `make
check`) and ideally a `CLAUDE.md` stating its definition of done; the workflow
skill runs those.

## 5. Config

```sh
mkdir -p ~/.config/claude-harness
cp examples/config.example.toml ~/.config/claude-harness/config.toml
cp examples/outreach.example.md ~/.config/claude-harness/outreach.md
```

Replace every placeholder with real channel/user IDs, people and repos, and
give the runner a `name`. These files stay out of the repo. Then run
`harness check` to validate the config, print the resolved paths and report which
intake sources have their keys.

### Secrets

API keys for the Linear and Jira pollers live in a file outside the repo, never
in `config.toml`:

```sh
cp examples/secrets.env.example ~/.config/claude-harness/secrets.env
chmod 600 ~/.config/claude-harness/secrets.env
```

The runner refuses a file readable by group or others (the source stays idle
with one stdout line). Override the path with `HARNESS_SECRETS`. Keys can be
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
harness --config ~/.config/claude-harness/example-app.toml check
```

All runners on the machine share one store (`~/.local/state/claude-harness/harness.db`,
override `HARNESS_DB`), which keeps two of them off the same card or event. Give
each its own service (section 7) with its own `--config` or `HARNESS_CONFIG`, and
narrow each one's intake (`[intake.notifications].match`, Linear `projects`, Jira
`jql`) to its project.

### Who may be messaged

The harness does not gate sends; Claude Code does, with your own settings:

- **Permission rules** (<https://code.claude.com/docs/en/permissions>): in
  `~/.claude/settings.json`, `permissions.deny` on send tools (e.g.
  `mcp__*__send_message`, or specific Slack / Gmail send tools) makes every
  message a draft.
- **Hooks** (<https://code.claude.com/docs/en/hooks>): a `PreToolUse` command
  hook can allow sends only to listed channels or domains, or rate-limit them.
- **`outreach.md`**: says what may be auto-sent to whom; the `outreach` skill
  drafts everything else, and turns any denied send into a draft.

Start with sends denied and loosen them as you trust the output. `harness stop`
creates the kill switch: nothing new starts and running sessions are ended.

## 6. Notification intake (macOS)

The notification watcher reads the macOS notification database read-only, so
Slack messages wake the runner without a model polling. Skip this section if
`[intake.notifications]` is disabled.

- **Full Disk Access:** System Settings → Privacy & Security → Full Disk Access;
  add the `harness` binary (`~/.cargo/bin/harness`) or, for manual runs, the
  terminal app that starts it. For a service, grant it to the binary the
  service runs. If access disappears after a rebuild, remove and re-add the
  entry. The runner prints one stdout line when the database cannot be read.
- **Slack desktop notifications** (Slack → Settings → Notifications): notify
  about direct messages, mentions and keywords (add your project keywords under
  "My keywords"), and turn on notifying on desktop even when you are active on
  mobile. Allow Slack in System Settings → Notifications.
- **Focus mode:** Focus modes and Do Not Disturb can hold notifications back.
  With the Focus you actually use turned on, send yourself a test DM and check
  that `harness` prints the event (or use `harness enqueue` to test the rest of
  the path).

## 7. Scheduling

Pick one:

- **Long-running loop (recommended):** `harness heartbeat` under a service
  manager that restarts it (launchd `KeepAlive`, systemd `Restart=on-failure`).
  Triage keeps its interval while cards run.
- **Timer calling one tick:** `harness heartbeat --once` from launchd
  (`StartInterval` in a LaunchAgent plist, macOS) or a systemd `.timer` +
  `.service` pair (Linux). The interval in the scheduler replaces
  `[triage].interval`. Intake events only start triage while a runner is
  running, so a timer gives up the event-driven wake-up. A tick lasts until its cards finish; a tick that
  starts while another harness run is still going exits `1` without doing
  anything (single-instance lock), so overlapping timers only waste a start.

Either way: run as your user (so it sees your Claude, `gh` and connector auth),
set `PATH` to include `claude`, `gh` and `git`, and send stdout to a file or the
journal — it is one line per run. Keep the machine awake (e.g. `caffeinate` on
macOS) if it sleeps.
