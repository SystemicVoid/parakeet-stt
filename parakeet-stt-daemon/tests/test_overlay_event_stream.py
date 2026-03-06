"""Overlay event stream invariants for Phase 2 emission wiring."""

from __future__ import annotations

import asyncio
import threading
from datetime import UTC, datetime
from typing import Any, cast
from uuid import UUID, uuid4

import numpy as np
from parakeet_stt_daemon import server as server_module
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

    def __init__(self, *, ready_chunks: list[np.ndarray] | None = None) -> None:
        self.ready_chunks = ready_chunks or []

    def start_session(self) -> None:
        return None

    def stop_session_with_streaming(self) -> tuple[np.ndarray, list[np.ndarray], np.ndarray]:
        samples = np.ones((1600,), dtype=np.float32)
        return samples, list(self.ready_chunks), np.zeros((0,), dtype=np.float32)

    def abort_session(self) -> None:
        return None

    def take_stream_chunks(self) -> list[np.ndarray]:
        return []


class FakeWebSocket:
    def __init__(self) -> None:
        self.sent_json: list[dict[str, object]] = []

    async def send_json(self, payload: dict[str, object]) -> None:
        self.sent_json.append(payload)


class FakeIncrementalTranscriber:
    def __init__(
        self,
        outputs: list[str] | None = None,
        *,
        fail_at_call: int | None = None,
    ) -> None:
        self.outputs = outputs or []
        self.fail_at_call = fail_at_call
        self.calls = 0
        self.sample_sizes: list[int] = []

    def transcribe_samples(self, samples: np.ndarray, *, sample_rate: int = 16_000) -> str:
        del sample_rate
        self.calls += 1
        self.sample_sizes.append(int(samples.size))
        if self.fail_at_call is not None and self.calls == self.fail_at_call:
            raise RuntimeError("incremental source failed")
        if self.calls <= len(self.outputs):
            return self.outputs[self.calls - 1]
        return ""


class BlockingIncrementalTranscriber:
    def __init__(self, output: str) -> None:
        self.output = output
        self.started = threading.Event()
        self.release = threading.Event()

    def transcribe_samples(self, samples: np.ndarray, *, sample_rate: int = 16_000) -> str:
        del samples, sample_rate
        self.started.set()
        assert self.release.wait(timeout=1.0)
        return self.output


def _build_server(
    *,
    overlay_events_enabled: bool,
    ready_chunks: list[np.ndarray] | None = None,
    incremental_outputs: list[str] | None = None,
    incremental_fail_at_call: int | None = None,
) -> DaemonServer:
    server = cast(Any, DaemonServer.__new__(DaemonServer))
    server.settings = ServerSettings(
        device="cpu",
        status_enabled=True,
        streaming_enabled=False,
        overlay_events_enabled=overlay_events_enabled,
    )
    server.sessions = SessionManager()
    server.audio = FakeAudio(ready_chunks=ready_chunks)
    server.model = object()
    server.transcriber = FakeIncrementalTranscriber(
        incremental_outputs,
        fail_at_call=incremental_fail_at_call,
    )
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
    server._live_interim_audio = np.zeros((0,), dtype=np.float32)
    server._live_interim_failed = False
    server._overlay_event_seq_by_session = {}
    server._overlay_last_interim_text_by_session = {}
    server._overlay_state_by_session = {}
    server._overlay_events_emitted = 0
    server._overlay_events_dropped = 0
    server._websocket_send_locks = {}

    async def fake_finalize(_audio_samples: np.ndarray) -> tuple[str, int]:
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


def _disable_server_sleep(monkeypatch) -> None:
    async def no_sleep(_seconds: float) -> None:
        return None

    monkeypatch.setattr("parakeet_stt_daemon.server.asyncio.sleep", no_sleep)


def test_overlay_events_disabled_emits_only_baseline_messages(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

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
        _disable_server_sleep(monkeypatch)

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
        _disable_server_sleep(monkeypatch)

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
        _disable_server_sleep(monkeypatch)

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


def test_interim_text_emitted_when_incremental_source_validates(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

        ready_chunks = [
            np.full((400,), 0.1, dtype=np.float32),
            np.full((400,), 0.2, dtype=np.float32),
            np.full((400,), 0.3, dtype=np.float32),
        ]
        server = _build_server(
            overlay_events_enabled=True,
            ready_chunks=ready_chunks,
            incremental_outputs=["hello", "hello", "hello world"],
        )
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
            "interim_text",
            "interim_text",
            "interim_state",
            "final_result",
            "session_ended",
        ]
        interim_texts = [
            cast(str, payload["text"])
            for payload in websocket.sent_json
            if payload["type"] == "interim_text"
        ]
        assert interim_texts == ["hello", "hello world"]
        assert [
            cast(int, payload["seq"])
            for payload in websocket.sent_json
            if payload["type"] in {"interim_state", "interim_text"}
        ] == [0, 1, 2, 3, 4, 5]

        status = server.status()
        assert status.overlay_events_emitted == 7
        assert status.overlay_events_dropped == 0

    asyncio.run(scenario())


def test_incremental_source_failure_does_not_break_final_result(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

        ready_chunks = [np.full((400,), 0.1, dtype=np.float32)]
        server = _build_server(
            overlay_events_enabled=True,
            ready_chunks=ready_chunks,
            incremental_outputs=["hello"],
            incremental_fail_at_call=1,
        )
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

        status = server.status()
        assert status.overlay_events_emitted == 4
        assert status.overlay_events_dropped == 0

    asyncio.run(scenario())


def test_live_interim_chunk_emission_dedupes_repeated_text(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

        server = _build_server(
            overlay_events_enabled=True,
            incremental_outputs=["hello", "hello", "hello world"],
        )
        websocket = FakeWebSocket()
        session_id = uuid4()

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        await server._emit_live_interim_from_chunk(
            cast(Any, websocket), session_id, np.full((400,), 0.1, dtype=np.float32)
        )
        await server._emit_live_interim_from_chunk(
            cast(Any, websocket), session_id, np.full((400,), 0.2, dtype=np.float32)
        )
        await server._emit_live_interim_from_chunk(
            cast(Any, websocket), session_id, np.full((400,), 0.3, dtype=np.float32)
        )

        sent_types = [cast(str, payload["type"]) for payload in websocket.sent_json]
        assert sent_types == [
            "session_started",
            "interim_state",
            "interim_text",
            "interim_text",
        ]
        interim_texts = [
            cast(str, payload["text"])
            for payload in websocket.sent_json
            if payload["type"] == "interim_text"
        ]
        assert interim_texts == ["hello", "hello world"]
        assert [
            cast(int, payload["seq"])
            for payload in websocket.sent_json
            if payload["type"] in {"interim_state", "interim_text"}
        ] == [0, 1, 2]

    asyncio.run(scenario())


def test_late_live_interim_is_dropped_once_final_send_begins(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

        server = _build_server(overlay_events_enabled=True)
        transcriber = BlockingIncrementalTranscriber("late interim")
        server.transcriber = transcriber
        websocket = FakeWebSocket()
        session_id = uuid4()
        final_started = asyncio.Event()
        allow_final_send = asyncio.Event()

        async def send_json(payload: dict[str, object]) -> None:
            if payload.get("type") == "final_result":
                final_started.set()
                await allow_final_send.wait()
            websocket.sent_json.append(payload)

        websocket.send_json = send_json  # type: ignore[method-assign]

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        live_task = asyncio.create_task(
            server._emit_live_interim_from_chunk(
                cast(Any, websocket),
                session_id,
                np.full((400,), 0.2, dtype=np.float32),
            )
        )
        await asyncio.to_thread(transcriber.started.wait)

        stop_task = asyncio.create_task(
            server._handle_stop(cast(Any, websocket), _stop_message(session_id))
        )
        await final_started.wait()

        transcriber.release.set()
        allow_final_send.set()

        await stop_task
        await live_task

        sent_types = [cast(str, payload["type"]) for payload in websocket.sent_json]
        assert sent_types == [
            "session_started",
            "interim_state",
            "interim_state",
            "interim_state",
            "final_result",
            "session_ended",
        ]
        session_ended_index = sent_types.index("session_ended")
        assert "interim_text" not in sent_types[session_ended_index + 1 :]

        status = server.status()
        assert status.overlay_events_emitted == 4
        assert status.overlay_events_dropped == 1

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


def test_phase6_quick_utterance_contract_preserves_final_once(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

        server = _build_server(
            overlay_events_enabled=True,
            ready_chunks=[np.full((400,), 0.1, dtype=np.float32)],
            incremental_outputs=["quick command"],
        )
        websocket = FakeWebSocket()
        session_id = uuid4()

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        await server._handle_stop(cast(Any, websocket), _stop_message(session_id))

        sent_types = [cast(str, payload["type"]) for payload in websocket.sent_json]
        assert sent_types.count("final_result") == 1
        assert sent_types[-1] == "session_ended"

        overlay_seqs = [
            cast(int, payload["seq"])
            for payload in websocket.sent_json
            if payload["type"] in {"interim_state", "interim_text"}
        ]
        assert overlay_seqs == sorted(overlay_seqs)

    asyncio.run(scenario())


def test_phase6_long_dictation_contract_preserves_monotonic_interim_tail(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

        ready_chunks = [
            np.full((400,), 0.1, dtype=np.float32),
            np.full((400,), 0.2, dtype=np.float32),
            np.full((400,), 0.3, dtype=np.float32),
            np.full((400,), 0.4, dtype=np.float32),
        ]
        server = _build_server(
            overlay_events_enabled=True,
            ready_chunks=ready_chunks,
            incremental_outputs=[
                "phase",
                "phase",
                "phase one",
                "phase one",
                "phase one two",
                "phase one two",
                "phase one two three",
                "phase one two three four",
                "phase one two three four five",
            ],
        )
        websocket = FakeWebSocket()
        session_id = uuid4()

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        for chunk in ready_chunks:
            await server._emit_live_interim_from_chunk(cast(Any, websocket), session_id, chunk)
        await server._handle_stop(cast(Any, websocket), _stop_message(session_id))

        sent_types = [cast(str, payload["type"]) for payload in websocket.sent_json]
        assert sent_types.count("final_result") == 1
        interim_texts = [
            cast(str, payload["text"])
            for payload in websocket.sent_json
            if payload["type"] == "interim_text"
        ]
        assert len(interim_texts) >= 4
        assert interim_texts[-1].startswith("phase one two three four")
        assert len(set(interim_texts)) == len(interim_texts)

        overlay_seqs = [
            cast(int, payload["seq"])
            for payload in websocket.sent_json
            if payload["type"] in {"interim_state", "interim_text"}
        ]
        assert overlay_seqs == sorted(overlay_seqs)

    asyncio.run(scenario())


def test_phase6_daemon_reconnect_contract_recovers_with_fresh_session(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

        server = _build_server(overlay_events_enabled=True)
        first_socket = FakeWebSocket()
        second_socket = FakeWebSocket()
        first_session = uuid4()
        second_session = uuid4()

        await server._handle_start(cast(Any, first_socket), _start_message(first_session))
        cleaned = await server._cleanup_active_session(
            "websocket disconnected",
            expected_session_id=first_session,
            require_session_match=True,
        )
        assert cleaned is True
        assert server.sessions.active is None

        await server._handle_start(cast(Any, second_socket), _start_message(second_session))
        await server._handle_stop(cast(Any, second_socket), _stop_message(second_session))

        sent_types = [cast(str, payload["type"]) for payload in second_socket.sent_json]
        assert sent_types.count("final_result") == 1
        assert sent_types[-1] == "session_ended"
        assert [
            cast(int, payload["seq"])
            for payload in second_socket.sent_json
            if payload["type"] == "interim_state"
        ] == [0, 1, 2]

    asyncio.run(scenario())


def test_phase6_overlay_crash_mid_session_contract_keeps_final_non_fatal(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

        server = _build_server(
            overlay_events_enabled=True,
            ready_chunks=[np.full((400,), 0.1, dtype=np.float32)],
            incremental_outputs=["first interim", "second interim"],
        )
        websocket = FakeWebSocket()
        session_id = uuid4()
        failed_overlay_sends = 0

        async def flaky_send_json(payload: dict[str, object]) -> None:
            nonlocal failed_overlay_sends
            if payload.get("type") in {"interim_state", "interim_text", "session_ended"}:
                if failed_overlay_sends < 2:
                    failed_overlay_sends += 1
                    raise RuntimeError("overlay process unavailable")
            websocket.sent_json.append(payload)

        websocket.send_json = flaky_send_json  # type: ignore[method-assign]

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        await server._emit_live_interim_from_chunk(
            cast(Any, websocket), session_id, np.full((400,), 0.2, dtype=np.float32)
        )
        await server._handle_stop(cast(Any, websocket), _stop_message(session_id))

        sent_types = [cast(str, payload["type"]) for payload in websocket.sent_json]
        assert sent_types.count("final_result") == 1
        assert "session_ended" in sent_types

        status = server.status()
        assert status.overlay_events_dropped is not None
        assert status.overlay_events_dropped >= 2

    asyncio.run(scenario())


def test_live_interim_context_window_is_bounded() -> None:
    async def scenario() -> None:
        server = _build_server(
            overlay_events_enabled=True,
            incremental_outputs=[f"chunk-{index}" for index in range(1, 10)],
        )
        websocket = FakeWebSocket()
        session_id = uuid4()

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        chunk = np.full((8_000,), 0.2, dtype=np.float32)
        for _ in range(8):
            await server._emit_live_interim_from_chunk(cast(Any, websocket), session_id, chunk)

        max_seen = max(cast(Any, server.transcriber).sample_sizes)
        expected_max = int(
            server.audio.sample_rate * server_module.OVERLAY_INTERIM_CONTEXT_WINDOW_SECS
        )
        assert max_seen <= expected_max

    asyncio.run(scenario())


def test_stop_path_interim_context_window_is_bounded(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

        ready_chunks = [np.full((8_000,), 0.2, dtype=np.float32) for _ in range(8)]
        server = _build_server(
            overlay_events_enabled=True,
            ready_chunks=ready_chunks,
            incremental_outputs=[f"stop-{index}" for index in range(1, 20)],
        )
        websocket = FakeWebSocket()
        session_id = uuid4()

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        await server._handle_stop(cast(Any, websocket), _stop_message(session_id))

        max_seen = max(cast(Any, server.transcriber).sample_sizes)
        expected_max = int(
            server.audio.sample_rate * server_module.OVERLAY_INTERIM_CONTEXT_WINDOW_SECS
        )
        assert max_seen <= expected_max

    asyncio.run(scenario())
