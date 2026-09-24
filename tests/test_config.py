from datetime import timedelta
from pathlib import Path

import pytest
from pydantic import ValidationError

from harness.config import Config, load_config
from harness.paths import CONFIG_ENV, REPO_ROOT, Paths, resolve_config_path


def test_loads_full_config(config_file: Path, tmp_path: Path) -> None:
    config = load_config(config_file)
    assert config.heartbeat.interval_delta == timedelta(minutes=10)
    assert config.heartbeat.max_cards_per_tick == 2
    assert config.card.model == "claude-opus-5-5"
    assert config.card.effort == "high"
    assert config.send.max_per_hour == 2
    assert config.workspaces[0].path == tmp_path / "code/example-app"


def test_defaults_match_spec() -> None:
    config = Config()
    assert config.heartbeat.model == "sonnet"
    assert config.heartbeat.effort == "medium"
    assert config.heartbeat.max_budget_usd == 1.0
    assert config.card.max_budget_usd == 20.0
    assert config.send.mode == "allowlist"
    assert config.state_dir == Path("~/.local/state/claude-harness").expanduser()
    assert config.outreach_file == Path("~/.config/claude-harness/outreach.md").expanduser()


def test_sources_accept_free_form_keys() -> None:
    config = Config.model_validate({"sources": {"jira_board": "EX"}})
    assert config.sources.model_dump()["jira_board"] == "EX"


@pytest.mark.parametrize(
    "raw",
    [
        {"unknown_key": 1},
        {"heartbeat": {"interval": "soon"}},
        {"heartbeat": {"effort": "extreme"}},
        {"send": {"mode": "yolo"}},
        {"card": {"max_budget_usd": 0}},
    ],
)
def test_rejects_invalid_config(raw: dict[str, object]) -> None:
    with pytest.raises(ValidationError):
        Config.model_validate(raw)


def test_workspace_lookup(config_file: Path) -> None:
    config = load_config(config_file)
    assert config.workspace_for("EX-12") is not None
    assert config.workspace_for("OTHER-1") is None
    assert config.workspace_named("example-app").name == "example-app"
    with pytest.raises(ValueError, match="no workspace"):
        config.workspace_named("missing")


def test_config_path_precedence(tmp_path: Path) -> None:
    env = {CONFIG_ENV: str(tmp_path / "env.toml")}
    assert resolve_config_path("cli.toml", env) == Path("cli.toml")
    assert resolve_config_path(None, env) == tmp_path / "env.toml"
    default = Path("~/.config/claude-harness/config.toml").expanduser()
    assert resolve_config_path(None, {}) == default


def test_plugin_dir_defaults_to_repo_plugin(tmp_path: Path) -> None:
    resolved = Paths.resolve(tmp_path / "c.toml", Config())
    assert resolved.plugin_dir == REPO_ROOT / "plugin"
    assert (REPO_ROOT / "pyproject.toml").is_file()


def test_plugin_dir_override(tmp_path: Path) -> None:
    config = Config.model_validate({"plugin_dir": str(tmp_path / "custom")})
    assert Paths.resolve(tmp_path / "c.toml", config).plugin_dir == tmp_path / "custom"
