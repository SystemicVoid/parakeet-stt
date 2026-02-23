"""Lifecycle cleanup invariants for disconnect/error handling."""

from __future__ import annotations

import asyncio
from datetime import UTC, datetime
from typing import Any, cast
from uuid import UUID, uuid4

from fastapi import WebSocketDisconnect

from parakeet_stt_daemon.config import ServerSettings
from parakeet_stt_daemon.messages import AbortSession, ClientMessageType, StartSession
from parakeet_stt_daemon.server import DaemonServer
from parakeet_stt_daemon.session import SessionManager


class FakeAudio:
    sample_rate = 16_000

    def __init__(self) -> None:
        self.abort_calls = 0
        self.start_calls = 0
        self.raise_on_start = False

    def start_session(self) -> None:
        self.start_calls += 1
        if self.raise_on_start:
            raise RuntimeError("audio start failed")

    def abort_session(self) -> None:
        self.abort_calls += 1

    def take_stream_chunks(self) -> list[object]:
        return []


class DummyDrainTask:
    def __init__(self) -> None:
        self.cancel_called = False

    def done(self) -> bool:
        return False

    def cancel(self) -> None:
        self.cancel_called = True


class FakeStreamingTranscriber:
    def __init__(self, raise_on_start: bool = False) -> None:
        self.raise_on_start = raise_on_start

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
    return cast(DaemonServer, server)


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

        def explode_start_stream_drain_loop() -> None:
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
