"""Session event seam tests."""

from __future__ import annotations

import asyncio
from datetime import UTC, datetime
from typing import Any
from uuid import UUID, uuid4

from parakeet_stt_daemon.events import (
    AudioLevelEvent,
    FinalResultEvent,
    InterimStateEvent,
    InterimTextEvent,
    RecordingEventSink,
    SessionEndedEvent,
    SessionStartedEvent,
    WebSocketEventSink,
)
from parakeet_stt_daemon.messages import InterimStateValue, SessionEndReason


class FakeWebSocket:
    def __init__(self) -> None:
        self.sent_json: list[dict[str, Any]] = []

    async def send_json(self, data: dict[str, Any]) -> None:
        self.sent_json.append(data)


def test_recording_event_sink_preserves_session_flow_order() -> None:
    async def scenario() -> None:
        sink = RecordingEventSink()
        session_id = uuid4()

        await sink.emit(
            SessionStartedEvent(
                session_id=session_id,
                ts=datetime.now(tz=UTC),
                mic_device="test mic",
                lang="auto",
            )
        )
        await sink.emit(
            InterimTextEvent(
                session_id=session_id,
                text="partial text",
            )
        )
        await sink.emit(
            FinalResultEvent(
                session_id=session_id,
                text="final text",
                latency_ms=10,
                audio_ms=100,
                lang="auto",
                confidence=None,
                tail_trim_mode="rms",
                vad_active=False,
                vad_fallback_reason=None,
            )
        )
        await sink.emit(SessionEndedEvent(session_id=session_id, reason=SessionEndReason.FINAL))

        assert [type(event).__name__ for event in sink.events] == [
            "SessionStartedEvent",
            "InterimTextEvent",
            "FinalResultEvent",
            "SessionEndedEvent",
        ]

    asyncio.run(scenario())


def test_websocket_event_sink_serializes_existing_wire_messages() -> None:
    async def scenario() -> None:
        websocket = FakeWebSocket()
        send_lock = asyncio.Lock()
        overlay_seq_by_session: dict[UUID, int] = {}
        overlay_state_by_session: dict[UUID, str] = {}
        last_interim_text_by_session: dict[UUID, str] = {}
        counters = {"emitted": 0, "dropped": 0}

        def next_overlay_seq(session_id: UUID) -> int:
            current = overlay_seq_by_session.get(session_id, 0)
            overlay_seq_by_session[session_id] = current + 1
            return current

        def overlay_session_state(session_id: UUID) -> str | None:
            return overlay_state_by_session.get(session_id)

        def set_overlay_session_state(session_id: UUID, state: str) -> None:
            overlay_state_by_session[session_id] = state

        def last_interim_text(session_id: UUID) -> str | None:
            return last_interim_text_by_session.get(session_id)

        def record_interim_text(session_id: UUID, text: str) -> None:
            last_interim_text_by_session[session_id] = text

        def clear_interim_text(session_id: UUID) -> None:
            last_interim_text_by_session.pop(session_id, None)

        def increment_emitted() -> None:
            counters["emitted"] += 1

        def increment_dropped() -> None:
            counters["dropped"] += 1

        sink = WebSocketEventSink(
            websocket=websocket,
            send_lock=lambda: send_lock,
            overlay_events_enabled=True,
            next_overlay_seq=next_overlay_seq,
            overlay_session_state=overlay_session_state,
            set_overlay_session_state=set_overlay_session_state,
            last_interim_text=last_interim_text,
            record_interim_text=record_interim_text,
            clear_interim_text=clear_interim_text,
            increment_overlay_events_emitted=increment_emitted,
            increment_overlay_events_dropped=increment_dropped,
        )
        session_id = uuid4()

        await sink.emit(
            SessionStartedEvent(
                session_id=session_id,
                ts=datetime(2026, 5, 18, tzinfo=UTC),
                mic_device=None,
                lang="auto",
            )
        )
        await sink.emit(InterimStateEvent(session_id=session_id, state=InterimStateValue.LISTENING))
        await sink.emit(InterimTextEvent(session_id=session_id, text=" hello   world "))
        await sink.emit(InterimTextEvent(session_id=session_id, text="hello world"))
        await sink.emit(AudioLevelEvent(session_id=session_id, rms=0.1))
        set_overlay_session_state(session_id, "terminal")
        await sink.emit(
            FinalResultEvent(
                session_id=session_id,
                text="final text",
                latency_ms=12,
                audio_ms=345,
                lang="auto",
                confidence=None,
                tail_trim_mode="vad",
                vad_active=True,
                vad_fallback_reason=None,
            )
        )
        await sink.emit(SessionEndedEvent(session_id=session_id, reason=SessionEndReason.FINAL))

        sent_types = [payload["type"] for payload in websocket.sent_json]
        assert sent_types == [
            "session_started",
            "interim_state",
            "interim_text",
            "audio_level",
            "final_result",
            "session_ended",
        ]

        overlay_payloads = [
            payload
            for payload in websocket.sent_json
            if payload["type"] in {"interim_state", "interim_text"}
        ]
        assert [payload["seq"] for payload in overlay_payloads] == [0, 1]
        assert websocket.sent_json[2]["text"] == "hello world"
        assert websocket.sent_json[4] == {
            "type": "final_result",
            "session_id": str(session_id),
            "text": "final text",
            "latency_ms": 12,
            "audio_ms": 345,
            "lang": "auto",
            "confidence": None,
        }
        assert last_interim_text_by_session == {}
        assert counters == {"emitted": 4, "dropped": 0}

    asyncio.run(scenario())
