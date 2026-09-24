# claude-harness

A framework around Claude Code for autonomous, long-horizon work on a dedicated machine.
See `docs/specs/2026-09-24-harness-design.md`.

## Runner

A single Rust binary, `harness`. Requires a stable Rust toolchain, `git`, and the `claude`
CLI on `PATH`. Claude Code auth and MCP connectors come from the machine (see
`docs/machine-setup.md`).

```sh
cargo install --path .                  # installs ~/.cargo/bin/harness
harness check                           # validate config, print resolved paths
harness heartbeat --once                # one triage and its cards (launchd / systemd timer)
harness heartbeat --interval 10m        # long-running loop
harness card EX-123 --workspace example-app
harness stop                            # create the kill switch; `resume` removes it
```

- Config: `--config`, else `$HARNESS_CONFIG`, else `~/.config/claude-harness/config.toml`.
- State: `<state_dir>/state.json` (cursors, card status; locked + atomic writes); kill switch
  `<state_dir>/STOP`; per-card git worktrees under `<state_dir>/worktrees/`.
- Plugin: the repo's `plugin/` (the checkout `cargo install` built from) is loaded with
  `--plugin-dir`; override with `plugin_dir` in config.
- Every run is one `claude -p` process in `auto` permission mode with JSON structured output.
  The heartbeat keeps triaging on its interval while cards run, up to `[card].max_parallel`
  at once, each card waiting for its `blocked_by` cards.
- Who may be messaged is up to your Claude Code permission rules and `outreach.md`, not the
  runner; a denied send becomes a draft.
- Output: one line per run on stdout. Exit codes: `0` ok, `1` run failed / kill switch,
  `2` bad config.

Development: `make lint typecheck test`.
