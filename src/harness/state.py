import os
import tempfile
from datetime import datetime, timedelta
from pathlib import Path
from typing import Literal

from pydantic import BaseModel, Field

CardStatus = Literal["in_progress", "done", "blocked", "failed"]
SEND_WINDOW = timedelta(hours=1)


class CardState(BaseModel):
    status: CardStatus
    updated_at: datetime
    pr_url: str | None = None


class State(BaseModel):
    cursors: dict[str, str] = Field(default_factory=dict)
    cards: dict[str, CardState] = Field(default_factory=dict)
    sends: list[datetime] = Field(default_factory=list)

    def recent_sends(self, now: datetime) -> list[datetime]:
        return [sent for sent in self.sends if now - sent < SEND_WINDOW]

    def record_send(self, now: datetime) -> None:
        self.sends = [*self.recent_sends(now), now]

    def in_progress(self, ref: str) -> bool:
        card = self.cards.get(ref)
        return card is not None and card.status == "in_progress"

    def set_card(
        self, ref: str, status: CardStatus, now: datetime, pr_url: str | None = None
    ) -> None:
        self.cards[ref] = CardState(status=status, updated_at=now, pr_url=pr_url)


def load_state(path: Path) -> State:
    if not path.exists():
        return State()
    return State.model_validate_json(path.read_text(encoding="utf-8"))


def save_state(path: Path, state: State) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_name = tempfile.mkstemp(dir=path.parent, prefix=".state-", suffix=".json")
    tmp_path = Path(tmp_name)
    try:
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(state.model_dump_json(indent=2))
            handle.flush()
            os.fsync(handle.fileno())
        tmp_path.replace(path)
    except BaseException:
        tmp_path.unlink(missing_ok=True)
        raise
