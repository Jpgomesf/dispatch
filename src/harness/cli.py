import argparse
import signal
import sys
from collections.abc import Callable, Sequence
from pathlib import Path
from threading import Event
from types import FrameType

from pydantic import ValidationError

from harness.config import Config, load_config
from harness.durations import parse_duration
from harness.paths import Paths, resolve_config_path
from harness.runner import Runner
from harness.session import Session

SessionFactory = Callable[[], Session]


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(prog="harness", description="claude-harness runner")
    parser.add_argument("--config", help="config.toml path (default: $HARNESS_CONFIG or ~/.config)")
    commands = parser.add_subparsers(dest="command", required=True)

    heartbeat = commands.add_parser("heartbeat", help="triage loop over Slack and the tracker")
    heartbeat.add_argument("--interval", type=parse_duration, help="override, e.g. 10m")
    heartbeat.add_argument("--once", action="store_true", help="run a single tick and exit")

    card = commands.add_parser("card", help="work one card end to end")
    card.add_argument("ref")
    card.add_argument("--workspace", help="workspace name from config")

    commands.add_parser("stop", help="create the kill switch")
    commands.add_parser("resume", help="remove the kill switch")
    commands.add_parser("check", help="validate config and print resolved paths")
    return parser


def _load(config_path: Path, allow_missing: bool = False) -> Config:
    if allow_missing and not config_path.exists():
        return Config()
    return load_config(config_path)


def install_signal_handlers(stop: Event) -> None:
    def handle(signum: int, _frame: FrameType | None) -> None:
        if stop.is_set():
            raise KeyboardInterrupt
        stop.set()
        print(f"received signal {signum}; exiting after the current run", file=sys.stderr)

    signal.signal(signal.SIGINT, handle)
    signal.signal(signal.SIGTERM, handle)


def cmd_check(paths: Paths) -> int:
    plugin_ok = (paths.plugin_dir / ".claude-plugin" / "plugin.json").is_file()
    print(f"config:       {paths.config} (ok)")
    print(f"state dir:    {paths.state_dir}")
    print(f"state file:   {paths.state_file}")
    print(f"kill switch:  {paths.kill_switch} ({'SET' if paths.kill_switch.exists() else 'off'})")
    print(f"outreach:     {paths.outreach_file}")
    print(f"plugin dir:   {paths.plugin_dir} ({'ok' if plugin_ok else 'MISSING plugin.json'})")
    return 0 if plugin_ok else 1


def cmd_stop(paths: Paths) -> int:
    paths.state_dir.mkdir(parents=True, exist_ok=True)
    paths.kill_switch.touch()
    print(f"kill switch set: {paths.kill_switch}")
    return 0


def cmd_resume(paths: Paths) -> int:
    paths.kill_switch.unlink(missing_ok=True)
    print(f"kill switch removed: {paths.kill_switch}")
    return 0


def cmd_heartbeat(runner: Runner, args: argparse.Namespace) -> int:
    install_signal_handlers(runner.stop)
    interval = args.interval or runner.config.heartbeat.interval_delta
    runner.heartbeat(interval, once=args.once)
    return 0


def cmd_card(runner: Runner, args: argparse.Namespace) -> int:
    if runner.killed():
        print(f"kill switch present ({runner.paths.kill_switch}); not starting", file=sys.stderr)
        return 1
    try:
        result = runner.run_card(args.ref, args.workspace)
    except ValueError as error:
        print(str(error), file=sys.stderr)
        return 2
    return 0 if result is not None and result.status != "failed" else 1


def _default_session() -> Session:
    from harness.session import SdkSession

    return SdkSession()


def main(argv: Sequence[str] | None = None, session_factory: SessionFactory | None = None) -> int:
    args = build_parser().parse_args(argv)
    config_path = resolve_config_path(args.config)
    try:
        config = _load(config_path, allow_missing=args.command in ("stop", "resume"))
    except (OSError, ValueError, ValidationError) as error:
        print(f"invalid config {config_path}: {error}", file=sys.stderr)
        return 2
    paths = Paths.resolve(config_path, config)
    if args.command == "check":
        return cmd_check(paths)
    if args.command == "stop":
        return cmd_stop(paths)
    if args.command == "resume":
        return cmd_resume(paths)
    runner = Runner(config, paths, (session_factory or _default_session)())
    if args.command == "heartbeat":
        return cmd_heartbeat(runner, args)
    return cmd_card(runner, args)


if __name__ == "__main__":
    sys.exit(main())
