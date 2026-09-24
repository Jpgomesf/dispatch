# claude-harness

A framework around Claude Code for autonomous, long-horizon work on a dedicated machine.
See `docs/specs/2026-09-24-harness-design.md`.

## Runner

Requires Python 3.12+ and [uv](https://docs.astral.sh/uv/). Claude Code auth and MCP
connectors come from the machine (see `docs/machine-setup.md`).

```sh
uv sync --extra dev
uv run harness check                         # validate config, print resolved paths
uv run harness heartbeat --once              # one triage tick (cron / launchd / systemd timer)
uv run harness heartbeat --interval 10m      # long-running loop
uv run harness card EX-123 --workspace example-app
uv run harness stop                          # create the kill switch; `resume` removes it
```

- Config: `--config`, else `$HARNESS_CONFIG`, else `~/.config/claude-harness/config.toml`.
- State: `<state_dir>/state.json` (cursors, card status, recent sends); kill switch `<state_dir>/STOP`.
- Plugin: the repo's `plugin/` is loaded as a local plugin; override with `plugin_dir` in config.
- Every run is one Agent SDK session in `auto` permission mode with structured output.
  Sends (Slack / Gmail) are gated by the runner's `[send]` policy via a `PreToolUse` hook;
  a denied send tells the agent to create a draft instead.
- Output: one line per run on stdout. Exit codes: `0` ok, `1` run failed / kill switch, `2` bad config.

Development: `make lint typecheck test`.
