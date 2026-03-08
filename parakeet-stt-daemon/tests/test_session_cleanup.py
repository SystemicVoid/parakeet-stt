"""Lifecycle cleanup invariants for disconnect/error handling."""

from __future__ import annotations

import asyncio
from datetime import UTC, datetime
from typing import Any, cast
from uuid import UUID, uuid4

import numpy as np
from fastapi import WebSocketDisconnect
from parakeet_stt_daemon.audio import AudioInput
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

    def __init__(self) -> None:
        self.abort_calls = 0
        self.start_calls = 0
        self.raise_on_start = False
        self._session_limit_exceeded = False

    def start_session(self) -> None:
        self.start_calls += 1
        if self.raise_on_start:
            raise RuntimeError("audio start failed")

    def abort_session(self) -> None:
        self.abort_calls += 1

    def take_stream_chunks(self) -> list[object]:
        return []

    def session_limit_exceeded(self) -> bool:
        return self._session_limit_exceeded


class DummyDrainTask:
    def __init__(self) -> None:
        self.cancel_called = False

    def done(self) -> bool:
        return False

    def cancel(self) -> None:
        self.cancel_called = True


class FakeStreamingTranscriber:
    def __init__(self, raise_on_start: bool = False, *, helper_active: bool = True) -> None:
        self.raise_on_start = raise_on_start
        self.helper_active = helper_active
        self.fallback_reason: str | None = None if helper_active else "init_failed:ImportError"

    def start_session(self, _sample_rate: int) -> object:
        if self.raise_on_start:
            raise RuntimeError("stream start failed")
        return object()


class FakeWebSocket:
    def __init__(self, incoming: list[dict | Exception]) -> None:
        self._incoming = incoming
        self._idx = 0
        self.headers: dict[str, str] = {}
        self.client = "test-client"
        self.sent_json: list[dict] = []
        self.accepted = False

    async def accept(self) -> None:
        self.accepted = True

    async def close(self, code: int) -> None:
        self.sent_json.append({"type": "closed", "code": code})

    async def receive_json(self) -> dict:
        if self._idx >= len(self._incoming):
            raise RuntimeError("receive_json called without queued message")
        event = self._incoming[self._idx]
        self._idx += 1
        if isinstance(event, Exception):
            raise event
        return event

    async def send_json(self, payload: dict) -> None:
        self.sent_json.append(payload)


def _start_message(session_id: UUID) -> StartSession:
    return StartSession(
        type=ClientMessageType.START_SESSION,
        session_id=session_id,
        timestamp=datetime.now(tz=UTC),
    )


def _start_event(session_id: UUID) -> dict[str, object]:
    return _start_message(session_id).model_dump(mode="json")


def _build_server() -> DaemonServer:
    server = cast(Any, DaemonServer.__new__(DaemonServer))
    server.settings = ServerSettings(device="cpu", status_enabled=True, streaming_enabled=False)
    server.sessions = SessionManager()
    server.audio = FakeAudio()
    server.model = object()
    server.transcriber = object()
    server._transcribe_lock = asyncio.Lock()
    server.streaming_transcriber = None
    server._active_stream = object()
    server._stream_drain_task = None
    server._stream_drain_running = False
    server._session_guard_task = None
    server._session_guard_running = False
    server._session_sample_limit = 1_440_000
    server._session_age_limit_ms = 90_000
    server._requested_device = "cpu"
    server._effective_device = "cpu"
    server._last_audio_ms = None
    server._last_audio_stop_ms = None
    server._last_finalize_ms = None
    server._last_infer_ms = None
    server._last_send_ms = None
    server._live_interim_chunks = []
    server._live_interim_failed = False
    server._overlay_event_seq_by_session = {}
    server._overlay_last_interim_text_by_session = {}
    server._overlay_events_emitted = 0
    server._overlay_events_dropped = 0
    return cast(DaemonServer, server)


def test_audio_input_enforces_session_sample_limit() -> None:
    audio = AudioInput(sample_rate=16_000, channels=1, max_session_samples=5)
    audio.start_session()

    chunk = np.array([[0.1], [0.2], [0.3], [0.4]], dtype=np.float32)
    audio._callback(chunk, frames=4, time=None, status=cast(Any, 0))
    audio._callback(chunk, frames=4, time=None, status=cast(Any, 0))

    assert audio.session_limit_exceeded() is True

    captured = audio.stop_session()

    assert captured.size == 5
    assert np.allclose(captured, np.array([0.1, 0.2, 0.3, 0.4, 0.1], dtype=np.float32))


def test_disconnect_cleans_active_session_state() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.audio)
        session_id = uuid4()
        await server.sessions.start_session(session_id)

        drain_task = DummyDrainTask()
        server._stream_drain_task = cast(Any, drain_task)
        server._stream_drain_running = True

        websocket = FakeWebSocket([WebSocketDisconnect()])
        await server.handle_websocket(cast(Any, websocket))

        assert server.sessions.active is None
        assert audio.abort_calls == 1
        assert server._active_stream is None
        assert server._stream_drain_task is None
        assert drain_task.cancel_called is True

    asyncio.run(scenario())


def test_handler_exception_cleans_active_session_state() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.audio)
        session_id = uuid4()
        await server.sessions.start_session(session_id)

        async def explode_dispatch(*_args, **_kwargs) -> None:
            raise RuntimeError("boom")

        server._dispatch = explode_dispatch  # type: ignore[method-assign]
        websocket = FakeWebSocket(
            [
                {
                    "type": "start_session",
                    "session_id": str(uuid4()),
                    "timestamp": datetime.now(tz=UTC).isoformat(),
                }
            ]
        )
        await server.handle_websocket(cast(Any, websocket))

        assert server.sessions.active is None
        assert audio.abort_calls == 1
        assert server._active_stream is None
        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "UNEXPECTED"

    asyncio.run(scenario())


def test_abort_only_cleans_matching_session() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.audio)
        active_session_id = uuid4()
        await server.sessions.start_session(active_session_id)

        message = AbortSession(
            type=ClientMessageType.ABORT_SESSION,
            session_id=uuid4(),
            reason="user",
            timestamp=datetime.now(tz=UTC),
        )
        websocket = FakeWebSocket([])
        await server._handle_abort(cast(Any, websocket), message)

        assert server.sessions.active is not None
        assert server.sessions.active.session_id == active_session_id
        assert audio.abort_calls == 0
        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "SESSION_NOT_FOUND"

    asyncio.run(scenario())


def test_stop_session_missing_id_returns_not_found() -> None:
    async def scenario() -> None:
        server = _build_server()
        session_id = uuid4()
        websocket = FakeWebSocket([])

        message = StopSession(
            type=ClientMessageType.STOP_SESSION,
            session_id=session_id,
            timestamp=datetime.now(tz=UTC),
        )
        await server._handle_stop(cast(Any, websocket), message)

        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "SESSION_NOT_FOUND"

    asyncio.run(scenario())


def test_abort_session_returns_aborted_on_match() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.audio)
        session_id = uuid4()
        await server.sessions.start_session(session_id)

        message = AbortSession(
            type=ClientMessageType.ABORT_SESSION,
            session_id=session_id,
            reason="user",
            timestamp=datetime.now(tz=UTC),
        )
        websocket = FakeWebSocket([])
        await server._handle_abort(cast(Any, websocket), message)

        assert server.sessions.active is None
        assert audio.abort_calls == 1
        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "SESSION_ABORTED"

    asyncio.run(scenario())


def test_invalid_request_errors_on_parse_failure() -> None:
    async def scenario() -> None:
        server = _build_server()
        websocket = FakeWebSocket([{"type": "bogus"}, WebSocketDisconnect()])

        await server.handle_websocket(cast(Any, websocket))

        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "INVALID_REQUEST"

    asyncio.run(scenario())


def test_start_session_rolls_back_when_audio_start_fails() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.audio)
        audio.raise_on_start = True
        session_id = uuid4()

        websocket = FakeWebSocket([])
        await server._handle_start(cast(Any, websocket), _start_message(session_id))

        assert server.sessions.active is None
        assert audio.start_calls == 1
        assert audio.abort_calls == 1
        assert server._active_stream is None
        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "UNEXPECTED"

    asyncio.run(scenario())


def test_start_session_rolls_back_when_stream_start_fails() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.audio)
        server.streaming_transcriber = cast(Any, FakeStreamingTranscriber(raise_on_start=True))
        session_id = uuid4()

        websocket = FakeWebSocket([])
        await server._handle_start(cast(Any, websocket), _start_message(session_id))

        assert server.sessions.active is None
        assert audio.abort_calls == 1
        assert server._active_stream is None
        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "UNEXPECTED"

    asyncio.run(scenario())


def test_start_session_rolls_back_when_drain_loop_start_fails() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.audio)
        server.streaming_transcriber = cast(Any, FakeStreamingTranscriber())
        session_id = uuid4()

        def explode_start_stream_drain_loop(_websocket: Any, _session_id: UUID) -> None:
            raise RuntimeError("drain loop failed")

        server._start_stream_drain_loop = explode_start_stream_drain_loop  # type: ignore[method-assign]
        websocket = FakeWebSocket([])
        await server._handle_start(cast(Any, websocket), _start_message(session_id))

        assert server.sessions.active is None
        assert audio.abort_calls == 1
        assert server._active_stream is None
        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "UNEXPECTED"

    asyncio.run(scenario())


def test_start_session_rolls_back_when_session_started_send_fails() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.audio)
        session_id = uuid4()
        websocket = FakeWebSocket([])

        send_attempts = 0

        async def fail_first_send(payload: dict) -> None:
            nonlocal send_attempts
            send_attempts += 1
            if send_attempts == 1:
                raise RuntimeError("send failed")
            websocket.sent_json.append(payload)

        websocket.send_json = fail_first_send  # type: ignore[method-assign]
        await server._handle_start(cast(Any, websocket), _start_message(session_id))

        assert server.sessions.active is None
        assert audio.abort_calls == 1
        assert server._active_stream is None
        assert send_attempts == 2
        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "UNEXPECTED"

    asyncio.run(scenario())


def test_start_session_streaming_send_failure_stops_drain_loop() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.audio)
        server.streaming_transcriber = cast(Any, FakeStreamingTranscriber())
        session_id = uuid4()
        websocket = FakeWebSocket([])

        send_attempts = 0

        async def fail_first_send(payload: dict) -> None:
            nonlocal send_attempts
            send_attempts += 1
            if send_attempts == 1:
                raise RuntimeError("send failed")
            websocket.sent_json.append(payload)

        websocket.send_json = fail_first_send  # type: ignore[method-assign]
        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        await asyncio.sleep(0)

        assert server.sessions.active is None
        assert audio.abort_calls == 1
        assert server._active_stream is None
        assert server._stream_drain_task is None
        assert server._stream_drain_running is False
        assert send_attempts == 2
        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "UNEXPECTED"

    asyncio.run(scenario())


def test_start_session_disconnect_rolls_back_and_bubbles_disconnect() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.audio)
        session_id = uuid4()
        websocket = FakeWebSocket([])

        async def raise_disconnect(_payload: dict) -> None:
            raise WebSocketDisconnect()

        websocket.send_json = raise_disconnect  # type: ignore[method-assign]
        try:
            await server._handle_start(cast(Any, websocket), _start_message(session_id))
        except WebSocketDisconnect:
            pass
        else:
            raise AssertionError("expected WebSocketDisconnect")

        assert server.sessions.active is None
        assert audio.abort_calls == 1
        assert server._active_stream is None
        assert not websocket.sent_json

    asyncio.run(scenario())


def test_handle_websocket_disconnect_during_start_does_not_cleanup_new_session() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.audio)
        start_session_id = uuid4()
        replacement_session_id = uuid4()
        websocket = FakeWebSocket([_start_event(start_session_id)])

        async def raise_disconnect(_payload: dict) -> None:
            raise WebSocketDisconnect()

        websocket.send_json = raise_disconnect  # type: ignore[method-assign]
        original_cleanup = server._cleanup_active_session
        cleanup_calls = 0

        async def cleanup_with_interleaving(
            reason: str,
            expected_session_id: UUID | None = None,
            *,
            require_session_match: bool = False,
        ) -> bool:
            nonlocal cleanup_calls
            cleanup_calls += 1
            if cleanup_calls == 2:
                await server.sessions.start_session(replacement_session_id)
            return await original_cleanup(
                reason,
                expected_session_id=expected_session_id,
                require_session_match=require_session_match,
            )

        server._cleanup_active_session = cleanup_with_interleaving  # type: ignore[method-assign]
        await server.handle_websocket(cast(Any, websocket))

        assert cleanup_calls == 2
        assert audio.abort_calls == 1
        assert server.sessions.active is not None
        assert server.sessions.active.session_id == replacement_session_id

    asyncio.run(scenario())


def test_status_reports_runtime_truth_and_last_timings() -> None:
    async def scenario() -> None:
        server = _build_server()
        server.settings = ServerSettings(device="cuda", status_enabled=True, streaming_enabled=True)
        server._requested_device = "cuda"
        server._effective_device = "cpu"
        server.streaming_transcriber = cast(Any, FakeStreamingTranscriber(helper_active=False))
        server._last_audio_ms = 1200
        server._last_audio_stop_ms = 9
        server._last_finalize_ms = 120
        server._last_infer_ms = 85
        server._last_send_ms = 4
        session_id = uuid4()
        await server.sessions.start_session(session_id)

        status = server.status()

        assert status.device == "cuda"
        assert status.effective_device == "cpu"
        assert status.streaming_enabled is True
        assert status.stream_helper_active is False
        assert status.stream_fallback_reason == "init_failed:ImportError"
        assert status.audio_stop_ms == 9
        assert status.finalize_ms == 120
        assert status.infer_ms == 85
        assert status.send_ms == 4
        assert status.last_audio_ms == 1200
        assert status.last_infer_ms == 85
        assert status.last_send_ms == 4
        assert status.active_session_age_ms is not None
        assert status.active_session_age_ms >= 0

    asyncio.run(scenario())


def test_session_guard_aborts_when_audio_sample_limit_exceeded() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.audio)
        websocket = FakeWebSocket([])
        session_id = uuid4()

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        audio._session_limit_exceeded = True
        await asyncio.sleep(0.15)

        assert server.sessions.active is None
        assert audio.abort_calls == 1
        error_payloads = [
            payload for payload in websocket.sent_json if payload.get("type") == "error"
        ]
        assert error_payloads
        assert error_payloads[-1]["code"] == "AUDIO_DEVICE"
        assert "max buffered audio" in cast(str, error_payloads[-1]["message"])

    asyncio.run(scenario())


def test_session_guard_aborts_when_duration_limit_exceeded() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.audio)
        server._session_age_limit_ms = 0
        websocket = FakeWebSocket([])
        session_id = uuid4()

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        await asyncio.sleep(0.15)

        assert server.sessions.active is None
        assert audio.abort_calls == 1
        error_payloads = [
            payload for payload in websocket.sent_json if payload.get("type") == "error"
        ]
        assert error_payloads
        assert error_payloads[-1]["code"] == "AUDIO_DEVICE"
        assert "max duration" in cast(str, error_payloads[-1]["message"])

    asyncio.run(scenario())
