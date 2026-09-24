from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime
from typing import Any

from harness.policy import ToolGate
from harness.session import SessionError, SessionOutcome, SessionRequest

NOW = datetime(2026, 1, 15, 9, 30, tzinfo=UTC)

Responder = Callable[[SessionRequest, ToolGate], dict[str, Any]]


@dataclass
class FakeSession:
    """Scripted session: pops one responder per run; a raised SessionError simulates failure."""

    responders: list[Responder | Exception]
    requests: list[SessionRequest] = field(default_factory=list)

    def run(self, request: SessionRequest, gate: ToolGate) -> SessionOutcome:
        self.requests.append(request)
        responder = self.responders.pop(0)
        if isinstance(responder, Exception):
            raise responder
        return SessionOutcome(output=responder(request, gate), cost_usd=0.05)


def returns(output: dict[str, Any]) -> Responder:
    return lambda _request, _gate: output


def fails(message: str = "boom") -> SessionError:
    return SessionError(message)
