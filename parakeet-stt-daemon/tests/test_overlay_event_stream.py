"""Overlay event stream invariants for Phase 2 emission wiring."""

from __future__ import annotations

import asyncio
import io
import threading
from contextlib import contextmanager
from datetime import UTC, datetime
from typing import Any, cast
from uuid import UUID, uuid4

import numpy as np
from loguru import logger
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
        self.abort_calls = 0
        self.stop_calls = 0
        self._session_limit_exceeded = False

    def start_session(self) -> None:
        return None

    def stop_session_with_streaming(self) -> tuple[np.ndarray, list[np.ndarray], np.ndarray]:
        self.stop_calls += 1
        samples = np.ones((1600,), dtype=np.float32)
        return samples, list(self.ready_chunks), np.zeros((0,), dtype=np.float32)

    def abort_session(self) -> None:
        self.abort_calls += 1

    def take_stream_chunks(self) -> list[np.ndarray]:
        return []

    def session_limit_exceeded(self) -> bool:
        return self._session_limit_exceeded


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


class SerializingTranscriber:
    def __init__(self) -> None:
        self.live_started = threading.Event()
        self.final_started = threading.Event()
        self.release_live = threading.Event()
        self.calls = 0
        self.active_calls = 0
        self.max_active_calls = 0
        self._state_lock = threading.Lock()

    def transcribe_samples(self, samples: np.ndarray, *, sample_rate: int = 16_000) -> str:
        del samples, sample_rate
        with self._state_lock:
            self.calls += 1
            call_number = self.calls
            self.active_calls += 1
            self.max_active_calls = max(self.max_active_calls, self.active_calls)
        try:
            if call_number == 1:
                self.live_started.set()
                assert self.release_live.wait(timeout=1.0)
                return "live interim"
            if call_number == 2:
                self.final_started.set()
                return "final text"
            raise AssertionError(f"unexpected transcribe call {call_number}")
        finally:
            with self._state_lock:
                self.active_calls -= 1


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
    server._session_lock = asyncio.Lock()
    server._inference_lock = asyncio.Lock()
    server.streaming_transcriber = None
    server._active_stream = None
    server._stream_drain_task = None
    server._stream_drain_running = False
    server._session_guard_task = None
    server._session_guard_running = False
    server._session_sample_limit = 1_600
    server._session_age_limit_ms = 90_000
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
    server._overlay_interim_transcript_by_session = {}
    server._overlay_interim_source_seq_by_session = {}
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


def _pause_guard_sleep(monkeypatch) -> tuple[asyncio.Event, asyncio.Event]:
    guard_sleep_entered = asyncio.Event()
    allow_guard_resume = asyncio.Event()

    async def fake_guard_sleep(_seconds: float) -> None:
        guard_sleep_entered.set()
        await allow_guard_resume.wait()

    monkeypatch.setattr(server_module, "_REAL_ASYNCIO_SLEEP", fake_guard_sleep)
    return guard_sleep_entered, allow_guard_resume


@contextmanager
def _capture_loguru_messages() -> Any:
    buffer = io.StringIO()
    handler_id = logger.add(buffer, format="{message}")
    try:
        yield buffer
    finally:
        logger.remove(handler_id)


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


def test_stop_session_finalizes_gracefully_when_sample_limit_breached(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)
        guard_sleep_entered, allow_guard_resume = _pause_guard_sleep(monkeypatch)

        server = _build_server(overlay_events_enabled=False)
        audio = cast(FakeAudio, server.audio)
        websocket = FakeWebSocket()
        session_id = uuid4()

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        await asyncio.wait_for(guard_sleep_entered.wait(), timeout=1.0)

        audio._session_limit_exceeded = True
        await server._handle_stop(cast(Any, websocket), _stop_message(session_id))
        allow_guard_resume.set()
        await asyncio.sleep(0)

        sent_types = [cast(str, payload["type"]) for payload in websocket.sent_json]
        # Graceful cutoff: finalization runs on capped audio instead of aborting.
        assert "final_result" in sent_types
        assert audio.abort_calls == 0
        assert audio.stop_calls == 1
        assert server.sessions.active is None

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


def test_live_interim_chunk_emission_confirms_repeated_text_before_append(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

        server = _build_server(
            overlay_events_enabled=True,
            incremental_outputs=["hello", "hello", "hello world", "hello world"],
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
        await server._emit_live_interim_from_chunk(
            cast(Any, websocket), session_id, np.full((400,), 0.4, dtype=np.float32)
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


def test_live_interim_first_snapshot_is_visible_immediately(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

        server = _build_server(
            overlay_events_enabled=True,
            incremental_outputs=["hello there"],
        )
        websocket = FakeWebSocket()
        session_id = uuid4()

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        await server._emit_live_interim_from_chunk(
            cast(Any, websocket), session_id, np.full((400,), 0.1, dtype=np.float32)
        )

        interim_texts = [
            cast(str, payload["text"])
            for payload in websocket.sent_json
            if payload["type"] == "interim_text"
        ]
        assert interim_texts == ["hello there"]

    asyncio.run(scenario())


def test_live_interim_zero_overlap_rewrite_replaces_mutable_tail(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

        server = _build_server(
            overlay_events_enabled=True,
            incremental_outputs=["their", "there speech", "there speech"],
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

        interim_texts = [
            cast(str, payload["text"])
            for payload in websocket.sent_json
            if payload["type"] == "interim_text"
        ]
        assert interim_texts == ["their", "there speech"]

    asyncio.run(scenario())


def test_live_interim_stabilizer_logs_when_streaming_debug_enabled(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)
        monkeypatch.setenv("PARAKEET_STREAMING_DEBUG", "1")

        server = _build_server(
            overlay_events_enabled=True,
            incremental_outputs=["alpha beta", "beta gamma"],
        )
        websocket = FakeWebSocket()
        session_id = uuid4()

        with _capture_loguru_messages() as log_output:
            await server._handle_start(cast(Any, websocket), _start_message(session_id))
            await server._emit_live_interim_from_chunk(
                cast(Any, websocket), session_id, np.full((400,), 0.1, dtype=np.float32)
            )
            await server._emit_live_interim_from_chunk(
                cast(Any, websocket), session_id, np.full((400,), 0.2, dtype=np.float32)
            )

        messages = log_output.getvalue()
        assert "overlay_stabilizer" in messages
        assert f"session_id={session_id}" in messages
        assert "source=live source_seq=0" in messages
        assert "source=live source_seq=1" in messages
        assert 'raw_text="beta gamma"' in messages
        assert "overlap=1" in messages
        assert 'current_display="alpha beta gamma"' in messages

    asyncio.run(scenario())


def test_stop_path_stabilizer_logs_when_streaming_debug_enabled(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)
        monkeypatch.setenv("PARAKEET_STREAMING_DEBUG", "true")

        ready_chunks = [np.full((400,), 0.1, dtype=np.float32) for _ in range(2)]
        server = _build_server(
            overlay_events_enabled=True,
            ready_chunks=ready_chunks,
            incremental_outputs=["phase one", "one two"],
        )
        websocket = FakeWebSocket()
        session_id = uuid4()

        with _capture_loguru_messages() as log_output:
            await server._handle_start(cast(Any, websocket), _start_message(session_id))
            await server._handle_stop(cast(Any, websocket), _stop_message(session_id))

        messages = log_output.getvalue()
        assert "source=stop_replay source_seq=0" in messages
        assert "source=stop_replay source_seq=1" in messages
        assert 'raw_text="one two"' in messages
        assert "overlap=1" in messages
        assert 'current_display="phase one two"' in messages

    asyncio.run(scenario())


def test_stabilizer_logs_empty_candidate_action_when_debug_enabled(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)
        monkeypatch.setenv("PARAKEET_STREAMING_DEBUG", "on")

        server = _build_server(
            overlay_events_enabled=True,
            incremental_outputs=["   "],
        )
        websocket = FakeWebSocket()
        session_id = uuid4()

        with _capture_loguru_messages() as log_output:
            await server._handle_start(cast(Any, websocket), _start_message(session_id))
            await server._emit_live_interim_from_chunk(
                cast(Any, websocket), session_id, np.full((400,), 0.1, dtype=np.float32)
            )

        messages = log_output.getvalue()
        assert "overlay_stabilizer" in messages
        assert "action=empty" in messages
        assert 'normalized_text=""' in messages
        assert all(payload["type"] != "interim_text" for payload in websocket.sent_json)

    asyncio.run(scenario())


def test_stabilizer_logs_skip_on_live_transcribe_error_when_debug_enabled(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)
        monkeypatch.setenv("PARAKEET_STREAMING_DEBUG", "yes")

        server = _build_server(
            overlay_events_enabled=True,
            incremental_outputs=["ignored"],
            incremental_fail_at_call=1,
        )
        websocket = FakeWebSocket()
        session_id = uuid4()

        with _capture_loguru_messages() as log_output:
            await server._handle_start(cast(Any, websocket), _start_message(session_id))
            await server._emit_live_interim_from_chunk(
                cast(Any, websocket), session_id, np.full((400,), 0.1, dtype=np.float32)
            )

        messages = log_output.getvalue()
        assert "overlay_stabilizer_skip" in messages
        assert "source=live source_seq=0" in messages
        assert "reason=transcribe_error" in messages
        assert "error_class=RuntimeError" in messages

    asyncio.run(scenario())


def test_stabilizer_logging_is_disabled_by_default(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)
        monkeypatch.delenv("PARAKEET_STREAMING_DEBUG", raising=False)

        server = _build_server(
            overlay_events_enabled=True,
            incremental_outputs=["alpha beta", "beta gamma"],
        )
        websocket = FakeWebSocket()
        session_id = uuid4()

        with _capture_loguru_messages() as log_output:
            await server._handle_start(cast(Any, websocket), _start_message(session_id))
            await server._emit_live_interim_from_chunk(
                cast(Any, websocket), session_id, np.full((400,), 0.1, dtype=np.float32)
            )
            await server._emit_live_interim_from_chunk(
                cast(Any, websocket), session_id, np.full((400,), 0.2, dtype=np.float32)
            )

        messages = log_output.getvalue()
        assert "overlay_stabilizer" not in messages
        assert "overlay_stabilizer_skip" not in messages

    asyncio.run(scenario())


def test_live_interim_case_only_updates_are_emitted(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

        server = _build_server(
            overlay_events_enabled=True,
            incremental_outputs=["hello world", "Hello world"],
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

        interim_texts = [
            cast(str, payload["text"])
            for payload in websocket.sent_json
            if payload["type"] == "interim_text"
        ]
        assert interim_texts == ["hello world", "Hello world"]

    asyncio.run(scenario())


def test_live_interim_stabilizer_preserves_full_session_transcript_when_window_rolls(
    monkeypatch,
) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

        ready_chunks = [np.full((400,), 0.1, dtype=np.float32) for _ in range(6)]
        server = _build_server(
            overlay_events_enabled=True,
            ready_chunks=ready_chunks,
            incremental_outputs=[
                "alpha beta",
                "alpha beta",
                "beta gamma",
                "beta gamma delta",
                "gamma delta",
                "gamma delta epsilon",
            ],
        )
        websocket = FakeWebSocket()
        session_id = uuid4()

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        for chunk in ready_chunks[:3]:
            await server._emit_live_interim_from_chunk(cast(Any, websocket), session_id, chunk)
        await server._handle_stop(cast(Any, websocket), _stop_message(session_id))

        interim_texts = [
            cast(str, payload["text"])
            for payload in websocket.sent_json
            if payload["type"] == "interim_text"
        ]
        assert interim_texts == [
            "alpha beta",
            "alpha beta gamma",
            "alpha beta gamma delta",
            "alpha beta gamma delta epsilon",
        ]
        assert interim_texts[-1] == "alpha beta gamma delta epsilon"

    asyncio.run(scenario())


def test_stop_waits_for_in_flight_live_interim_before_final_send(monkeypatch) -> None:
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
        server._stream_drain_task = live_task
        server._stream_drain_running = True
        await asyncio.to_thread(transcriber.started.wait)

        stop_task = asyncio.create_task(
            server._handle_stop(cast(Any, websocket), _stop_message(session_id))
        )
        await asyncio.sleep(0)

        assert final_started.is_set() is False

        transcriber.release.set()
        allow_final_send.set()

        await stop_task
        assert final_started.is_set() is True
        assert live_task.cancelled() is True

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
        assert status.overlay_events_dropped == 0

    asyncio.run(scenario())


def test_stop_path_serializes_live_interim_and_final_decode(monkeypatch) -> None:
    async def scenario() -> None:
        _disable_server_sleep(monkeypatch)

        server = _build_server(overlay_events_enabled=True)
        transcriber = SerializingTranscriber()
        server.transcriber = transcriber
        server._trim_tail_silence = (  # type: ignore[method-assign]
            lambda samples, _sample_rate, _window_ms=50: samples
        )
        server._finalise_transcription = (  # type: ignore[method-assign]
            DaemonServer._finalise_transcription.__get__(server, DaemonServer)
        )
        websocket = FakeWebSocket()
        session_id = uuid4()

        await server._handle_start(cast(Any, websocket), _start_message(session_id))
        live_task = asyncio.create_task(
            server._emit_live_interim_from_chunk(
                cast(Any, websocket),
                session_id,
                np.full((400,), 0.2, dtype=np.float32),
            )
        )
        server._stream_drain_task = live_task
        server._stream_drain_running = True
        await asyncio.to_thread(transcriber.live_started.wait)

        stop_task = asyncio.create_task(
            server._handle_stop(cast(Any, websocket), _stop_message(session_id))
        )
        await asyncio.sleep(0)

        assert transcriber.final_started.is_set() is False

        transcriber.release_live.set()

        await stop_task

        assert live_task.cancelled() is True
        assert transcriber.calls == 2
        assert transcriber.max_active_calls == 1
        sent_types = [cast(str, payload["type"]) for payload in websocket.sent_json]
        assert sent_types == [
            "session_started",
            "interim_state",
            "interim_state",
            "interim_state",
            "final_result",
            "session_ended",
        ]

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
        interim_texts = [
            cast(str, payload["text"])
            for payload in websocket.sent_json
            if payload["type"] == "interim_text"
        ]
        assert interim_texts == ["quick command"]

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
        assert len(interim_texts) >= 5
        assert interim_texts[-1] == "phase one two three four"
        assert all(
            current.startswith(previous)
            for previous, current in zip(interim_texts, interim_texts[1:], strict=False)
        )
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

        server = _build_server(
            overlay_events_enabled=True,
            incremental_outputs=["first phrase", "first phrase"],
        )
        first_socket = FakeWebSocket()
        second_socket = FakeWebSocket()
        first_session = uuid4()
        second_session = uuid4()

        await server._handle_start(cast(Any, first_socket), _start_message(first_session))
        await server._emit_live_interim_from_chunk(
            cast(Any, first_socket), first_session, np.full((400,), 0.2, dtype=np.float32)
        )
        await server._emit_live_interim_from_chunk(
            cast(Any, first_socket), first_session, np.full((400,), 0.3, dtype=np.float32)
        )
        cleaned = await server._cleanup_active_session(
            "websocket disconnected",
            expected_session_id=first_session,
            require_session_match=True,
        )
        assert cleaned is True
        assert server.sessions.active is None
        assert server._overlay_interim_transcript_by_session == {}

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


def test_live_interim_context_window_grows_across_session() -> None:
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

        sample_sizes = cast(Any, server.transcriber).sample_sizes
        assert sample_sizes == sorted(sample_sizes)
        assert sample_sizes[-1] == 8 * 8_000

    asyncio.run(scenario())


def test_stop_path_interim_context_window_grows_across_chunks(monkeypatch) -> None:
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

        sample_sizes = cast(Any, server.transcriber).sample_sizes
        assert sample_sizes == sorted(sample_sizes)
        assert sample_sizes[-1] == 8 * 8_000

    asyncio.run(scenario())
