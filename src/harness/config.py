import tomllib
from datetime import timedelta
from pathlib import Path
from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field, field_validator

from harness.durations import parse_duration

Effort = Literal["low", "medium", "high", "xhigh", "max"]
SendMode = Literal["all", "allowlist", "draft_only"]

DEFAULT_CONFIG_DIR = Path("~/.config/claude-harness")
DEFAULT_STATE_DIR = Path("~/.local/state/claude-harness")


class _Strict(BaseModel):
    model_config = ConfigDict(extra="forbid", frozen=True)


class HeartbeatConfig(_Strict):
    interval: str = "10m"
    model: str = "sonnet"
    effort: Effort = "medium"
    max_budget_usd: float = Field(default=1.0, gt=0)
    max_cards_per_tick: int = Field(default=1, ge=0)

    @field_validator("interval")
    @classmethod
    def _valid_interval(cls, value: str) -> str:
        parse_duration(value)
        return value

    @property
    def interval_delta(self) -> timedelta:
        return parse_duration(self.interval)


class CardConfig(_Strict):
    model: str = "opus"
    effort: Effort = "max"
    max_budget_usd: float = Field(default=20.0, gt=0)


class SourcesConfig(BaseModel):
    """Free-form: the workflow skill interprets these values."""

    model_config = ConfigDict(extra="allow", frozen=True)

    slack_channels: list[str] = Field(default_factory=list)
    tracker: str = "linear"
    tracker_query: str = ""


class Workspace(_Strict):
    name: str
    path: Path
    match: list[str] = Field(default_factory=list)

    @field_validator("path")
    @classmethod
    def _expand(cls, value: Path) -> Path:
        return value.expanduser()


class SendConfig(_Strict):
    mode: SendMode = "allowlist"
    slack_channels: list[str] = Field(default_factory=list)
    slack_users: list[str] = Field(default_factory=list)
    email_domains: list[str] = Field(default_factory=list)
    max_per_hour: int = Field(default=6, ge=0)


class ToolsConfig(_Strict):
    deny: list[str] = Field(default_factory=list)


class Config(_Strict):
    model_config = ConfigDict(extra="forbid", frozen=True, validate_default=True)

    outreach_file: Path = DEFAULT_CONFIG_DIR / "outreach.md"
    state_dir: Path = DEFAULT_STATE_DIR
    plugin_dir: Path | None = None
    heartbeat: HeartbeatConfig = HeartbeatConfig()
    card: CardConfig = CardConfig()
    sources: SourcesConfig = SourcesConfig()
    workspaces: list[Workspace] = Field(default_factory=list)
    send: SendConfig = SendConfig()
    tools: ToolsConfig = ToolsConfig()

    @field_validator("outreach_file", "state_dir", "plugin_dir")
    @classmethod
    def _expand(cls, value: Path | None) -> Path | None:
        return value.expanduser() if value is not None else None

    def workspace_named(self, name: str) -> Workspace:
        for workspace in self.workspaces:
            if workspace.name == name:
                return workspace
        raise ValueError(f"no workspace named {name!r} in config")

    def workspace_for(self, ref: str) -> Workspace | None:
        for workspace in self.workspaces:
            if any(pattern in ref for pattern in workspace.match):
                return workspace
        return None


def load_config(path: Path) -> Config:
    with path.open("rb") as handle:
        raw: dict[str, Any] = tomllib.load(handle)
    return Config.model_validate(raw)
