import json
from datetime import datetime
from typing import Any

from harness.config import Config, Workspace
from harness.paths import Paths
from harness.state import State

WORKFLOW_COMMAND = "/claude-harness:workflow"


def _render(invocation: str, context: dict[str, Any]) -> str:
    return f"{invocation}\n\n```json\n{json.dumps(context, indent=2, default=str)}\n```\n"


def _workspace_context(workspace: Workspace) -> dict[str, Any]:
    return {"name": workspace.name, "path": str(workspace.path), "match": workspace.match}


def heartbeat_prompt(config: Config, paths: Paths, state: State, now: datetime) -> str:
    context = {
        "now": now.isoformat(),
        "cursors": state.cursors,
        "sources": config.sources.model_dump(),
        "workspaces": [_workspace_context(w) for w in config.workspaces],
        "outreach_file": str(paths.outreach_file),
    }
    return _render(f"{WORKFLOW_COMMAND} heartbeat", context)


def card_prompt(
    ref: str, workspace: Workspace | None, config: Config, paths: Paths, now: datetime
) -> str:
    context = {
        "now": now.isoformat(),
        "ref": ref,
        "workspace": _workspace_context(workspace) if workspace else None,
        "workspaces": [_workspace_context(w) for w in config.workspaces],
        "outreach_file": str(paths.outreach_file),
    }
    return _render(f"{WORKFLOW_COMMAND} card {ref}", context)
