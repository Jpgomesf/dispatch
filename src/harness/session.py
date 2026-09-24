"""The only module that talks to the Claude Agent SDK."""

import asyncio
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Protocol, cast

from claude_agent_sdk import (
    ClaudeAgentOptions,
    HookContext,
    HookInput,
    HookJSONOutput,
    HookMatcher,
    ResultMessage,
    query,
)
from claude_agent_sdk.types import SyncHookJSONOutput

from harness.config import Effort
from harness.policy import Decision, ToolGate


@dataclass(frozen=True)
class SessionRequest:
    prompt: str
    model: str
    effort: Effort
    max_budget_usd: float
    cwd: Path
    plugin_dir: Path
    output_schema: dict[str, Any]


@dataclass(frozen=True)
class SessionOutcome:
    output: dict[str, Any]
    cost_usd: float | None = None


class SessionError(Exception):
    pass


class Session(Protocol):
    def run(self, request: SessionRequest, gate: ToolGate) -> SessionOutcome: ...


def hook_output(decision: Decision) -> HookJSONOutput:
    """Map a Decision to PreToolUse output; an allow defers to auto mode's own checks."""
    if decision.allowed:
        return {}
    output: SyncHookJSONOutput = {
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": decision.reason,
        }
    }
    if decision.stop:
        output["continue_"] = False
        output["stopReason"] = decision.reason
    return output


def build_options(request: SessionRequest, gate: ToolGate) -> ClaudeAgentOptions:
    async def pre_tool_use(
        input_data: HookInput, _tool_use_id: str | None, _context: HookContext
    ) -> HookJSONOutput:
        data = cast(dict[str, Any], input_data)
        tool_input = data.get("tool_input")
        decision = gate.check(
            str(data.get("tool_name", "")), tool_input if isinstance(tool_input, dict) else {}
        )
        return hook_output(decision)

    return ClaudeAgentOptions(
        permission_mode="auto",
        model=request.model,
        effort=request.effort,
        max_budget_usd=request.max_budget_usd,
        cwd=request.cwd,
        plugins=[{"type": "local", "path": str(request.plugin_dir)}],
        output_format={"type": "json_schema", "schema": request.output_schema},
        hooks={"PreToolUse": [HookMatcher(matcher=None, hooks=[pre_tool_use])]},
    )


def outcome_from(result: ResultMessage | None) -> SessionOutcome:
    if result is None:
        raise SessionError("session ended without a result")
    if result.is_error or not isinstance(result.structured_output, dict):
        detail = "; ".join(result.errors or []) or result.subtype
        raise SessionError(f"session failed: {detail}")
    return SessionOutcome(output=result.structured_output, cost_usd=result.total_cost_usd)


class SdkSession:
    def run(self, request: SessionRequest, gate: ToolGate) -> SessionOutcome:
        return asyncio.run(self._run(request, gate))

    async def _run(self, request: SessionRequest, gate: ToolGate) -> SessionOutcome:
        result: ResultMessage | None = None
        async for message in query(prompt=request.prompt, options=build_options(request, gate)):
            if isinstance(message, ResultMessage):
                result = message
        return outcome_from(result)
