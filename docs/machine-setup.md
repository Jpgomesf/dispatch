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

Replace every placeholder with real channel/user IDs, people and repos. These
files stay out of the repo. Then run `harness check` to validate the config and
print the resolved paths.

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

## 6. Scheduling

Pick one:

- **Long-running loop (recommended):** `harness heartbeat` under a service
  manager that restarts it (launchd `KeepAlive`, systemd `Restart=on-failure`).
  Triage keeps its interval while cards run.
- **Timer calling one tick:** `harness heartbeat --once` from launchd
  (`StartInterval` in a LaunchAgent plist, macOS) or a systemd `.timer` +
  `.service` pair (Linux). The interval in the scheduler replaces
  `[heartbeat].interval`. A tick lasts until its cards finish, and a starting
  heartbeat marks cards left `in_progress` as failed, so never let two ticks
  overlap: launchd and systemd do not start a job that is still running; plain
  cron does, so avoid it.

Either way: run as your user (so it sees your Claude, `gh` and connector auth),
set `PATH` to include `claude`, `gh` and `git`, and send stdout to a file or the
journal — it is one line per run. Keep the machine awake (e.g. `caffeinate` on
macOS) if it sleeps.
