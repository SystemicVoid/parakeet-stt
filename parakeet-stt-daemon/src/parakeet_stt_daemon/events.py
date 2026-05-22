"""Session event vocabulary for the Daemon, Session lifecycle, and Overlay adapters."""

from __future__ import annotations

import asyncio
import math
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from datetime import datetime
from typing import Any, Literal, Protocol, TypeAlias
from uuid import UUID

from loguru import logger

from .messages import (
    AudioLevelMessage,
    ErrorMessage,
    FinalResult,
    InterimStateMessage,
    InterimStateValue,
    InterimTextMessage,
    SessionEndedMessage,
    SessionEndReason,
    SessionStarted,
    SessionWarningMessage,
)

OVERLAY_SESSION_STATE_CACHE_LIMIT = 128

ErrorCode: TypeAlias = Literal[
    "SESSION_BUSY",
    "SESSION_NOT_FOUND",
    "SESSION_ABORTED",
    "AUDIO_DEVICE",
    "MODEL",
    "INVALID_REQUEST",
    "UNEXPECTED",
]


class JsonWebSocket(Protocol):
    async def send_json(self, data: dict[str, Any]) -> None: ...


@dataclass(frozen=True, slots=True)
class SessionStartedEvent:
    session_id: UUID
    ts: datetime
    mic_device: str | None
    lang: str | None


@dataclass(frozen=True, slots=True)
class InterimStateEvent:
    session_id: UUID
    state: InterimStateValue


@dataclass(frozen=True, slots=True)
class InterimTextEvent:
    session_id: UUID
    text: str


@dataclass(frozen=True, slots=True)
class AudioLevelEvent:
    session_id: UUID
    rms: float


@dataclass(frozen=True, slots=True)
class FinalResultEvent:
    session_id: UUID
    text: str
    latency_ms: int
    audio_ms: int
    lang: str | None
    confidence: float | None
    tail_trim_mode: Literal["rms", "vad"] | None
    vad_active: bool | None
    vad_fallback_reason: str | None


@dataclass(frozen=True, slots=True)
class SessionEndedEvent:
    session_id: UUID
    reason: SessionEndReason


@dataclass(frozen=True, slots=True)
class SessionWarningEvent:
    session_id: UUID
    remaining_seconds: float
    limit_seconds: float
    warning: Literal["approaching_limit"] = "approaching_limit"


@dataclass(frozen=True, slots=True)
class SessionErrorEvent:
    session_id: UUID | None
    code: ErrorCode
    message: str


SessionEvent: TypeAlias = (
    SessionStartedEvent
    | InterimStateEvent
    | InterimTextEvent
    | AudioLevelEvent
    | FinalResultEvent
    | SessionEndedEvent
    | SessionWarningEvent
    | SessionErrorEvent
)


class EventSink(Protocol):
    async def emit(self, event: SessionEvent) -> None: ...


class EventSinkClosed(RuntimeError):
    """Raised when a transport-backed event sink is no longer writable."""


@dataclass
class RecordingEventSink:
    events: list[SessionEvent] = field(default_factory=list)

    async def emit(self, event: SessionEvent) -> None:
        self.events.append(event)


class OverlayEventAdapter:
    """Own Overlay event sequencing, de-dupe, drop accounting, and delivery."""

    def __init__(
        self,
        *,
        overlay_events_enabled: bool,
        send_payload: Callable[[dict[str, Any]], Awaitable[None]],
        send_lock: Callable[[], asyncio.Lock],
        next_overlay_seq: Callable[[UUID], int],
        overlay_session_state: Callable[[UUID], str | None],
        set_overlay_session_state: Callable[[UUID, str], None],
        last_interim_text: Callable[[UUID], str | None],
        record_interim_text: Callable[[UUID, str], None],
        clear_interim_text: Callable[[UUID], None],
        increment_overlay_events_emitted: Callable[[], None],
        increment_overlay_events_dropped: Callable[[], None],
        clear_session_runtime: Callable[[UUID], None],
        mark_session_terminal: Callable[[UUID], None],
    ) -> None:
        self._overlay_events_enabled = overlay_events_enabled
        self._send_payload = send_payload
        self._send_lock = send_lock
        self._next_overlay_seq = next_overlay_seq
        self._overlay_session_state = overlay_session_state
        self._set_overlay_session_state = set_overlay_session_state
        self._last_interim_text = last_interim_text
        self._record_interim_text = record_interim_text
        self._clear_interim_text = clear_interim_text
        self._increment_overlay_events_emitted = increment_overlay_events_emitted
        self._increment_overlay_events_dropped = increment_overlay_events_dropped
        self._clear_session_runtime = clear_session_runtime
        self._mark_session_terminal = mark_session_terminal

    def reset_session(self, session_id: UUID) -> None:
        self._clear_session_runtime(session_id)

    def mark_session_terminal(self, session_id: UUID) -> None:
        self._mark_session_terminal(session_id)

    async def emit_interim_state(self, event: InterimStateEvent) -> None:
        if not self._overlay_events_enabled:
            return
        message = InterimStateMessage(
            session_id=event.session_id,
            seq=self._next_overlay_seq(event.session_id),
            state=event.state,
        )
        await self._send_overlay(event.session_id, message.model_dump(mode="json"))

    async def emit_interim_text(self, event: InterimTextEvent) -> None:
        if not self._overlay_events_enabled:
            return
        normalized = " ".join(event.text.split()).strip()
        if not normalized:
            return
        if self._last_interim_text(event.session_id) == normalized:
            return
        message = InterimTextMessage(
            session_id=event.session_id,
            seq=self._next_overlay_seq(event.session_id),
            text=normalized,
        )
        if await self._send_overlay(event.session_id, message.model_dump(mode="json")):
            self._record_interim_text(event.session_id, normalized)

    async def emit_audio_level(self, event: AudioLevelEvent) -> None:
        if not self._overlay_events_enabled:
            return
        if not math.isfinite(event.rms):
            return
        level_db = 20.0 * math.log10(max(event.rms, 1e-6))
        if not math.isfinite(level_db):
            return
        message = AudioLevelMessage(session_id=event.session_id, level_db=level_db)
        await self._send_overlay(event.session_id, message.model_dump(mode="json"))

    async def emit_session_ended(self, event: SessionEndedEvent) -> None:
        if not self._overlay_events_enabled:
            self._clear_interim_text(event.session_id)
            return
        message = SessionEndedMessage(session_id=event.session_id, reason=event.reason)
        await self._send_overlay(
            event.session_id,
            message.model_dump(mode="json"),
            allow_terminal=True,
            next_state="ended",
        )
        self._clear_interim_text(event.session_id)

    async def _send_overlay(
        self,
        session_id: UUID,
        payload: dict[str, Any],
        *,
        allow_terminal: bool = False,
        next_state: str | None = None,
    ) -> bool:
        if not self._overlay_events_enabled:
            return False
        try:
            async with self._send_lock():
                current_state = self._overlay_session_state(session_id)
                if current_state == "ended" or (current_state == "terminal" and not allow_terminal):
                    self._increment_overlay_events_dropped()
                    logger.debug(
                        "Dropping overlay event {} for session {} after terminal transition",
                        payload.get("type"),
                        session_id,
                    )
                    return False
                await self._send_payload(payload)
            if next_state is not None:
                self._set_overlay_session_state(session_id, next_state)
            self._increment_overlay_events_emitted()
            return True
        except Exception as exc:  # noqa: BLE001
            self._increment_overlay_events_dropped()
            logger.debug("Dropping overlay event after send failure: {}", exc)
            return False


class WebSocketEventSinkState:
    """Own overlay wire sequencing and counters for WebSocket event sinks."""

    def __init__(self, *, overlay_events_enabled: bool) -> None:
        self.overlay_events_enabled = overlay_events_enabled
        self._overlay_event_seq_by_session: dict[UUID, int] = {}
        self._overlay_last_interim_text_by_session: dict[UUID, str] = {}
        self._overlay_state_by_session: dict[UUID, str] = {}
        self._overlay_events_emitted = 0
        self._overlay_events_dropped = 0

    @property
    def overlay_events_emitted(self) -> int:
        return self._overlay_events_emitted

    @property
    def overlay_events_dropped(self) -> int:
        return self._overlay_events_dropped

    def sink(
        self,
        *,
        websocket: JsonWebSocket,
        send_lock: Callable[[], asyncio.Lock],
    ) -> WebSocketEventSink:
        return WebSocketEventSink(
            websocket=websocket,
            send_lock=send_lock,
            overlay_adapter=OverlayEventAdapter(
                overlay_events_enabled=self.overlay_events_enabled,
                send_payload=websocket.send_json,
                send_lock=send_lock,
                next_overlay_seq=self._next_overlay_seq,
                overlay_session_state=self._overlay_session_state,
                set_overlay_session_state=self._set_overlay_session_state,
                last_interim_text=self._overlay_last_interim_text,
                record_interim_text=self._record_overlay_interim_text,
                clear_interim_text=self._clear_overlay_interim_text,
                increment_overlay_events_emitted=self._increment_overlay_events_emitted,
                increment_overlay_events_dropped=self._increment_overlay_events_dropped,
                clear_session_runtime=self._clear_overlay_session_runtime,
                mark_session_terminal=self._mark_overlay_session_terminal,
            ),
        )

    def _next_overlay_seq(self, session_id: UUID) -> int:
        current = self._overlay_event_seq_by_session.get(session_id, 0)
        self._overlay_event_seq_by_session[session_id] = current + 1
        return current

    def _overlay_session_state(self, session_id: UUID) -> str | None:
        return self._overlay_state_by_session.get(session_id)

    def _set_overlay_session_state(self, session_id: UUID, state: str) -> None:
        self._overlay_state_by_session.pop(session_id, None)
        self._overlay_state_by_session[session_id] = state
        while len(self._overlay_state_by_session) > OVERLAY_SESSION_STATE_CACHE_LIMIT:
            oldest = next(iter(self._overlay_state_by_session))
            self._overlay_state_by_session.pop(oldest)

    def _clear_overlay_session_runtime(self, session_id: UUID) -> None:
        self._overlay_event_seq_by_session.pop(session_id, None)
        self._overlay_last_interim_text_by_session.pop(session_id, None)
        self._set_overlay_session_state(session_id, "active")

    def _mark_overlay_session_terminal(self, session_id: UUID) -> None:
        self._set_overlay_session_state(session_id, "terminal")

    def _overlay_last_interim_text(self, session_id: UUID) -> str | None:
        return self._overlay_last_interim_text_by_session.get(session_id)

    def _record_overlay_interim_text(self, session_id: UUID, text: str) -> None:
        self._overlay_last_interim_text_by_session[session_id] = text

    def _clear_overlay_interim_text(self, session_id: UUID) -> None:
        self._overlay_last_interim_text_by_session.pop(session_id, None)

    def _increment_overlay_events_emitted(self) -> None:
        self._overlay_events_emitted += 1

    def _increment_overlay_events_dropped(self) -> None:
        self._overlay_events_dropped += 1


class WebSocketEventSink:
    """Serialize Session events to the existing WebSocket protocol."""

    def __init__(
        self,
        *,
        websocket: JsonWebSocket,
        send_lock: Callable[[], asyncio.Lock],
        overlay_adapter: OverlayEventAdapter,
    ) -> None:
        self._websocket = websocket
        self._send_lock = send_lock
        self._overlay_adapter = overlay_adapter

    async def emit(self, event: SessionEvent) -> None:
        if isinstance(event, SessionStartedEvent):
            await self._emit_session_started(event)
        elif isinstance(event, InterimStateEvent):
            await self._emit_interim_state(event)
        elif isinstance(event, InterimTextEvent):
            await self._emit_interim_text(event)
        elif isinstance(event, AudioLevelEvent):
            await self._emit_audio_level(event)
        elif isinstance(event, FinalResultEvent):
            await self._emit_final_result(event)
        elif isinstance(event, SessionEndedEvent):
            await self._emit_session_ended(event)
        elif isinstance(event, SessionWarningEvent):
            await self._emit_session_warning(event)
        elif isinstance(event, SessionErrorEvent):
            await self._emit_session_error(event)

    async def _send_direct(self, payload: dict[str, Any]) -> None:
        try:
            async with self._send_lock():
                await self._websocket.send_json(payload)
        except Exception as exc:  # noqa: BLE001
            if exc.__class__.__name__ == "WebSocketDisconnect":
                raise EventSinkClosed("event sink transport disconnected") from exc
            raise

    async def _emit_session_started(self, event: SessionStartedEvent) -> None:
        self._overlay_adapter.reset_session(event.session_id)
        message = SessionStarted(
            session_id=event.session_id,
            ts=event.ts,
            mic_device=event.mic_device,
            lang=event.lang,
        )
        await self._send_direct(message.model_dump(mode="json"))

    async def _emit_interim_state(self, event: InterimStateEvent) -> None:
        await self._overlay_adapter.emit_interim_state(event)

    async def _emit_interim_text(self, event: InterimTextEvent) -> None:
        await self._overlay_adapter.emit_interim_text(event)

    async def _emit_audio_level(self, event: AudioLevelEvent) -> None:
        await self._overlay_adapter.emit_audio_level(event)

    async def _emit_final_result(self, event: FinalResultEvent) -> None:
        self._overlay_adapter.mark_session_terminal(event.session_id)
        message = FinalResult(
            session_id=event.session_id,
            text=event.text,
            latency_ms=event.latency_ms,
            audio_ms=event.audio_ms,
            lang=event.lang,
            confidence=event.confidence,
        )
        await self._send_direct(message.model_dump(mode="json"))

    async def _emit_session_ended(self, event: SessionEndedEvent) -> None:
        await self._overlay_adapter.emit_session_ended(event)

    async def _emit_session_warning(self, event: SessionWarningEvent) -> None:
        logger.info(
            "Session {} warning: {:.0f}s remaining of {:.0f}s limit",
            event.session_id,
            event.remaining_seconds,
            event.limit_seconds,
        )
        message = SessionWarningMessage(
            session_id=event.session_id,
            warning=event.warning,
            remaining_seconds=round(event.remaining_seconds, 1),
            limit_seconds=event.limit_seconds,
        )
        try:
            await self._send_direct(message.model_dump(mode="json"))
        except Exception as exc:  # noqa: BLE001
            logger.debug("Failed to send session warning for {}: {}", event.session_id, exc)

    async def _emit_session_error(self, event: SessionErrorEvent) -> None:
        message = ErrorMessage(
            session_id=event.session_id,
            code=event.code,
            message=event.message,
        )
        await self._send_direct(message.model_dump(mode="json"))


__all__ = [
    "AudioLevelEvent",
    "ErrorCode",
    "EventSinkClosed",
    "EventSink",
    "FinalResultEvent",
    "InterimStateEvent",
    "InterimTextEvent",
    "OverlayEventAdapter",
    "RecordingEventSink",
    "SessionEndedEvent",
    "SessionErrorEvent",
    "SessionEvent",
    "SessionStartedEvent",
    "SessionWarningEvent",
    "WebSocketEventSink",
    "WebSocketEventSinkState",
]
