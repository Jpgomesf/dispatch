import re
from datetime import timedelta

_DURATION = re.compile(r"^\s*(\d+(?:\.\d+)?)\s*([smh])\s*$")
_UNIT_SECONDS = {"s": 1, "m": 60, "h": 3600}


def parse_duration(text: str) -> timedelta:
    """Parse durations like "30s", "10m", "1.5h"."""
    match = _DURATION.match(text)
    if match is None:
        raise ValueError(f"invalid duration {text!r}: expected <number><s|m|h>, e.g. '10m'")
    value, unit = float(match.group(1)), match.group(2)
    if value <= 0:
        raise ValueError(f"invalid duration {text!r}: must be positive")
    return timedelta(seconds=value * _UNIT_SECONDS[unit])
