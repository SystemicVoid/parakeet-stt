"""Session lifecycle tracking for push-to-talk interactions."""

from __future__ import annotations

import asyncio
from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import StrEnum
from uuid import UUID


class SessionState(StrEnum):
    IDLE = "idle"
    LISTENING = "listening"
    PROCESSING = "processing"


@dataclass
class Session:
    session_id: UUID
    owner_token: int
    started_at: datetime = field(default_factory=lambda: datetime.now(tz=UTC))
    state: SessionState = SessionState.LISTENING
    last_updated: datetime = field(default_factory=lambda: datetime.now(tz=UTC))

    def mark_processing(self) -> None:
        self.state = SessionState.PROCESSING
        self.last_updated = datetime.now(tz=UTC)

    def mark_completed(self) -> None:
        self.state = SessionState.IDLE
        self.last_updated = datetime.now(tz=UTC)

    @property
    def audio_duration_ms(self) -> int:
        return int((datetime.now(tz=UTC) - self.started_at).total_seconds() * 1000)


class SessionBusyError(RuntimeError):
    pass


class SessionNotFoundError(RuntimeError):
    pass


class SessionManager:
    """Coordinate access to the single active session allowed by the daemon."""

    def __init__(self) -> None:
        self._active: Session | None = None
        self._lock = asyncio.Lock()

    @property
    def active(self) -> Session | None:
        return self._active

    async def start_session(self, session_id: UUID, *, owner_token: int) -> Session:
        async with self._lock:
            if self._active and self._active.state != SessionState.IDLE:
                raise SessionBusyError("A session is already active")
            self._active = Session(session_id=session_id, owner_token=owner_token)
            return self._active

    async def stop_session(self, session_id: UUID, *, owner_token: int | None = None) -> Session:
        async with self._lock:
            if not self._active or self._active.session_id != session_id:
                raise SessionNotFoundError("No matching active session")
            if owner_token is not None and self._active.owner_token != owner_token:
                raise SessionNotFoundError("No matching active session")
            session = self._active
            session.mark_processing()
            return session

    async def clear(self, session_id: UUID, *, owner_token: int | None = None) -> None:
        async with self._lock:
            if (
                self._active
                and self._active.session_id == session_id
                and (owner_token is None or self._active.owner_token == owner_token)
            ):
                self._active.mark_completed()
                self._active = None


__all__ = [
    "Session",
    "SessionBusyError",
    "SessionManager",
    "SessionNotFoundError",
    "SessionState",
]
