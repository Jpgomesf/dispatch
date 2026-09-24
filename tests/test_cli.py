from pathlib import Path

import pytest
from fakes import FakeSession, fails, returns

from harness.cli import main
from harness.paths import CONFIG_ENV


def _plugin(tmp_path: Path) -> None:
    manifest = tmp_path / "plugin" / ".claude-plugin" / "plugin.json"
    manifest.parent.mkdir(parents=True)
    manifest.write_text('{"name": "claude-harness"}', encoding="utf-8")


def test_check_reports_paths(
    config_file: Path, tmp_path: Path, capsys: pytest.CaptureFixture[str]
) -> None:
    _plugin(tmp_path)
    assert main(["--config", str(config_file), "check"]) == 0
    out = capsys.readouterr().out
    assert str(tmp_path / "state" / "state.json") in out
    assert str(tmp_path / "plugin") in out
    assert "kill switch" in out


def test_check_fails_without_plugin(config_file: Path) -> None:
    assert main(["--config", str(config_file), "check"]) == 1


def test_invalid_config_exits_2(tmp_path: Path, capsys: pytest.CaptureFixture[str]) -> None:
    bad = tmp_path / "bad.toml"
    bad.write_text('[send]\nmode = "yolo"\n', encoding="utf-8")
    assert main(["--config", str(bad), "check"]) == 2
    assert "invalid config" in capsys.readouterr().err


def test_missing_config_exits_2(tmp_path: Path) -> None:
    assert main(["--config", str(tmp_path / "nope.toml"), "check"]) == 2


def test_stop_and_resume(config_file: Path, tmp_path: Path) -> None:
    kill_switch = tmp_path / "state" / "STOP"
    assert main(["--config", str(config_file), "stop"]) == 0
    assert kill_switch.exists()
    assert main(["--config", str(config_file), "resume"]) == 0
    assert not kill_switch.exists()
    assert main(["--config", str(config_file), "resume"]) == 0


def test_config_from_env(
    config_file: Path, tmp_path: Path, monkeypatch: pytest.MonkeyPatch
) -> None:
    monkeypatch.setenv(CONFIG_ENV, str(config_file))
    assert main(["stop"]) == 0
    assert (tmp_path / "state" / "STOP").exists()


def test_card_command(config_file: Path) -> None:
    session = FakeSession(
        [returns({"ref": "EX-1", "status": "done", "pr_url": None, "summary": "ok"})]
    )
    assert main(["--config", str(config_file), "card", "EX-1"], lambda: session) == 0
    assert session.requests[0].prompt.startswith("/claude-harness:workflow card EX-1")


def test_card_command_failure_exit_code(config_file: Path) -> None:
    session = FakeSession([fails()])
    assert main(["--config", str(config_file), "card", "EX-1"], lambda: session) == 1


def test_card_unknown_workspace(config_file: Path) -> None:
    args = ["--config", str(config_file), "card", "EX-1", "--workspace", "missing"]
    assert main(args, lambda: FakeSession([])) == 2


def test_card_refuses_when_stopped(config_file: Path) -> None:
    main(["--config", str(config_file), "stop"])
    session = FakeSession([])
    assert main(["--config", str(config_file), "card", "EX-1"], lambda: session) == 1
    assert session.requests == []


def test_heartbeat_once(config_file: Path, monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr("harness.cli.install_signal_handlers", lambda _stop: None)
    output = {"cursors": {}, "handled": [], "cards_to_work": [], "summary": "quiet"}
    session = FakeSession([returns(output)])
    args = ["--config", str(config_file), "heartbeat", "--once", "--interval", "1m"]
    assert main(args, lambda: session) == 0
    assert len(session.requests) == 1


def test_bad_interval_is_rejected(config_file: Path) -> None:
    with pytest.raises(SystemExit):
        main(["--config", str(config_file), "heartbeat", "--interval", "soon"])
