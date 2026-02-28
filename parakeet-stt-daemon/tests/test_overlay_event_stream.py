"""Overlay event stream invariants for Phase 2 emission wiring."""

from __future__ import annotations

import asyncio
from datetime import UTC, datetime
from typing import Any, cast
from uuid import UUID, uuid4

import numpy as np
from parakeet_stt_daemon.config import ServerSettings
from parakeet_stt_daemon.messages import (
    AbortSession,
    ClientMessageType,
    StartSession,
    StopSession,
)
from parakeet_stt_daemon.server import DaemonServer
from parakeet_stt_daemon.session import SessionManager


class FakeAudio:
    sample_rate = 16_000

    def start_session(self) -> None:
        return None

    def stop_session_with_streaming(self) -> tuple[np.ndarray, list[np.ndarray], np.ndarray]:
        samples = np.ones((1600,), dtype=np.float32)
        return samples, [], np.zeros((0,), dtype=np.float32)

    def abort_session(self) -> None:
        return None

    def take_stream_chunks(self) -> list[np.ndarray]:
        return []


class FakeWebSocket:
    def __init__(self) -> None:
        self.sent_json: list[dict[str, object]] = []

    async def send_json(self, payload: dict[str, object]) -> None:
        self.sent_json.append(payload)


def _build_server(*, overlay_events_enabled: bool) -> DaemonServer:
    server = cast(Any, DaemonServer.__new__(DaemonServer))
    server.settings = ServerSettings(
        device="cpu",
        status_enabled=True,
        streaming_enabled=False,
        overlay_events_enabled=overlay_events_enabled,
    )
    server.sessions = SessionManager()
    server.audio = FakeAudio()
    server.model = object()
    server.transcriber = object()
    server._transcribe_lock = asyncio.Lock()
    server.streaming_transcriber = None
    server._active_stream = None
    server._stream_drain_task = None
    server._stream_drain_running = False
    server._requested_device = "cpu"
    server._effective_device = "cpu"
    server._last_audio_ms = None
    server._last_audio_stop_ms = None
    server._last_finalize_ms = None
    server._last_infer_ms = None
    server._last_send_ms = None
    server._overlay_event_seq_by_session = {}
    server._overlay_events_emitted = 0
    server._overlay_events_dropped = 0

    async def fake_finalize(
        _audio_samples: np.ndarray,
        _ready_chunks: list[np.ndarray],
        _tail: np.ndarray,
    ) -> tuple[str, int]:
        return "overlay test text", 7

    server._finalise_transcription = fake_finalize
    return cast(DaemonServer, server)


def _start_message(session_id: UUID) -> StartSession:
    return StartSession(
        type=ClientMessageType.START_SESSION,
        session_id=session_id,
        timestamp=datetime.now(tz=UTC),
    )


def _stop_message(session_id: UUID) -> StopSession:
    return StopSession(
        type=ClientMessageType.STOP_SESSION,
        session_id=session_id,
        timestamp=datetime.now(tz=UTC),
    )


def test_overlay_events_disabled_emits_only_baseline_messages(monkeypatch) -> None:
    async def scenario() -> None:
        async def no_sleep(_seconds: float) -> None:
            return None

        monkeypatch.setattr("parakeet_stt_daemon.server.asyncio.sleep", no_sleep)

        server = _build_server(overlay_events_enabled=False)
        websocket = FakeWebSocket()
        session_id = uuid4()

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        await server._handle_stop(cast(Any, websocket), _stop_message(session_id))

        sent_types = [cast(str, payload["type"]) for payload in websocket.sent_json]
        assert sent_types == ["session_started", "final_result"]
        assert sent_types.count("final_result") == 1

    asyncio.run(scenario())


def test_overlay_events_enabled_stream_is_ordered_and_final_once(monkeypatch) -> None:
    async def scenario() -> None:
        async def no_sleep(_seconds: float) -> None:
            return None

        monkeypatch.setattr("parakeet_stt_daemon.server.asyncio.sleep", no_sleep)

        server = _build_server(overlay_events_enabled=True)
        websocket = FakeWebSocket()
        session_id = uuid4()

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        await server._handle_stop(cast(Any, websocket), _stop_message(session_id))

        sent_types = [cast(str, payload["type"]) for payload in websocket.sent_json]
        assert sent_types == [
            "session_started",
            "interim_state",
            "interim_state",
            "interim_state",
            "final_result",
            "session_ended",
        ]
        assert sent_types.count("final_result") == 1

        interim_payloads = [
            payload for payload in websocket.sent_json if payload["type"] == "interim_state"
        ]
        assert [payload["state"] for payload in interim_payloads] == [
            "listening",
            "processing",
            "finalizing",
        ]
        assert [payload["seq"] for payload in interim_payloads] == [0, 1, 2]

        session_ended = websocket.sent_json[-1]
        assert session_ended["type"] == "session_ended"
        assert session_ended["reason"] == "final"

        status = server.status()
        assert status.overlay_events_enabled is True
        assert status.overlay_events_emitted == 4
        assert status.overlay_events_dropped == 0

    asyncio.run(scenario())


def test_overlay_sequence_does_not_leak_across_sessions(monkeypatch) -> None:
    async def scenario() -> None:
        async def no_sleep(_seconds: float) -> None:
            return None

        monkeypatch.setattr("parakeet_stt_daemon.server.asyncio.sleep", no_sleep)

        server = _build_server(overlay_events_enabled=True)
        websocket = FakeWebSocket()
        first = uuid4()
        second = uuid4()

        await server._handle_start(cast(Any, websocket), _start_message(first))
        await server._handle_stop(cast(Any, websocket), _stop_message(first))
        await server._handle_start(cast(Any, websocket), _start_message(second))
        await server._handle_stop(cast(Any, websocket), _stop_message(second))

        interim_by_session: dict[str, list[int]] = {}
        for payload in websocket.sent_json:
            if payload.get("type") != "interim_state":
                continue
            key = cast(str, payload["session_id"])
            interim_by_session.setdefault(key, []).append(cast(int, payload["seq"]))

        assert interim_by_session[str(first)] == [0, 1, 2]
        assert interim_by_session[str(second)] == [0, 1, 2]
        assert [payload["type"] for payload in websocket.sent_json].count("final_result") == 2

    asyncio.run(scenario())


def test_overlay_send_failures_do_not_block_final_result(monkeypatch) -> None:
    async def scenario() -> None:
        async def no_sleep(_seconds: float) -> None:
            return None

        monkeypatch.setattr("parakeet_stt_daemon.server.asyncio.sleep", no_sleep)

        server = _build_server(overlay_events_enabled=True)
        websocket = FakeWebSocket()
        session_id = uuid4()

        async def send_json(payload: dict[str, object]) -> None:
            if payload.get("type") in {"interim_state", "session_ended"}:
                raise RuntimeError("overlay sink failed")
            websocket.sent_json.append(payload)

        websocket.send_json = send_json  # type: ignore[method-assign]

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        await server._handle_stop(cast(Any, websocket), _stop_message(session_id))

        sent_types = [cast(str, payload["type"]) for payload in websocket.sent_json]
        assert sent_types == ["session_started", "final_result"]

        status = server.status()
        assert status.overlay_events_emitted == 0
        assert status.overlay_events_dropped == 4

    asyncio.run(scenario())


def test_abort_emits_session_ended_and_no_final_result() -> None:
    async def scenario() -> None:
        server = _build_server(overlay_events_enabled=True)
        websocket = FakeWebSocket()
        session_id = uuid4()

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        await server._handle_abort(
            cast(Any, websocket),
            AbortSession(
                type=ClientMessageType.ABORT_SESSION,
                session_id=session_id,
                reason="user",
                timestamp=datetime.now(tz=UTC),
            ),
        )

        sent_types = [cast(str, payload["type"]) for payload in websocket.sent_json]
        assert "final_result" not in sent_types
        assert "session_ended" in sent_types
        session_ended = next(
            payload for payload in websocket.sent_json if payload["type"] == "session_ended"
        )
        assert session_ended["reason"] == "abort"

    asyncio.run(scenario())
