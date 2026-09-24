import asyncio
from pathlib import Path
from typing import Any

import pytest
from claude_agent_sdk import ResultMessage
from fakes import NOW

from harness.config import SendConfig
from harness.policy import SEND_DENIED, STOPPED, Decision, Policy, ToolGate
from harness.results import HeartbeatResult, json_schema
from harness.session import (
    SessionError,
    SessionRequest,
    build_options,
    hook_output,
    outcome_from,
)
from harness.state import State


def make_request(tmp_path: Path) -> SessionRequest:
    return SessionRequest(
        prompt="/claude-harness:workflow heartbeat",
        model="sonnet",
        effort="medium",
        max_budget_usd=1.0,
        cwd=tmp_path,
        plugin_dir=tmp_path / "plugin",
        output_schema=json_schema(HeartbeatResult),
    )


def make_gate(stopped: bool = False) -> ToolGate:
    policy = Policy(send=SendConfig(mode="draft_only"))
    return ToolGate(policy, State(), lambda: NOW, lambda: stopped, lambda _s: None)


def result_message(**overrides: Any) -> ResultMessage:
    fields: dict[str, Any] = {
        "subtype": "success",
        "duration_ms": 1,
        "duration_api_ms": 1,
        "is_error": False,
        "num_turns": 1,
        "session_id": "s1",
        "total_cost_usd": 0.42,
        "structured_output": {"summary": "ok"},
    }
    return ResultMessage(**{**fields, **overrides})


def test_allow_defers_to_auto_mode() -> None:
    assert hook_output(Decision(allowed=True)) == {}


def test_deny_maps_to_pre_tool_use_deny() -> None:
    output = hook_output(Decision(allowed=False, reason=SEND_DENIED, is_send=True))
    assert output == {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": SEND_DENIED,
        }
    }


def test_stop_also_ends_the_session() -> None:
    output = hook_output(Decision(allowed=False, reason=STOPPED, stop=True))
    assert output.get("continue_") is False
    assert output.get("stopReason") == STOPPED


def test_build_options(tmp_path: Path) -> None:
    options = build_options(make_request(tmp_path), make_gate())
    assert options.permission_mode == "auto"
    assert options.model == "sonnet"
    assert options.effort == "medium"
    assert options.max_budget_usd == 1.0
    assert options.cwd == tmp_path
    assert options.plugins == [{"type": "local", "path": str(tmp_path / "plugin")}]
    assert options.output_format == {
        "type": "json_schema",
        "schema": json_schema(HeartbeatResult),
    }
    assert options.can_use_tool is None
    assert options.setting_sources is None


def _call_hook(tmp_path: Path, gate: ToolGate, tool: str, tool_input: Any) -> Any:
    options = build_options(make_request(tmp_path), gate)
    assert options.hooks is not None
    hook = options.hooks["PreToolUse"][0].hooks[0]
    data: Any = {"hook_event_name": "PreToolUse", "tool_name": tool, "tool_input": tool_input}
    return asyncio.run(hook(data, "tool-1", {"signal": None}))


def test_hook_routes_through_gate(tmp_path: Path) -> None:
    send = "mcp__claude_ai_Slack__slack_send_message"
    denied = _call_hook(tmp_path, make_gate(), send, {"channel_id": "C0000000001"})
    assert denied["hookSpecificOutput"]["permissionDecision"] == "deny"
    assert _call_hook(tmp_path, make_gate(), "Read", {"file_path": "x"}) == {}
    assert _call_hook(tmp_path, make_gate(), "Read", "not-a-dict") == {}


def test_hook_stops_on_kill_switch(tmp_path: Path) -> None:
    output = _call_hook(tmp_path, make_gate(stopped=True), "Read", {})
    assert output["continue_"] is False


def test_outcome_from_success() -> None:
    outcome = outcome_from(result_message())
    assert outcome.output == {"summary": "ok"}
    assert outcome.cost_usd == 0.42


@pytest.mark.parametrize(
    "message",
    [
        None,
        result_message(is_error=True, subtype="error_max_budget_usd"),
        result_message(structured_output=None),
        result_message(structured_output="plain text"),
    ],
)
def test_outcome_from_failures(message: ResultMessage | None) -> None:
    with pytest.raises(SessionError):
        outcome_from(message)
