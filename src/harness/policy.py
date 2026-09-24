from collections.abc import Callable
from dataclasses import dataclass
from datetime import datetime
from typing import Any

from harness.config import SendConfig
from harness.state import State

SEND_DENIED = "Not allowed to send here — create a draft instead."
STOPPED = "Harness kill switch is set — stopping."
DENIED_BY_CONFIG = "Tool denied by harness config (tools.deny)."
DESTRUCTIVE = "Destructive messaging tools are not allowed."

_SEND_SUFFIXES = ("send_message", "schedule_message", "reply", "forward")
_DESTRUCTIVE_WORDS = ("trash", "delete")
_MESSAGING_SERVERS = ("slack", "gmail", "mail")
_SLACK_TARGET_KEYS = ("channel_id", "channel", "user_id", "user")
_EMAIL_TARGET_KEYS = ("to", "cc", "bcc")


@dataclass(frozen=True)
class Policy:
    send: SendConfig
    deny: tuple[str, ...] = ()


@dataclass(frozen=True)
class Decision:
    allowed: bool
    reason: str = ""
    is_send: bool = False
    stop: bool = False


def base_name(tool_name: str) -> str:
    return tool_name.rsplit("__", 1)[-1]


def is_send_tool(tool_name: str) -> bool:
    name = base_name(tool_name).lower()
    return any(name == suffix or name.endswith(f"_{suffix}") for suffix in _SEND_SUFFIXES)


def is_draft_tool(tool_name: str) -> bool:
    return "draft" in base_name(tool_name).lower()


def is_destructive_messaging_tool(tool_name: str) -> bool:
    lowered = tool_name.lower()
    is_messaging = any(server in lowered for server in _MESSAGING_SERVERS)
    words = base_name(lowered).split("_")
    return is_messaging and any(word in words for word in _DESTRUCTIVE_WORDS)


def is_denied_by_config(tool_name: str, deny: tuple[str, ...]) -> bool:
    return any(tool_name.startswith(entry) or base_name(tool_name) == entry for entry in deny)


def _strings(value: Any) -> list[str] | None:
    """Flatten a str / list[str] field; None when the shape is unexpected."""
    if isinstance(value, str):
        return [part.strip() for part in value.split(",") if part.strip()]
    if isinstance(value, list) and all(isinstance(item, str) for item in value):
        return [item.strip() for item in value if item.strip()]
    return None


def _targets(tool_input: dict[str, Any], keys: tuple[str, ...]) -> list[str] | None:
    found: list[str] = []
    for key in keys:
        if key not in tool_input:
            continue
        values = _strings(tool_input[key])
        if values is None:
            return None
        found.extend(values)
    return found or None


def _email_domain(address: str) -> str | None:
    _, at, domain = address.rpartition("@")
    return domain.strip(" >").lower() if at and domain else None


def slack_target_allowed(tool_input: dict[str, Any], send: SendConfig) -> bool:
    targets = _targets(tool_input, _SLACK_TARGET_KEYS)
    allowed = {*send.slack_channels, *send.slack_users}
    return targets is not None and all(target in allowed for target in targets)


def email_target_allowed(tool_input: dict[str, Any], send: SendConfig) -> bool:
    # Gmail ignores `to` when sending a stored draft and adds thread recipients on reply-all,
    # so the real recipients are not in the input.
    if tool_input.get("draftId") or tool_input.get("replyAll"):
        return False
    targets = _targets(tool_input, _EMAIL_TARGET_KEYS)
    allowed = {domain.lower() for domain in send.email_domains}
    return targets is not None and all(_email_domain(t) in allowed for t in targets)


def target_allowlisted(tool_name: str, tool_input: dict[str, Any], send: SendConfig) -> bool:
    if "slack" in tool_name.lower():
        return slack_target_allowed(tool_input, send)
    return email_target_allowed(tool_input, send)


def decide_send(
    tool_name: str, tool_input: dict[str, Any], send: SendConfig, state: State, now: datetime
) -> Decision:
    denied = Decision(allowed=False, reason=SEND_DENIED, is_send=True)
    if send.mode == "draft_only":
        return denied
    if send.mode == "allowlist" and not target_allowlisted(tool_name, tool_input, send):
        return denied
    if len(state.recent_sends(now)) >= send.max_per_hour:
        return denied
    return Decision(allowed=True, is_send=True)


def decide(
    tool_name: str,
    tool_input: dict[str, Any],
    policy: Policy,
    state: State,
    now: datetime,
    *,
    stopped: bool = False,
) -> Decision:
    if stopped:
        return Decision(allowed=False, reason=STOPPED, stop=True)
    if is_denied_by_config(tool_name, policy.deny):
        return Decision(allowed=False, reason=DENIED_BY_CONFIG)
    if is_destructive_messaging_tool(tool_name):
        return Decision(allowed=False, reason=DESTRUCTIVE)
    if is_draft_tool(tool_name):
        return Decision(allowed=True)
    if is_send_tool(tool_name):
        return decide_send(tool_name, tool_input, policy.send, state, now)
    return Decision(allowed=True)


@dataclass
class ToolGate:
    """Stateful wrapper around `decide`: checks the kill switch and records allowed sends."""

    policy: Policy
    state: State
    clock: Callable[[], datetime]
    is_stopped: Callable[[], bool]
    persist: Callable[[State], None]

    def check(self, tool_name: str, tool_input: dict[str, Any]) -> Decision:
        now = self.clock()
        decision = decide(
            tool_name, tool_input, self.policy, self.state, now, stopped=self.is_stopped()
        )
        if decision.allowed and decision.is_send:
            self.state.record_send(now)
            self.persist(self.state)
        return decision
