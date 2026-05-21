"""Lifecycle cleanup invariants for disconnect/error handling."""

from __future__ import annotations

import asyncio
from datetime import UTC, datetime, timedelta
from typing import Any, cast
from uuid import UUID, uuid4

import numpy as np
from fastapi import WebSocketDisconnect

from parakeet_stt_daemon.audio import AudioInput
from parakeet_stt_daemon.config import ServerSettings
from parakeet_stt_daemon.events import EventSink, EventSinkClosed, WebSocketEventSinkState
from parakeet_stt_daemon.messages import (
    AbortSession,
    ClientMessageType,
    ParsedMessage,
    SessionEndReason,
    StartSession,
    StopSession,
)
from parakeet_stt_daemon.server import DaemonServer
from parakeet_stt_daemon.session import Session, SessionManager
from parakeet_stt_daemon.session_orchestrator import SessionOrchestrator


class FakeAudio:
    sample_rate = 16_000

    def __init__(self) -> None:
        self.abort_calls = 0
        self.start_calls = 0
        self.stop_calls = 0
        self.raise_on_start = False
        self._session_limit_exceeded = False

    def start_session(self) -> None:
        self.start_calls += 1
        if self.raise_on_start:
            raise RuntimeError("audio start failed")

    def abort_session(self) -> None:
        self.abort_calls += 1

    def stop_session_with_streaming(self) -> tuple[np.ndarray, list[np.ndarray], np.ndarray]:
        self.stop_calls += 1
        samples = np.ones((1_600,), dtype=np.float32)
        return samples, [], np.zeros((0,), dtype=np.float32)

    def take_stream_chunks(self) -> list[object]:
        return []

    def take_audio_levels(self) -> list[float]:
        return []

    def session_limit_exceeded(self) -> bool:
        return self._session_limit_exceeded


class DummyDrainTask:
    def __init__(self) -> None:
        self.cancel_called = False
        self.awaited = False

    def done(self) -> bool:
        return False

    def cancel(self) -> None:
        self.cancel_called = True

    def __await__(self):
        async def _wait() -> None:
            self.awaited = True
            return None

        return _wait().__await__()


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


def _set_dynamic_attr(target: object, name: str, value: object) -> None:
    setattr(cast(Any, target), name, value)


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
    orchestrator = cast(Any, SessionOrchestrator.__new__(SessionOrchestrator))
    settings = ServerSettings(device="cpu", status_enabled=True, streaming_enabled=False)
    server.settings = settings
    server.orchestrator = orchestrator
    server.event_sinks = WebSocketEventSinkState(
        overlay_events_enabled=settings.overlay_events_enabled
    )
    server._websocket_send_locks = {}
    orchestrator.settings = settings
    orchestrator.sessions = SessionManager()
    orchestrator.audio = FakeAudio()
    orchestrator.model = object()
    orchestrator.transcriber = object()
    orchestrator._session_lock = asyncio.Lock()
    orchestrator._inference_lock = asyncio.Lock()
    orchestrator.streaming_transcriber = None
    orchestrator._active_stream = object()
    orchestrator._stream_drain_task = None
    orchestrator._stream_drain_running = False
    orchestrator._session_guard_task = None
    orchestrator._session_guard_running = False
    orchestrator._session_sample_limit = 1_440_000
    orchestrator._session_age_limit_ms = 90_000
    orchestrator._requested_device = "cpu"
    orchestrator._effective_device = "cpu"
    orchestrator._last_audio_ms = None
    orchestrator._last_audio_stop_ms = None
    orchestrator._last_finalize_ms = None
    orchestrator._last_infer_ms = None
    orchestrator._last_send_ms = None

    async def fake_collect_interim_text_updates(
        _session_id: UUID, _ready_chunks: list[object]
    ) -> list[str]:
        return []

    async def fake_finalize(_audio_samples: np.ndarray) -> tuple[str, int]:
        return "final text", 7

    orchestrator._collect_interim_text_updates = fake_collect_interim_text_updates
    orchestrator._finalise_transcription = fake_finalize
    return cast(DaemonServer, server)


async def _dispatch_message(
    server: DaemonServer,
    websocket: FakeWebSocket,
    message: StartSession | StopSession | AbortSession,
) -> None:
    await server._dispatch(
        cast(Any, websocket),
        ParsedMessage(kind=ClientMessageType(message.type), model=message),
    )


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


def test_audio_input_clips_pre_roll_to_session_sample_limit() -> None:
    audio = AudioInput(sample_rate=16_000, channels=1, max_session_samples=3)

    first = np.array([[0.1], [0.2]], dtype=np.float32)
    second = np.array([[0.3], [0.4]], dtype=np.float32)
    audio._callback(first, frames=2, time=None, status=cast(Any, 0))
    audio._callback(second, frames=2, time=None, status=cast(Any, 0))

    audio.start_session()

    assert audio.session_limit_exceeded() is False

    captured = audio.stop_session()

    assert captured.size == 3
    assert np.allclose(captured, np.array([0.2, 0.3, 0.4], dtype=np.float32))


def test_disconnect_cleans_active_session_state() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.orchestrator.audio)
        websocket = FakeWebSocket([WebSocketDisconnect()])
        session_id = uuid4()
        await server.orchestrator.sessions.start_session(session_id, owner_token=id(websocket))

        drain_task = DummyDrainTask()
        server.orchestrator._stream_drain_task = cast(Any, drain_task)
        server.orchestrator._stream_drain_running = True

        await server.handle_websocket(cast(Any, websocket))

        assert server.orchestrator.sessions.active is None
        assert audio.abort_calls == 1
        assert server.orchestrator._active_stream is None
        assert server.orchestrator._stream_drain_task is None
        assert drain_task.cancel_called is True
        assert drain_task.awaited is True

    asyncio.run(scenario())


def test_handler_exception_cleans_active_session_state() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.orchestrator.audio)
        websocket = FakeWebSocket(
            [
                {
                    "type": "start_session",
                    "session_id": str(uuid4()),
                    "timestamp": datetime.now(tz=UTC).isoformat(),
                }
            ]
        )
        session_id = uuid4()
        await server.orchestrator.sessions.start_session(session_id, owner_token=id(websocket))

        async def explode_dispatch(*_args, **_kwargs) -> None:
            raise RuntimeError("boom")

        _set_dynamic_attr(server, "_dispatch", explode_dispatch)
        await server.handle_websocket(cast(Any, websocket))

        assert server.orchestrator.sessions.active is None
        assert audio.abort_calls == 1
        assert server.orchestrator._active_stream is None
        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "UNEXPECTED"

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
        audio = cast(FakeAudio, server.orchestrator.audio)
        audio.raise_on_start = True
        session_id = uuid4()

        websocket = FakeWebSocket([])
        await _dispatch_message(server, websocket, _start_message(session_id))

        assert server.orchestrator.sessions.active is None
        assert audio.start_calls == 1
        assert audio.abort_calls == 1
        assert server.orchestrator._active_stream is None
        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "UNEXPECTED"

    asyncio.run(scenario())


def test_start_session_rolls_back_when_stream_start_fails() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.orchestrator.audio)
        server.orchestrator.streaming_transcriber = cast(
            Any, FakeStreamingTranscriber(raise_on_start=True)
        )
        session_id = uuid4()

        websocket = FakeWebSocket([])
        await _dispatch_message(server, websocket, _start_message(session_id))

        assert server.orchestrator.sessions.active is None
        assert audio.abort_calls == 1
        assert server.orchestrator._active_stream is None
        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "UNEXPECTED"

    asyncio.run(scenario())


def test_start_session_rolls_back_when_drain_loop_start_fails() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.orchestrator.audio)
        server.orchestrator.streaming_transcriber = cast(Any, FakeStreamingTranscriber())
        session_id = uuid4()

        def explode_start_stream_drain_loop(_websocket: Any, _session_id: UUID) -> None:
            raise RuntimeError("drain loop failed")

        _set_dynamic_attr(
            server.orchestrator, "_start_stream_drain_loop", explode_start_stream_drain_loop
        )
        websocket = FakeWebSocket([])
        await _dispatch_message(server, websocket, _start_message(session_id))

        assert server.orchestrator.sessions.active is None
        assert audio.abort_calls == 1
        assert server.orchestrator._active_stream is None
        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "UNEXPECTED"

    asyncio.run(scenario())


def test_start_session_rolls_back_when_session_started_send_fails() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.orchestrator.audio)
        session_id = uuid4()
        websocket = FakeWebSocket([])

        send_attempts = 0

        async def fail_first_send(payload: dict) -> None:
            nonlocal send_attempts
            send_attempts += 1
            if send_attempts == 1:
                raise RuntimeError("send failed")
            websocket.sent_json.append(payload)

        _set_dynamic_attr(websocket, "send_json", fail_first_send)
        await _dispatch_message(server, websocket, _start_message(session_id))

        assert server.orchestrator.sessions.active is None
        assert audio.abort_calls == 1
        assert server.orchestrator._active_stream is None
        assert send_attempts == 2
        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "UNEXPECTED"

    asyncio.run(scenario())


def test_start_session_streaming_send_failure_stops_drain_loop() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.orchestrator.audio)
        server.orchestrator.streaming_transcriber = cast(Any, FakeStreamingTranscriber())
        session_id = uuid4()
        websocket = FakeWebSocket([])

        send_attempts = 0

        async def fail_first_send(payload: dict) -> None:
            nonlocal send_attempts
            send_attempts += 1
            if send_attempts == 1:
                raise RuntimeError("send failed")
            websocket.sent_json.append(payload)

        _set_dynamic_attr(websocket, "send_json", fail_first_send)
        await _dispatch_message(server, websocket, _start_message(session_id))
        await asyncio.sleep(0)

        assert server.orchestrator.sessions.active is None
        assert audio.abort_calls == 1
        assert server.orchestrator._active_stream is None
        assert server.orchestrator._stream_drain_task is None
        assert server.orchestrator._stream_drain_running is False
        assert send_attempts == 2
        assert websocket.sent_json
        assert websocket.sent_json[-1]["type"] == "error"
        assert websocket.sent_json[-1]["code"] == "UNEXPECTED"

    asyncio.run(scenario())


def test_start_session_disconnect_rolls_back_and_bubbles_sink_closed() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.orchestrator.audio)
        session_id = uuid4()
        websocket = FakeWebSocket([])

        async def raise_disconnect(_payload: dict) -> None:
            raise WebSocketDisconnect()

        _set_dynamic_attr(websocket, "send_json", raise_disconnect)
        try:
            await _dispatch_message(server, websocket, _start_message(session_id))
        except EventSinkClosed:
            pass
        else:
            raise AssertionError("expected EventSinkClosed")

        assert server.orchestrator.sessions.active is None
        assert audio.abort_calls == 1
        assert server.orchestrator._active_stream is None
        assert not websocket.sent_json

    asyncio.run(scenario())


def test_handle_websocket_disconnect_during_start_does_not_cleanup_new_session() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.orchestrator.audio)
        start_session_id = uuid4()
        replacement_session_id = uuid4()
        websocket = FakeWebSocket([_start_event(start_session_id)])

        async def raise_disconnect(_payload: dict) -> None:
            raise WebSocketDisconnect()

        _set_dynamic_attr(websocket, "send_json", raise_disconnect)
        original_cleanup = server.orchestrator._cleanup_active_session
        cleanup_calls = 0

        async def cleanup_with_interleaving(
            reason: str,
            expected_session_id: UUID | None = None,
            expected_owner_token: int | None = None,
            *,
            require_session_match: bool = False,
            require_owner_match: bool = False,
            event_sink: EventSink | None = None,
            session_end_reason: SessionEndReason | None = None,
        ) -> bool:
            nonlocal cleanup_calls
            cleanup_calls += 1
            if cleanup_calls == 2:
                await server.orchestrator.sessions.start_session(
                    replacement_session_id, owner_token=-1
                )
            return await original_cleanup(
                reason,
                expected_session_id=expected_session_id,
                expected_owner_token=expected_owner_token,
                require_session_match=require_session_match,
                require_owner_match=require_owner_match,
                event_sink=event_sink,
                session_end_reason=session_end_reason,
            )

        _set_dynamic_attr(server.orchestrator, "_cleanup_active_session", cleanup_with_interleaving)
        await server.handle_websocket(cast(Any, websocket))

        assert cleanup_calls == 2
        assert audio.abort_calls == 1
        assert server.orchestrator.sessions.active is not None
        assert server.orchestrator.sessions.active.session_id == replacement_session_id

    asyncio.run(scenario())


def test_status_reports_runtime_truth_and_last_timings() -> None:
    async def scenario() -> None:
        server = _build_server()
        server.settings = ServerSettings(device="cuda", status_enabled=True, streaming_enabled=True)
        server.orchestrator.settings = server.settings
        server.orchestrator._requested_device = "cuda"
        server.orchestrator._effective_device = "cpu"
        server.orchestrator.streaming_transcriber = cast(
            Any, FakeStreamingTranscriber(helper_active=False)
        )
        server.orchestrator._last_audio_ms = 1200
        server.orchestrator._last_audio_stop_ms = 9
        server.orchestrator._last_finalize_ms = 120
        server.orchestrator._last_infer_ms = 85
        server.orchestrator._last_send_ms = 4
        session_id = uuid4()
        await server.orchestrator.sessions.start_session(session_id, owner_token=1)

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


def test_session_guard_warning_threshold_tracks_effective_sample_cap() -> None:
    server = _build_server()
    server.settings = ServerSettings(
        device="cpu",
        status_enabled=True,
        streaming_enabled=False,
        max_session_seconds=90.0,
        max_session_samples=16_000,
    )
    server.orchestrator.settings = server.settings
    server.orchestrator._session_age_limit_ms = 90_000
    server.orchestrator._session_sample_limit = 16_000
    session = Session(session_id=uuid4(), owner_token=1)
    session.started_at = datetime.now(tz=UTC) - timedelta(milliseconds=850)

    assert server.orchestrator._effective_session_limit_seconds() == 1.0
    assert server.orchestrator._session_guard_warning_due(session) is True
    assert 0.0 <= server.orchestrator._session_remaining_seconds(session) <= 0.2


def test_disconnect_from_non_owner_websocket_leaves_active_session_state() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.orchestrator.audio)
        owner_websocket = FakeWebSocket([])
        other_websocket = FakeWebSocket([WebSocketDisconnect()])
        session_id = uuid4()
        await server.orchestrator.sessions.start_session(
            session_id, owner_token=id(owner_websocket)
        )

        active_stream = cast(Any, object())
        drain_task = DummyDrainTask()
        server.orchestrator._active_stream = active_stream
        server.orchestrator._stream_drain_task = cast(Any, drain_task)
        server.orchestrator._stream_drain_running = True

        await server.handle_websocket(cast(Any, other_websocket))

        assert server.orchestrator.sessions.active is not None
        assert server.orchestrator.sessions.active.session_id == session_id
        assert server.orchestrator.sessions.active.owner_token == id(owner_websocket)
        assert audio.abort_calls == 0
        assert server.orchestrator._active_stream is active_stream
        assert server.orchestrator._stream_drain_task is drain_task
        assert drain_task.cancel_called is False

    asyncio.run(scenario())


def test_stop_session_from_non_owner_returns_not_found() -> None:
    async def scenario() -> None:
        server = _build_server()
        owner_websocket = FakeWebSocket([])
        other_websocket = FakeWebSocket([])
        session_id = uuid4()
        await server.orchestrator.sessions.start_session(
            session_id, owner_token=id(owner_websocket)
        )

        message = StopSession(
            type=ClientMessageType.STOP_SESSION,
            session_id=session_id,
            timestamp=datetime.now(tz=UTC),
        )
        await _dispatch_message(server, other_websocket, message)

        assert server.orchestrator.sessions.active is not None
        assert server.orchestrator.sessions.active.session_id == session_id
        assert server.orchestrator.sessions.active.owner_token == id(owner_websocket)
        assert other_websocket.sent_json
        assert other_websocket.sent_json[-1]["type"] == "error"
        assert other_websocket.sent_json[-1]["code"] == "SESSION_NOT_FOUND"

    asyncio.run(scenario())


def test_abort_session_from_non_owner_returns_not_found() -> None:
    async def scenario() -> None:
        server = _build_server()
        audio = cast(FakeAudio, server.orchestrator.audio)
        owner_websocket = FakeWebSocket([])
        other_websocket = FakeWebSocket([])
        session_id = uuid4()
        await server.orchestrator.sessions.start_session(
            session_id, owner_token=id(owner_websocket)
        )

        message = AbortSession(
            type=ClientMessageType.ABORT_SESSION,
            session_id=session_id,
            reason="user",
            timestamp=datetime.now(tz=UTC),
        )
        await _dispatch_message(server, other_websocket, message)

        assert server.orchestrator.sessions.active is not None
        assert server.orchestrator.sessions.active.session_id == session_id
        assert server.orchestrator.sessions.active.owner_token == id(owner_websocket)
        assert audio.abort_calls == 0
        assert other_websocket.sent_json
        assert other_websocket.sent_json[-1]["type"] == "error"
        assert other_websocket.sent_json[-1]["code"] == "SESSION_NOT_FOUND"

    asyncio.run(scenario())
