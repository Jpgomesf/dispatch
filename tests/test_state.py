from datetime import timedelta
from pathlib import Path

from fakes import NOW

from harness.state import State, load_state, save_state


def test_missing_state_file_is_empty_state(tmp_path: Path) -> None:
    assert load_state(tmp_path / "state.json") == State()


def test_round_trip(tmp_path: Path) -> None:
    path = tmp_path / "nested" / "state.json"
    state = State(cursors={"slack:C0000000001": "1700000000.000100"})
    state.set_card("EX-1", "done", NOW, "https://example.com/pr/1")
    state.record_send(NOW)
    save_state(path, state)
    assert load_state(path) == state
    assert [p.name for p in path.parent.iterdir()] == ["state.json"]


def test_record_send_prunes_entries_older_than_an_hour() -> None:
    state = State(sends=[NOW - timedelta(hours=2), NOW - timedelta(minutes=10)])
    state.record_send(NOW)
    assert state.sends == [NOW - timedelta(minutes=10), NOW]
    assert len(state.recent_sends(NOW + timedelta(minutes=55))) == 1


def test_in_progress() -> None:
    state = State()
    state.set_card("EX-1", "in_progress", NOW)
    state.set_card("EX-2", "done", NOW)
    assert state.in_progress("EX-1")
    assert not state.in_progress("EX-2")
    assert not state.in_progress("EX-3")
