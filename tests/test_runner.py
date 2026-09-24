import json
from datetime import timedelta
from pathlib import Path
from typing import Any

import pytest
from fakes import NOW, FakeSession, Responder, fails, returns

from harness.config import Config, load_config
from harness.paths import Paths
from harness.policy import ToolGate
from harness.runner import BACKOFF_BASE, Runner, backoff_delay
from harness.session import SessionRequest
from harness.state import load_state, save_state

SEND = "mcp__claude_ai_Slack__slack_send_message"


def heartbeat_output(cards: list[str] | None = None, **cursors: str) -> dict[str, Any]:
    return {
        "cursors": cursors,
        "handled": [{"source": "slack:C0000000001", "item": "m1", "action": "drafted"}],
        "cards_to_work": cards or [],
        "summary": "one quick reply drafted",
    }


def card_output(ref: str, status: str = "done") -> dict[str, Any]:
    return {
        "ref": ref,
        "status": status,
        "pr_url": "https://example.com/pr/7" if status == "done" else None,
        "blocked_on": None,
        "summary": "implemented",
    }


def make_runner(
    config_file: Path, paths: Paths, responders: list[Responder | Exception]
) -> tuple[Runner, FakeSession, list[str]]:
    lines: list[str] = []
    session = FakeSession(responders)
    runner = Runner(load_config(config_file), paths, session, lambda: NOW, lines.append)
    return runner, session, lines


def context_of(request: SessionRequest) -> dict[str, Any]:
    body = request.prompt.split("```json\n", 1)[1].rsplit("```", 1)[0]
    parsed: dict[str, Any] = json.loads(body)
    return parsed


def test_backoff_doubles_and_caps_at_interval() -> None:
    interval = timedelta(minutes=10)
    assert backoff_delay(0, interval) == interval
    assert backoff_delay(1, interval) == BACKOFF_BASE
    assert backoff_delay(2, interval) == BACKOFF_BASE * 2
    assert backoff_delay(3, interval) == BACKOFF_BASE * 4
    assert backoff_delay(10, interval) == interval
    assert backoff_delay(10_000, interval) == interval


def test_tick_persists_cursors_and_runs_cards(config_file: Path, paths: Paths) -> None:
    save_state(
        paths.state_file,
        load_state(paths.state_file).model_copy(update={"cursors": {"tracker:linear": "old"}}),
    )
    runner, session, lines = make_runner(
        config_file,
        paths,
        [
            returns(heartbeat_output(["EX-1", "EX-1", "OTHER-2"], **{"slack:C0000000001": "c9"})),
            returns(card_output("EX-1")),
            returns(card_output("OTHER-2", "blocked")),
        ],
    )
    assert runner.tick()

    state = load_state(paths.state_file)
    assert state.cursors == {"tracker:linear": "old", "slack:C0000000001": "c9"}
    assert state.cards["EX-1"].status == "done"
    assert state.cards["EX-1"].pr_url == "https://example.com/pr/7"
    assert state.cards["OTHER-2"].status == "blocked"

    triage, card_ex, card_other = session.requests
    assert triage.prompt.startswith("/claude-harness:workflow heartbeat\n")
    assert triage.model == "sonnet"
    assert triage.cwd == paths.state_dir
    assert context_of(triage)["cursors"] == {"tracker:linear": "old"}
    assert card_ex.prompt.startswith("/claude-harness:workflow card EX-1\n")
    assert card_ex.model == "claude-opus-5-5"
    assert card_ex.effort == "high"
    assert card_ex.cwd == runner.config.workspaces[0].path
    assert card_other.cwd == paths.state_dir
    assert len(lines) == 3
    assert "heartbeat ok handled=1 cards=3" in lines[0]


def test_card_context_includes_all_workspaces(config_file: Path, paths: Paths) -> None:
    runner, session, _ = make_runner(config_file, paths, [returns(card_output("EX-3"))])
    runner.run_card("EX-3")
    context = context_of(session.requests[0])
    assert context["ref"] == "EX-3"
    assert context["workspace"]["name"] == "example-app"
    assert [w["name"] for w in context["workspaces"]] == ["example-app"]
    assert context["outreach_file"] == str(paths.outreach_file)


def test_tick_respects_max_cards_and_skips_in_progress(config_file: Path, paths: Paths) -> None:
    state = load_state(paths.state_file)
    state.set_card("EX-1", "in_progress", NOW)
    save_state(paths.state_file, state)
    runner, session, _ = make_runner(
        config_file,
        paths,
        [
            returns(heartbeat_output(["EX-1", "EX-2", "EX-3", "EX-4"])),
            returns(card_output("EX-2")),
            returns(card_output("EX-3")),
        ],
    )
    assert runner.tick()
    assert [r.prompt.split("\n")[0] for r in session.requests[1:]] == [
        "/claude-harness:workflow card EX-2",
        "/claude-harness:workflow card EX-3",
    ]


def test_failed_triage_reports_failure(config_file: Path, paths: Paths) -> None:
    runner, _, lines = make_runner(config_file, paths, [fails("budget exceeded")])
    assert not runner.tick()
    assert "heartbeat failed budget exceeded" in lines[0]


def test_invalid_structured_output_is_a_failure(config_file: Path, paths: Paths) -> None:
    runner, _, _ = make_runner(config_file, paths, [returns({"unexpected": True})])
    assert not runner.tick()


def test_failed_card_is_marked_failed(config_file: Path, paths: Paths) -> None:
    runner, _, lines = make_runner(
        config_file, paths, [returns(heartbeat_output(["EX-1"])), fails()]
    )
    assert not runner.tick()
    assert load_state(paths.state_file).cards["EX-1"].status == "failed"
    assert "card failed EX-1" in lines[1]


def test_gate_sends_are_persisted(config_file: Path, paths: Paths) -> None:
    def sends_twice(_request: SessionRequest, gate: ToolGate) -> dict[str, Any]:
        assert gate.check(SEND, {"channel_id": "C0000000001"}).allowed
        assert gate.check(SEND, {"channel_id": "C0000000001"}).allowed
        assert not gate.check(SEND, {"channel_id": "C0000000001"}).allowed
        return heartbeat_output()

    runner, _, _ = make_runner(config_file, paths, [sends_twice])
    assert runner.tick()
    assert load_state(paths.state_file).sends == [NOW, NOW]


def test_heartbeat_once_runs_a_single_tick(config_file: Path, paths: Paths) -> None:
    runner, session, _ = make_runner(config_file, paths, [returns(heartbeat_output())])
    runner.heartbeat(timedelta(minutes=10), once=True)
    assert len(session.requests) == 1


def test_heartbeat_loop_backs_off_then_stops(
    config_file: Path, paths: Paths, monkeypatch: pytest.MonkeyPatch
) -> None:
    runner, session, _ = make_runner(
        config_file, paths, [fails(), fails(), returns(heartbeat_output())]
    )
    waits: list[float] = []

    def fake_wait(timeout: float) -> bool:
        waits.append(timeout)
        if len(waits) == 3:
            runner.stop.set()
        return runner.stop.is_set()

    monkeypatch.setattr("harness.runner.KILL_SWITCH_POLL", timedelta(hours=1))
    monkeypatch.setattr(runner.stop, "wait", fake_wait)
    runner.heartbeat(timedelta(minutes=10))
    assert waits == [30.0, 60.0, 600.0]
    assert len(session.requests) == 3


def test_kill_switch_prevents_ticks(config_file: Path, paths: Paths) -> None:
    paths.state_dir.mkdir(parents=True)
    paths.kill_switch.touch()
    runner, session, lines = make_runner(config_file, paths, [])
    runner.heartbeat(timedelta(minutes=10))
    assert session.requests == []
    assert "stopped kill switch present" in lines[0]


def test_kill_switch_between_cards(config_file: Path, paths: Paths) -> None:
    def card_then_stop(_request: SessionRequest, _gate: ToolGate) -> dict[str, Any]:
        paths.kill_switch.touch()
        return card_output("EX-1")

    runner, session, _ = make_runner(
        config_file, paths, [returns(heartbeat_output(["EX-1", "EX-2"])), card_then_stop]
    )
    runner.heartbeat(timedelta(minutes=10))
    assert len(session.requests) == 2


def test_run_card_with_named_workspace(config_file: Path, paths: Paths) -> None:
    runner, session, _ = make_runner(config_file, paths, [returns(card_output("ZZ-1"))])
    runner.run_card("ZZ-1", "example-app")
    assert session.requests[0].cwd == runner.config.workspaces[0].path


def test_run_card_unknown_workspace_raises(config_file: Path, paths: Paths) -> None:
    runner, _, _ = make_runner(config_file, paths, [])
    with pytest.raises(ValueError, match="no workspace"):
        runner.run_card("EX-1", "missing")


def test_sleep_wakes_on_kill_switch(
    config_file: Path, paths: Paths, monkeypatch: pytest.MonkeyPatch
) -> None:
    runner, _, _ = make_runner(config_file, paths, [])
    paths.state_dir.mkdir(parents=True)
    waits: list[float] = []

    def fake_wait(timeout: float) -> bool:
        waits.append(timeout)
        paths.kill_switch.touch()
        return False

    monkeypatch.setattr(runner.stop, "wait", fake_wait)
    runner._sleep(timedelta(minutes=10))
    assert waits == [5.0]


def test_runner_accepts_default_config(tmp_path: Path, paths: Paths) -> None:
    runner = Runner(Config(), paths, FakeSession([]))
    assert not runner.killed()


def test_heartbeat_releases_cards_left_in_progress(config_file: Path, paths: Paths) -> None:
    state = load_state(paths.state_file)
    state.set_card("EX-1", "in_progress", NOW)
    save_state(paths.state_file, state)
    runner, session, _ = make_runner(
        config_file,
        paths,
        [returns(heartbeat_output(["EX-1"])), returns(card_output("EX-1"))],
    )
    runner.heartbeat(timedelta(minutes=10), once=True)
    assert session.requests[1].prompt.split("\n")[0] == "/claude-harness:workflow card EX-1"
    assert load_state(paths.state_file).cards["EX-1"].status == "done"
