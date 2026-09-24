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

### Send policy trade-off (`[send].mode`)

- `draft_only` — nothing leaves without you; every message waits as a draft.
  Safest; the harness only unblocks as fast as you review drafts.
- `allowlist` (recommended) — sends only to the listed channels, users and email
  domains, under `max_per_hour`; everything else becomes a draft. Start small and
  widen it as you trust the output.
- `all` — sends anywhere the tools reach, rate-limited only. Fastest, and a
  wrong message goes out under your name. Use only with a strict "always draft"
  section in `outreach.md`.

The runner enforces the mode; the model cannot bypass it. `harness stop` creates
the kill switch that halts all tool use.

## 6. Scheduling

Pick one:

- **Timer calling one tick:** `harness heartbeat --once` from launchd
  (`StartInterval` in a LaunchAgent plist, macOS), a systemd `.timer` + `.service`
  pair (Linux), or cron. The interval in the scheduler replaces `[heartbeat].interval`.
- **Long-running loop:** `harness heartbeat` under a service manager that
  restarts it (launchd `KeepAlive`, systemd `Restart=on-failure`).

Either way: run as your user (so it sees your Claude, `gh` and connector auth),
set `PATH` to include `claude`, `gh` and `uv`, and send stdout to a file or the
journal — it is one line per run. Keep the machine awake (e.g. `caffeinate` on
macOS) if it sleeps.
