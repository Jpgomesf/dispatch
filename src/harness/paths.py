import os
from collections.abc import Mapping
from dataclasses import dataclass
from pathlib import Path

from harness.config import DEFAULT_CONFIG_DIR, Config

CONFIG_ENV = "HARNESS_CONFIG"
REPO_ROOT = Path(__file__).resolve().parents[2]


def resolve_config_path(cli_value: str | None, env: Mapping[str, str] = os.environ) -> Path:
    raw = cli_value or env.get(CONFIG_ENV) or str(DEFAULT_CONFIG_DIR / "config.toml")
    return Path(raw).expanduser()


@dataclass(frozen=True)
class Paths:
    config: Path
    state_dir: Path
    outreach_file: Path
    plugin_dir: Path

    @property
    def state_file(self) -> Path:
        return self.state_dir / "state.json"

    @property
    def kill_switch(self) -> Path:
        return self.state_dir / "STOP"

    @classmethod
    def resolve(cls, config_path: Path, config: Config) -> "Paths":
        return cls(
            config=config_path,
            state_dir=config.state_dir,
            outreach_file=config.outreach_file,
            plugin_dir=config.plugin_dir or REPO_ROOT / "plugin",
        )
