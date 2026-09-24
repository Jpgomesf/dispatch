from typing import Any, Literal

from pydantic import BaseModel, ConfigDict, Field


class _Result(BaseModel):
    model_config = ConfigDict(extra="ignore")


class HandledItem(_Result):
    source: str
    item: str
    action: Literal["replied", "drafted", "ignored", "escalated"]


class HeartbeatResult(_Result):
    cursors: dict[str, str] = Field(default_factory=dict)
    handled: list[HandledItem] = Field(default_factory=list)
    cards_to_work: list[str] = Field(default_factory=list)
    summary: str


class CardResult(_Result):
    ref: str
    status: Literal["done", "blocked", "failed"]
    pr_url: str | None = None
    blocked_on: str | None = None
    summary: str


def json_schema(model: type[BaseModel]) -> dict[str, Any]:
    return model.model_json_schema()
