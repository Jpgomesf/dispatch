from datetime import timedelta

import pytest

from harness.durations import parse_duration


@pytest.mark.parametrize(
    ("text", "expected"),
    [
        ("30s", timedelta(seconds=30)),
        ("10m", timedelta(minutes=10)),
        ("2h", timedelta(hours=2)),
        ("1.5h", timedelta(minutes=90)),
        (" 5 m ", timedelta(minutes=5)),
    ],
)
def test_parses_valid_durations(text: str, expected: timedelta) -> None:
    assert parse_duration(text) == expected


@pytest.mark.parametrize("text", ["", "10", "m", "10d", "-5m", "0s", "ten minutes"])
def test_rejects_invalid_durations(text: str) -> None:
    with pytest.raises(ValueError, match="invalid duration"):
        parse_duration(text)
