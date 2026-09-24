from pathlib import Path

import pytest

from harness.paths import Paths

CONFIG_TOML = """
outreach_file = "{root}/outreach.md"
state_dir = "{root}/state"
plugin_dir = "{root}/plugin"

[heartbeat]
interval = "10m"
max_cards_per_tick = 2

[sources]
slack_channels = ["C0000000001"]
tracker = "linear"
tracker_query = "assignee:me label:agent"

[[workspaces]]
name = "example-app"
path = "{root}/code/example-app"
match = ["EX-"]

[send]
mode = "allowlist"
slack_channels = ["C0000000001"]
slack_users = ["U0000000001"]
email_domains = ["example.com"]
max_per_hour = 2
"""


@pytest.fixture
def config_file(tmp_path: Path) -> Path:
    path = tmp_path / "config.toml"
    path.write_text(CONFIG_TOML.format(root=tmp_path), encoding="utf-8")
    return path


@pytest.fixture
def paths(tmp_path: Path) -> Paths:
    return Paths(
        config=tmp_path / "config.toml",
        state_dir=tmp_path / "state",
        outreach_file=tmp_path / "outreach.md",
        plugin_dir=tmp_path / "plugin",
    )
