from datetime import timedelta
from typing import Any

import pytest
from fakes import NOW

from harness.config import SendConfig, SendMode
from harness.policy import (
    DENIED_BY_CONFIG,
    DESTRUCTIVE,
    SEND_DENIED,
    STOPPED,
    Decision,
    Policy,
    ToolGate,
    decide,
    is_send_tool,
)
from harness.state import State

SLACK_SEND = "mcp__claude_ai_Slack__slack_send_message"
SLACK_SCHEDULE = "mcp__claude_ai_Slack__slack_schedule_message"
SLACK_DRAFT = "mcp__claude_ai_Slack__slack_send_message_draft"
GMAIL_SEND = "mcp__claude_ai_Gmail__send_message"
GMAIL_REPLY = "mcp__claude_ai_Gmail__reply"
GMAIL_FORWARD = "mcp__claude_ai_Gmail__forward"
GMAIL_DRAFT = "mcp__claude_ai_Gmail__create_draft"

ALLOWED_CHANNEL = {"channel_id": "C0000000001", "message": "hi"}
ALLOWED_USER_DM = {"channel_id": "U0000000001", "message": "hi"}
OTHER_CHANNEL = {"channel_id": "C0000000099", "message": "hi"}
ALLOWED_EMAIL = {"to": ["alex@example.com"], "subject": "s", "body": "b"}


def make_policy(mode: SendMode = "allowlist", deny: tuple[str, ...] = ()) -> Policy:
    send = SendConfig(
        mode=mode,
        slack_channels=["C0000000001"],
        slack_users=["U0000000001"],
        email_domains=["Example.com"],
        max_per_hour=2,
    )
    return Policy(send=send, deny=deny)


def run(
    tool: str,
    tool_input: dict[str, Any],
    policy: Policy | None = None,
    state: State | None = None,
    stopped: bool = False,
) -> Decision:
    return decide(tool, tool_input, policy or make_policy(), state or State(), NOW, stopped=stopped)


@pytest.mark.parametrize(
    ("tool", "tool_input"),
    [
        (SLACK_SEND, ALLOWED_CHANNEL),
        (SLACK_SEND, ALLOWED_USER_DM),
        (SLACK_SCHEDULE, {**ALLOWED_CHANNEL, "post_at": 1}),
        ("slack_send_message", ALLOWED_CHANNEL),
        ("mcp__other_prefix__slack_send_message", ALLOWED_CHANNEL),
        (GMAIL_SEND, ALLOWED_EMAIL),
        (GMAIL_SEND, {"to": "alex@example.com, sam@EXAMPLE.com"}),
        (GMAIL_REPLY, {"messageId": "m1", "to": ["alex@example.com"]}),
        (GMAIL_FORWARD, {"messageId": "m1", "to": ["alex@example.com"], "cc": ["sam@example.com"]}),
    ],
)
def test_allowlist_hit_allows_send(tool: str, tool_input: dict[str, Any]) -> None:
    assert run(tool, tool_input) == Decision(allowed=True, is_send=True)


@pytest.mark.parametrize(
    ("tool", "tool_input"),
    [
        (SLACK_SEND, OTHER_CHANNEL),
        (SLACK_SEND, {"message": "no target"}),
        (SLACK_SEND, {"channel_id": 42}),
        (SLACK_SEND, {"channel_id": ["C0000000001", 7]}),
        (GMAIL_SEND, {"to": ["alex@example.org"]}),
        (GMAIL_SEND, {"to": ["alex@example.com"], "bcc": ["spy@elsewhere.test"]}),
        (GMAIL_SEND, {"to": ["not-an-email"]}),
        (GMAIL_SEND, {"draftId": "d1"}),
        (GMAIL_REPLY, {"messageId": "m1"}),
        (GMAIL_SEND, {"to": {"address": "alex@example.com"}}),
    ],
)
def test_allowlist_miss_or_unknown_shape_denies(tool: str, tool_input: dict[str, Any]) -> None:
    decision = run(tool, tool_input)
    assert decision == Decision(allowed=False, reason=SEND_DENIED, is_send=True)


def test_mode_all_allows_any_target() -> None:
    assert run(SLACK_SEND, OTHER_CHANNEL, make_policy("all")).allowed
    assert run(GMAIL_REPLY, {"messageId": "m1"}, make_policy("all")).allowed


def test_mode_draft_only_denies_even_allowlisted() -> None:
    decision = run(SLACK_SEND, ALLOWED_CHANNEL, make_policy("draft_only"))
    assert not decision.allowed
    assert decision.reason == SEND_DENIED


@pytest.mark.parametrize("mode", ["all", "allowlist"])
def test_rate_limit(mode: SendMode) -> None:
    full = State(sends=[NOW - timedelta(minutes=5), NOW - timedelta(minutes=50)])
    assert not run(SLACK_SEND, ALLOWED_CHANNEL, make_policy(mode), full).allowed
    expired = State(sends=[NOW - timedelta(minutes=61), NOW - timedelta(minutes=50)])
    assert run(SLACK_SEND, ALLOWED_CHANNEL, make_policy(mode), expired).allowed


@pytest.mark.parametrize("mode", ["all", "allowlist", "draft_only"])
@pytest.mark.parametrize("tool", [SLACK_DRAFT, GMAIL_DRAFT, "mcp__x__update_draft"])
def test_draft_tools_always_allowed(mode: SendMode, tool: str) -> None:
    full = State(sends=[NOW, NOW, NOW])
    assert run(tool, OTHER_CHANNEL, make_policy(mode), full) == Decision(allowed=True)


@pytest.mark.parametrize(
    "tool",
    [
        "mcp__claude_ai_Gmail__trash_message",
        "mcp__claude_ai_Gmail__trash_thread",
        "mcp__claude_ai_Gmail__delete_draft",
        "mcp__claude_ai_Slack__slack_delete_message",
    ],
)
def test_destructive_messaging_tools_denied(tool: str) -> None:
    assert run(tool, {}, make_policy("all")) == Decision(allowed=False, reason=DESTRUCTIVE)


def test_non_messaging_delete_is_not_blocked() -> None:
    assert run("mcp__tracker__delete_comment", {}).allowed


def test_deny_list_by_prefix_and_name() -> None:
    policy = make_policy("all", deny=("mcp__claude_ai_Notion", "WebFetch", "slack_send_message"))
    assert run("mcp__claude_ai_Notion__notion-search", {}, policy).reason == DENIED_BY_CONFIG
    assert run("WebFetch", {}, policy).reason == DENIED_BY_CONFIG
    assert run(SLACK_SEND, ALLOWED_CHANNEL, policy).reason == DENIED_BY_CONFIG
    assert run("Read", {}, policy).allowed


def test_kill_switch_denies_everything_and_stops() -> None:
    for tool in ("Read", SLACK_DRAFT, SLACK_SEND):
        decision = run(tool, ALLOWED_CHANNEL, make_policy("all"), stopped=True)
        assert decision == Decision(allowed=False, reason=STOPPED, stop=True)


def test_other_tools_allowed() -> None:
    assert run("Bash", {"command": "ls"}) == Decision(allowed=True)
    assert run("mcp__claude_ai_Slack__slack_read_channel", OTHER_CHANNEL).allowed


@pytest.mark.parametrize(
    ("tool", "expected"),
    [
        (SLACK_SEND, True),
        (GMAIL_REPLY, True),
        (GMAIL_FORWARD, True),
        (SLACK_DRAFT, False),
        ("mcp__claude_ai_Canva__reply-to-comment", False),
        ("mcp__claude_ai_Slack__slack_read_channel", False),
        ("mcp__claude_ai_Gmail__forwarding_settings", False),
    ],
)
def test_send_tool_suffix_matching(tool: str, expected: bool) -> None:
    assert is_send_tool(tool) is expected


def test_gate_records_allowed_sends_and_persists() -> None:
    state = State()
    saved: list[int] = []
    gate = ToolGate(
        make_policy(), state, lambda: NOW, lambda: False, lambda s: saved.append(len(s.sends))
    )
    assert gate.check(SLACK_SEND, ALLOWED_CHANNEL).allowed
    assert gate.check(SLACK_SEND, ALLOWED_CHANNEL).allowed
    assert not gate.check(SLACK_SEND, ALLOWED_CHANNEL).allowed
    assert gate.check(SLACK_DRAFT, ALLOWED_CHANNEL).allowed
    assert saved == [1, 2]
    assert state.sends == [NOW, NOW]


def test_gate_reads_kill_switch_each_call() -> None:
    flag = {"stopped": False}
    gate = ToolGate(make_policy(), State(), lambda: NOW, lambda: flag["stopped"], lambda _s: None)
    assert gate.check("Read", {}).allowed
    flag["stopped"] = True
    assert gate.check("Read", {}).stop


@pytest.mark.parametrize(
    ("tool", "tool_input"),
    [
        (GMAIL_SEND, {"draftId": "d1", "to": ["alex@example.com"]}),
        (GMAIL_REPLY, {"messageId": "m1", "replyAll": True, "to": ["alex@example.com"]}),
    ],
)
def test_allowlisted_to_cannot_mask_real_recipients(tool: str, tool_input: dict[str, Any]) -> None:
    assert not run(tool, tool_input).allowed


@pytest.mark.parametrize(
    "tool", ["mcp__claude_ai_Gmail__untrash_message", "mcp__claude_ai_Gmail__untrash_thread"]
)
def test_untrash_is_not_destructive(tool: str) -> None:
    assert run(tool, {"messageId": "m1"}).allowed
