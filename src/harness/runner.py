from collections.abc import Callable
from dataclasses import dataclass, field
from datetime import UTC, datetime, timedelta
from pathlib import Path
from threading import Event

from pydantic import ValidationError

from harness.config import Config, Workspace
from harness.paths import Paths
from harness.policy import Policy, ToolGate
from harness.prompts import card_prompt, heartbeat_prompt
from harness.results import CardResult, HeartbeatResult, json_schema
from harness.session import Session, SessionError, SessionRequest
from harness.state import State, load_state, save_state

BACKOFF_BASE = timedelta(seconds=30)
MAX_DOUBLINGS = 16
KILL_SWITCH_POLL = timedelta(seconds=5)


def utc_now() -> datetime:
    return datetime.now(UTC)


def backoff_delay(failures: int, interval: timedelta) -> timedelta:
    if failures <= 0:
        return interval
    return min(interval, BACKOFF_BASE * (1 << min(failures - 1, MAX_DOUBLINGS)))


@dataclass
class Runner:
    config: Config
    paths: Paths
    session: Session
    clock: Callable[[], datetime] = utc_now
    out: Callable[[str], None] = print
    stop: Event = field(default_factory=Event)

    def killed(self) -> bool:
        return self.paths.kill_switch.exists()

    def should_stop(self) -> bool:
        return self.stop.is_set() or self.killed()

    def _load(self) -> State:
        return load_state(self.paths.state_file)

    def _save(self, state: State) -> None:
        save_state(self.paths.state_file, state)

    def _gate(self, state: State) -> ToolGate:
        policy = Policy(send=self.config.send, deny=tuple(self.config.tools.deny))
        return ToolGate(policy, state, self.clock, self.killed, self._save)

    def _emit(self, kind: str, status: str, detail: str) -> None:
        self.out(f"{self.clock():%Y-%m-%dT%H:%M:%SZ} {kind} {status} {detail}".rstrip())

    def resolve_workspace(self, ref: str, name: str | None) -> Workspace | None:
        return self.config.workspace_named(name) if name else self.config.workspace_for(ref)

    def run_card(self, ref: str, workspace_name: str | None = None) -> CardResult | None:
        workspace = self.resolve_workspace(ref, workspace_name)
        state = self._load()
        state.set_card(ref, "in_progress", self.clock())
        self._save(state)
        request = SessionRequest(
            prompt=card_prompt(ref, workspace, self.config, self.paths, self.clock()),
            model=self.config.card.model,
            effort=self.config.card.effort,
            max_budget_usd=self.config.card.max_budget_usd,
            cwd=workspace.path if workspace else self._state_dir(),
            plugin_dir=self.paths.plugin_dir,
            output_schema=json_schema(CardResult),
        )
        try:
            outcome = self.session.run(request, self._gate(state))
            result = CardResult.model_validate(outcome.output)
        except (SessionError, ValidationError) as error:
            state.set_card(ref, "failed", self.clock())
            self._save(state)
            self._emit("card", "failed", f"{ref} — {_one_line(error)}")
            return None
        state.set_card(ref, result.status, self.clock(), result.pr_url)
        self._save(state)
        self._emit("card", result.status, f"{ref}{_cost(outcome.cost_usd)} — {result.summary}")
        return result

    def triage(self) -> HeartbeatResult | None:
        state = self._load()
        hb = self.config.heartbeat
        request = SessionRequest(
            prompt=heartbeat_prompt(self.config, self.paths, state, self.clock()),
            model=hb.model,
            effort=hb.effort,
            max_budget_usd=hb.max_budget_usd,
            cwd=self._state_dir(),
            plugin_dir=self.paths.plugin_dir,
            output_schema=json_schema(HeartbeatResult),
        )
        try:
            outcome = self.session.run(request, self._gate(state))
            result = HeartbeatResult.model_validate(outcome.output)
        except (SessionError, ValidationError) as error:
            self._emit("heartbeat", "failed", _one_line(error))
            return None
        state.cursors.update(result.cursors)
        self._save(state)
        detail = f"handled={len(result.handled)} cards={len(result.cards_to_work)}"
        self._emit("heartbeat", "ok", f"{detail}{_cost(outcome.cost_usd)} — {result.summary}")
        return result

    def cards_to_run(self, refs: list[str]) -> list[str]:
        state = self._load()
        pending = [ref for ref in dict.fromkeys(refs) if not state.in_progress(ref)]
        return pending[: self.config.heartbeat.max_cards_per_tick]

    def tick(self) -> bool:
        """One heartbeat tick. Returns False when any session failed."""
        result = self.triage()
        if result is None:
            return False
        ok = True
        for ref in self.cards_to_run(result.cards_to_work):
            if self.should_stop():
                break
            ok = self.run_card(ref) is not None and ok
        return ok

    def release_stale_cards(self) -> None:
        """Cards left in_progress by a crashed run become failed, so triage can pick them up."""
        state = self._load()
        stale = [ref for ref in state.cards if state.in_progress(ref)]
        for ref in stale:
            state.set_card(ref, "failed", self.clock())
        if stale:
            self._save(state)

    def heartbeat(self, interval: timedelta, once: bool = False) -> None:
        self.release_stale_cards()
        failures = 0
        while not self.should_stop():
            failures = 0 if self.tick() else failures + 1
            if once:
                return
            self._sleep(backoff_delay(failures, interval))
        if self.killed():
            self._emit("heartbeat", "stopped", "kill switch present")

    def _sleep(self, duration: timedelta) -> None:
        """Wait, waking early on a signal or kill switch."""
        remaining = duration.total_seconds()
        while remaining > 0 and not self.should_stop():
            step = min(remaining, KILL_SWITCH_POLL.total_seconds())
            self.stop.wait(step)
            remaining -= step

    def _state_dir(self) -> Path:
        self.paths.state_dir.mkdir(parents=True, exist_ok=True)
        return self.paths.state_dir


def _cost(cost_usd: float | None) -> str:
    return f" cost=${cost_usd:.2f}" if cost_usd is not None else ""


def _one_line(error: Exception) -> str:
    return " ".join(str(error).split())[:300]
