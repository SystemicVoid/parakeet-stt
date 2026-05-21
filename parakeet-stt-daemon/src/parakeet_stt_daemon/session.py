"""Session lifecycle tracking for push-to-talk interactions."""

from __future__ import annotations

import asyncio
import math
from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import StrEnum
from typing import Literal
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


StreamHelperScope = Literal["live_session_only"]


@dataclass(frozen=True, slots=True)
class StreamPathRuntimeFacts:
    streaming_enabled: bool
    helper_active: bool
    helper_scope: StreamHelperScope
    helper_class_name: str | None
    fallback_reason: str | None
    chunk_secs: float | None
    path_executed: bool
    chunks_processed: int


class StreamPathRuntime:
    """Own Stream path runtime truth for the active and last Daemon Session."""

    def __init__(
        self,
        *,
        streaming_enabled: bool,
        chunk_secs: object,
        streaming_transcriber: object | None,
    ) -> None:
        self.streaming_enabled = bool(streaming_enabled)
        self.chunk_secs = _chunk_secs_or_none(chunk_secs)
        self.streaming_transcriber = streaming_transcriber
        self._current_chunks_processed = 0
        self._current_fallback_reason: str | None = None
        self._last_path_executed = False
        self._last_chunks_processed = 0
        self._last_fallback_reason: str | None = None

    @classmethod
    def from_settings(
        cls,
        settings: object,
        streaming_transcriber: object | None,
    ) -> StreamPathRuntime:
        return cls(
            streaming_enabled=bool(getattr(settings, "streaming_enabled", False)),
            chunk_secs=getattr(settings, "chunk_secs", None),
            streaming_transcriber=streaming_transcriber,
        )

    def sync_from_runtime(
        self,
        *,
        settings: object,
        streaming_transcriber: object | None,
    ) -> None:
        self.streaming_enabled = bool(getattr(settings, "streaming_enabled", False))
        self.chunk_secs = _chunk_secs_or_none(getattr(settings, "chunk_secs", None))
        self.streaming_transcriber = streaming_transcriber

    def should_start_drain_loop(self) -> bool:
        return self.streaming_enabled or self.streaming_transcriber is not None

    def reset_current(self) -> None:
        self._current_chunks_processed = 0
        self._current_fallback_reason = None

    def helper_available(self) -> bool:
        transcriber = self.streaming_transcriber
        return transcriber is not None and bool(getattr(transcriber, "helper_active", False))

    def fallback_reason_from_runtime(self) -> str | None:
        if not self.streaming_enabled:
            return None
        transcriber = self.streaming_transcriber
        if transcriber is None:
            return "streaming_transcriber_unavailable"
        fallback_reason = getattr(transcriber, "fallback_reason", None)
        if fallback_reason is not None:
            return str(fallback_reason)
        if not bool(getattr(transcriber, "helper_active", False)):
            return "streaming_helper_inactive"
        return None

    def record_current_fallback(self, reason: object | None) -> None:
        self._current_fallback_reason = str(reason) if reason is not None else None

    def record_chunk_processed(self) -> None:
        self._current_chunks_processed += 1

    def record_session_result(self, *, active_stream: object | None) -> None:
        chunks_processed = self._current_chunks_processed
        fallback_reason = self._current_fallback_reason
        if active_stream is not None and fallback_reason is None:
            stream_reason = getattr(active_stream, "stream_fallback_reason", None)
            fallback_reason = str(stream_reason) if stream_reason is not None else None
        if self.streaming_enabled:
            if fallback_reason is None:
                fallback_reason = self.fallback_reason_from_runtime()
            if chunks_processed <= 0 and fallback_reason is None:
                fallback_reason = "stream_path_not_exercised:no_chunks"
        else:
            fallback_reason = None
        self._last_chunks_processed = max(0, int(chunks_processed))
        self._last_path_executed = chunks_processed > 0
        self._last_fallback_reason = fallback_reason

    def facts(self, *, active_session: bool) -> StreamPathRuntimeFacts:
        if not self.streaming_enabled:
            return StreamPathRuntimeFacts(
                streaming_enabled=False,
                helper_active=False,
                helper_scope="live_session_only",
                helper_class_name=None,
                fallback_reason=None,
                chunk_secs=None,
                path_executed=False,
                chunks_processed=0,
            )

        transcriber = self.streaming_transcriber
        if transcriber is None:
            return StreamPathRuntimeFacts(
                streaming_enabled=True,
                helper_active=False,
                helper_scope="live_session_only",
                helper_class_name=None,
                fallback_reason="streaming_transcriber_unavailable",
                chunk_secs=self.chunk_secs,
                path_executed=False,
                chunks_processed=0,
            )

        if active_session:
            chunks_processed = self._current_chunks_processed
            path_executed = chunks_processed > 0
            fallback_reason = self._current_fallback_reason
        else:
            chunks_processed = self._last_chunks_processed
            path_executed = self._last_path_executed
            fallback_reason = self._last_fallback_reason

        if fallback_reason is None:
            transcriber_fallback = getattr(transcriber, "fallback_reason", None)
            fallback_reason = (
                str(transcriber_fallback) if transcriber_fallback is not None else None
            )

        return StreamPathRuntimeFacts(
            streaming_enabled=True,
            helper_active=bool(getattr(transcriber, "helper_active", False)),
            helper_scope="live_session_only",
            helper_class_name=_string_or_none(getattr(transcriber, "_helper_class_name", None)),
            fallback_reason=fallback_reason,
            chunk_secs=self.chunk_secs,
            path_executed=path_executed,
            chunks_processed=max(0, int(chunks_processed)),
        )


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
    "StreamPathRuntime",
    "StreamPathRuntimeFacts",
]


def _chunk_secs_or_none(value: object) -> float | None:
    if value is None:
        return None
    if isinstance(value, bool):
        return None
    if not isinstance(value, str | int | float):
        return None
    try:
        parsed = float(value)
    except (TypeError, ValueError):
        return None
    if not math.isfinite(parsed):
        return None
    return parsed


def _string_or_none(value: object) -> str | None:
    if value is None:
        return None
    return str(value)
