"""SessionOrchestrator lifecycle tests using RecordingEventSink."""

from __future__ import annotations

import asyncio
from datetime import UTC, datetime, timedelta
from typing import Any, TypeVar, cast
from uuid import UUID, uuid4

import numpy as np
from pytest import MonkeyPatch

from parakeet_stt_daemon import session_orchestrator as orchestrator_module
from parakeet_stt_daemon.config import ServerSettings
from parakeet_stt_daemon.events import (
    FinalResultEvent,
    RecordingEventSink,
    SessionEndedEvent,
    SessionErrorEvent,
    SessionWarningEvent,
)
from parakeet_stt_daemon.messages import SessionEndReason
from parakeet_stt_daemon.session import SessionManager
from parakeet_stt_daemon.session_orchestrator import (
    AbortSessionIntent,
    SessionOrchestrator,
    StartSessionIntent,
    StopSessionIntent,
)


class FakeAudio:
    sample_rate = 16_000

    def __init__(
        self,
        *,
        samples: np.ndarray | None = None,
        stream_chunks: list[np.ndarray] | None = None,
    ) -> None:
        self.samples = samples if samples is not None else np.ones((1_600,), dtype=np.float32)
        self.stream_chunks = list(stream_chunks or [])
        self.abort_calls = 0
        self.limit_exceeded = False
        self.start_calls = 0
        self.stop_calls = 0
        self.take_audio_levels_calls = 0
        self.take_stream_chunks_calls = 0

    def start_session(self) -> None:
        self.start_calls += 1

    def abort_session(self) -> None:
        self.abort_calls += 1

    def stop_session_with_streaming(self) -> tuple[np.ndarray, list[np.ndarray], np.ndarray]:
        self.stop_calls += 1
        return self.samples, [], np.zeros((0,), dtype=np.float32)

    def take_audio_levels(self) -> list[float]:
        self.take_audio_levels_calls += 1
        return []

    def take_stream_chunks(self) -> list[np.ndarray]:
        self.take_stream_chunks_calls += 1
        chunks = self.stream_chunks
        self.stream_chunks = []
        return chunks

    def session_limit_exceeded(self) -> bool:
        return self.limit_exceeded


class FakeStreamSession:
    def __init__(self, feed_results: list[bool] | None = None) -> None:
        self.feed_calls = 0
        self.feed_results = list(feed_results or [True])
        self.stream_fallback_reason: str | None = None

    def feed(self, _chunk: np.ndarray) -> bool:
        self.feed_calls += 1
        result = self.feed_results.pop(0) if self.feed_results else True
        if not result:
            self.stream_fallback_reason = "stream_chunk_failed:RuntimeError"
        return result


class FakeStreamingTranscriber:
    def __init__(
        self,
        *,
        helper_active: bool = True,
        fallback_reason: str | None = None,
    ) -> None:
        self.helper_active = helper_active
        self.fallback_reason = fallback_reason

    def start_session(self, _sample_rate: int) -> FakeStreamSession:
        return FakeStreamSession()


def _build_orchestrator(
    *,
    streaming_enabled: bool = False,
    max_session_seconds: float = 90.0,
    stream_chunks: list[np.ndarray] | None = None,
) -> SessionOrchestrator:
    orchestrator = cast(Any, SessionOrchestrator.__new__(SessionOrchestrator))
    settings = ServerSettings(
        device="cpu",
        streaming_enabled=streaming_enabled,
        overlay_events_enabled=True,
        max_session_seconds=max_session_seconds,
    )
    orchestrator.settings = settings
    orchestrator.sessions = SessionManager()
    orchestrator.audio = FakeAudio(stream_chunks=stream_chunks)
    orchestrator.model = object()
    orchestrator.transcriber = object()
    orchestrator._session_lock = asyncio.Lock()
    orchestrator._inference_lock = asyncio.Lock()
    orchestrator.streaming_transcriber = FakeStreamingTranscriber() if streaming_enabled else None
    orchestrator._active_stream = None
    orchestrator._stream_drain_task = None
    orchestrator._stream_drain_running = False
    orchestrator._current_stream_chunks_processed = 0
    orchestrator._current_stream_fallback_reason = None
    orchestrator._last_stream_path_executed = False
    orchestrator._last_stream_chunks_processed = 0
    orchestrator._last_stream_fallback_reason = None
    orchestrator._session_guard_task = None
    orchestrator._session_guard_running = False
    orchestrator._session_sample_limit = int(max_session_seconds * FakeAudio.sample_rate)
    orchestrator._session_age_limit_ms = int(max_session_seconds * 1000)
    orchestrator._requested_device = "cpu"
    orchestrator._effective_device = "cpu"
    orchestrator._last_audio_ms = None
    orchestrator._last_audio_stop_ms = None
    orchestrator._last_finalize_ms = None
    orchestrator._last_infer_ms = None
    orchestrator._last_send_ms = None
    orchestrator._interim_transcript_by_session = {}
    orchestrator._vad_enabled = False

    async def fake_collect_interim_text_updates(
        _session_id: UUID, _ready_chunks: list[np.ndarray]
    ) -> list[str]:
        return []

    async def fake_finalise(_audio_samples: np.ndarray) -> tuple[str, int]:
        return "final text", 7

    orchestrator._collect_interim_text_updates = fake_collect_interim_text_updates
    orchestrator._finalise_transcription = fake_finalise
    return cast(SessionOrchestrator, orchestrator)


T = TypeVar("T")


def _events_of_type(
    sink: RecordingEventSink,
    event_type: type[T],
) -> list[T]:
    return [event for event in sink.events if isinstance(event, event_type)]


async def _start(
    orchestrator: SessionOrchestrator,
    sink: RecordingEventSink,
    session_id: UUID,
    *,
    owner_token: int = 1,
) -> None:
    await orchestrator.start(
        StartSessionIntent(
            session_id=session_id,
            owner_token=owner_token,
            event_sink=sink,
        )
    )


def test_start_while_busy_emits_session_busy() -> None:
    async def scenario() -> None:
        orchestrator = _build_orchestrator()
        sink = RecordingEventSink()
        await _start(orchestrator, sink, uuid4())

        await _start(orchestrator, sink, uuid4())

        errors = _events_of_type(sink, SessionErrorEvent)
        assert errors
        assert errors[-1].code == "SESSION_BUSY"

    asyncio.run(scenario())


def test_start_keeps_stream_drain_loop_when_helper_inactive() -> None:
    async def scenario() -> None:
        orchestrator = _build_orchestrator(
            streaming_enabled=True,
            stream_chunks=[np.array([0.1, 0.2], dtype=np.float32)],
        )
        cast(Any, orchestrator).streaming_transcriber = FakeStreamingTranscriber(
            helper_active=False,
            fallback_reason="init_failed:ImportError",
        )
        audio = cast(FakeAudio, orchestrator.audio)
        sink = RecordingEventSink()
        session_id = uuid4()

        await _start(orchestrator, sink, session_id)
        await asyncio.sleep(0.06)
        await orchestrator._stop_stream_drain_loop()
        await orchestrator.sessions.clear(session_id, owner_token=1)

        assert orchestrator._active_stream is None
        assert orchestrator._current_stream_fallback_reason == "init_failed:ImportError"
        assert audio.take_audio_levels_calls > 0
        assert audio.take_stream_chunks_calls > 0

    asyncio.run(scenario())


def test_stop_records_stream_fallback_when_helper_ready_but_no_chunks_ran() -> None:
    async def scenario() -> None:
        orchestrator = _build_orchestrator(streaming_enabled=True)
        sink = RecordingEventSink()
        session_id = uuid4()
        await _start(orchestrator, sink, session_id)

        await orchestrator.stop(
            StopSessionIntent(
                session_id=session_id,
                owner_token=1,
                event_sink=sink,
                post_roll_secs=0.0,
            )
        )

        truth = orchestrator.runtime_truth(overlay_events_enabled=True)
        assert truth.stream_helper_active is True
        assert truth.stream_path_executed is False
        assert truth.stream_chunks_processed == 0
        assert truth.stream_fallback_reason == "stream_path_not_exercised:no_chunks"

    asyncio.run(scenario())


def test_stop_records_stream_path_execution_when_chunks_are_processed() -> None:
    async def scenario() -> None:
        orchestrator = _build_orchestrator(streaming_enabled=True)
        sink = RecordingEventSink()
        session_id = uuid4()
        await _start(orchestrator, sink, session_id)

        active_stream = cast(FakeStreamSession, orchestrator._active_stream)
        await orchestrator._feed_stream_chunk(
            cast(Any, active_stream),
            np.array([0.1, 0.2], dtype=np.float32),
        )
        await orchestrator.stop(
            StopSessionIntent(
                session_id=session_id,
                owner_token=1,
                event_sink=sink,
                post_roll_secs=0.0,
            )
        )

        truth = orchestrator.runtime_truth(overlay_events_enabled=True)
        assert active_stream.feed_calls == 1
        assert truth.stream_path_executed is True
        assert truth.stream_chunks_processed == 1
        assert truth.stream_fallback_reason is None

    asyncio.run(scenario())


def test_stop_preserves_stream_fallback_after_successful_chunk() -> None:
    async def scenario() -> None:
        orchestrator = _build_orchestrator(streaming_enabled=True)
        sink = RecordingEventSink()
        session_id = uuid4()
        await _start(orchestrator, sink, session_id)

        active_stream = FakeStreamSession(feed_results=[True, False])
        orchestrator._active_stream = cast(Any, active_stream)
        await orchestrator._feed_stream_chunk(
            cast(Any, active_stream),
            np.array([0.1], dtype=np.float32),
        )
        await orchestrator._feed_stream_chunk(
            cast(Any, active_stream),
            np.array([0.2], dtype=np.float32),
        )
        await orchestrator.stop(
            StopSessionIntent(
                session_id=session_id,
                owner_token=1,
                event_sink=sink,
                post_roll_secs=0.0,
            )
        )

        truth = orchestrator.runtime_truth(overlay_events_enabled=True)
        assert active_stream.feed_calls == 2
        assert truth.stream_path_executed is True
        assert truth.stream_chunks_processed == 1
        assert truth.stream_fallback_reason == "stream_chunk_failed:RuntimeError"
        assert truth.degraded is True

    asyncio.run(scenario())


def test_stop_without_start_emits_session_not_found() -> None:
    async def scenario() -> None:
        orchestrator = _build_orchestrator()
        sink = RecordingEventSink()
        session_id = uuid4()

        await orchestrator.stop(
            StopSessionIntent(
                session_id=session_id,
                owner_token=1,
                event_sink=sink,
                post_roll_secs=0.0,
            )
        )

        errors = _events_of_type(sink, SessionErrorEvent)
        assert errors == [
            SessionErrorEvent(
                session_id=session_id,
                code="SESSION_NOT_FOUND",
                message="No matching active session",
            )
        ]

    asyncio.run(scenario())


def test_abort_mid_stream_cleans_runtime_without_final_result() -> None:
    async def scenario() -> None:
        orchestrator = _build_orchestrator(streaming_enabled=True)
        sink = RecordingEventSink()
        session_id = uuid4()

        await _start(orchestrator, sink, session_id)
        assert orchestrator._active_stream is not None

        await orchestrator.abort(
            AbortSessionIntent(
                session_id=session_id,
                owner_token=1,
                event_sink=sink,
                reason="user",
            )
        )

        assert orchestrator.sessions.active is None
        assert cast(FakeAudio, orchestrator.audio).abort_calls == 1
        assert _events_of_type(sink, FinalResultEvent) == []
        ended = _events_of_type(sink, SessionEndedEvent)
        assert ended
        assert ended[-1].reason == SessionEndReason.ABORT

    asyncio.run(scenario())


def test_guard_loop_warns_at_80_percent_then_terminates_at_100_percent(
    monkeypatch: MonkeyPatch,
) -> None:
    async def scenario() -> None:
        orchestrator = _build_orchestrator(max_session_seconds=1.0)
        sink = RecordingEventSink()
        session_id = uuid4()
        sleep_calls = 0

        async def fake_guard_sleep(_seconds: float) -> None:
            nonlocal sleep_calls
            sleep_calls += 1
            active = orchestrator.sessions.active
            if active is not None and sleep_calls == 1:
                active.started_at = datetime.now(tz=UTC) - timedelta(milliseconds=850)
            elif active is not None and sleep_calls == 2:
                active.started_at = datetime.now(tz=UTC) - timedelta(milliseconds=1100)
            await asyncio.sleep(0)

        monkeypatch.setattr(orchestrator_module, "_REAL_ASYNCIO_SLEEP", fake_guard_sleep)

        await _start(orchestrator, sink, session_id)
        for _ in range(20):
            if orchestrator.sessions.active is None:
                break
            await asyncio.sleep(0)

        assert orchestrator.sessions.active is None
        assert cast(FakeAudio, orchestrator.audio).stop_calls == 1
        warnings = _events_of_type(sink, SessionWarningEvent)
        assert len(warnings) == 1
        assert warnings[0].warning == "approaching_limit"
        assert warnings[0].remaining_seconds <= 0.2

    asyncio.run(scenario())


def test_guard_loop_auto_stops_when_audio_sample_limit_is_exceeded(
    monkeypatch: MonkeyPatch,
) -> None:
    async def scenario() -> None:
        orchestrator = _build_orchestrator(max_session_seconds=1.0)
        audio = cast(FakeAudio, orchestrator.audio)
        sink = RecordingEventSink()
        session_id = uuid4()

        async def fake_guard_sleep(_seconds: float) -> None:
            active = orchestrator.sessions.active
            if active is not None:
                active.started_at = datetime.now(tz=UTC) - timedelta(milliseconds=850)
                audio.limit_exceeded = True
            await asyncio.sleep(0)

        monkeypatch.setattr(orchestrator_module, "_REAL_ASYNCIO_SLEEP", fake_guard_sleep)

        await _start(orchestrator, sink, session_id)
        for _ in range(20):
            if orchestrator.sessions.active is None:
                break
            await asyncio.sleep(0)

        assert orchestrator.sessions.active is None
        assert audio.stop_calls == 1
        warnings = _events_of_type(sink, SessionWarningEvent)
        assert len(warnings) == 1
        assert warnings[0].warning == "approaching_limit"
        finals = _events_of_type(sink, FinalResultEvent)
        ended = _events_of_type(sink, SessionEndedEvent)
        assert len(finals) == 1
        assert ended
        assert ended[-1].reason == SessionEndReason.FINAL

    asyncio.run(scenario())


def test_clean_disconnect_during_streaming_emits_abort_end_reason() -> None:
    async def scenario() -> None:
        orchestrator = _build_orchestrator(streaming_enabled=True)
        sink = RecordingEventSink()
        session_id = uuid4()
        await _start(orchestrator, sink, session_id)

        cleaned = await orchestrator._cleanup_active_session(
            "websocket disconnected",
            expected_session_id=session_id,
            expected_owner_token=1,
            require_session_match=True,
            require_owner_match=True,
            event_sink=sink,
            session_end_reason=SessionEndReason.ABORT,
        )

        assert cleaned is True
        assert orchestrator.sessions.active is None
        ended = _events_of_type(sink, SessionEndedEvent)
        assert ended
        assert ended[-1].reason == SessionEndReason.ABORT

    asyncio.run(scenario())


def test_finalization_emits_one_final_result_then_final_session_ended() -> None:
    async def scenario() -> None:
        orchestrator = _build_orchestrator()
        sink = RecordingEventSink()
        session_id = uuid4()
        await _start(orchestrator, sink, session_id)

        await orchestrator.stop(
            StopSessionIntent(
                session_id=session_id,
                owner_token=1,
                event_sink=sink,
                post_roll_secs=0.0,
            )
        )

        finals = _events_of_type(sink, FinalResultEvent)
        ended = _events_of_type(sink, SessionEndedEvent)
        assert len(finals) == 1
        assert len(ended) == 1
        assert ended[0].reason == SessionEndReason.FINAL
        final_index = sink.events.index(finals[0])
        ended_index = sink.events.index(ended[0])
        assert final_index < ended_index

    asyncio.run(scenario())
