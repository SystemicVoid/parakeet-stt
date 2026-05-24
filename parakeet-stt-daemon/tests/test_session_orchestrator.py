"""SessionOrchestrator lifecycle tests using RecordingEventSink."""

from __future__ import annotations

import asyncio
import json
import threading
from datetime import UTC, datetime, timedelta
from typing import Any, TypeVar, cast
from uuid import UUID, uuid4

import numpy as np
from pytest import MonkeyPatch

from parakeet_stt_daemon import session_orchestrator as orchestrator_module
from parakeet_stt_daemon.audio import AudioInput, CaptureSessionResult
from parakeet_stt_daemon.config import ServerSettings
from parakeet_stt_daemon.events import (
    FinalResultEvent,
    RecordingEventSink,
    SessionEndedEvent,
    SessionErrorEvent,
    SessionWarningEvent,
)
from parakeet_stt_daemon.messages import SessionEndReason
from parakeet_stt_daemon.session import (
    SealPathFinalizationFailure,
    SealPathFinalizationResult,
    SealPathRuntime,
    SessionManager,
    StreamPathRuntime,
)
from parakeet_stt_daemon.session_orchestrator import (
    AbortSessionIntent,
    SessionOrchestrator,
    StartSessionIntent,
    StopSessionIntent,
)
from parakeet_stt_daemon.tail_trim import SealPathTailTrimmer


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

    def stop_session_with_streaming(self) -> CaptureSessionResult:
        self.stop_calls += 1
        return CaptureSessionResult(
            audio_samples=self.samples,
            ready_chunks=[],
            tail_buffer=np.zeros((0,), dtype=np.float32),
            pre_roll_samples=0,
            post_start_samples=int(self.samples.size),
        )

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


class RecordingAudioInput(AudioInput):
    def __init__(self, **kwargs: Any) -> None:
        super().__init__(**kwargs)
        self.last_capture_result: CaptureSessionResult | None = None

    def stop_session_with_streaming(self) -> CaptureSessionResult:
        result = super().stop_session_with_streaming()
        self.last_capture_result = result
        return result


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


class FakeTranscriber:
    def __init__(self, text: str = "final text") -> None:
        self.text = text
        self.calls: list[tuple[np.ndarray, int]] = []

    def transcribe_samples(self, samples: np.ndarray, *, sample_rate: int = 16_000) -> str:
        self.calls.append((samples.copy(), sample_rate))
        return self.text


class InferenceOverlapProbe:
    def __init__(self) -> None:
        self.live_started = threading.Event()
        self.release_live = threading.Event()
        self.stream_feed_started = threading.Event()
        self.release_stream_feed = threading.Event()
        self.final_started = threading.Event()
        self.order: list[str] = []
        self.max_active_calls = 0
        self._active_calls = 0
        self._lock = threading.Lock()

    def run(
        self,
        name: str,
        *,
        started: threading.Event | None = None,
        release: threading.Event | None = None,
        result: Any = None,
    ) -> Any:
        with self._lock:
            self._active_calls += 1
            self.max_active_calls = max(self.max_active_calls, self._active_calls)
            self.order.append(f"{name}:start")
        if started is not None:
            started.set()
        try:
            if release is not None and not release.wait(timeout=2.0):
                raise AssertionError(f"timed out waiting to release {name}")
            return result
        finally:
            with self._lock:
                self._active_calls -= 1
                self.order.append(f"{name}:finish")


class LiveFinalOverlapTranscriber:
    def __init__(self, probe: InferenceOverlapProbe) -> None:
        self.probe = probe
        self.calls = 0
        self._lock = threading.Lock()

    def transcribe_samples(self, samples: np.ndarray, *, sample_rate: int = 16_000) -> str:
        del samples, sample_rate
        with self._lock:
            self.calls += 1
            call_number = self.calls
        if call_number == 1:
            return cast(
                str,
                self.probe.run(
                    "live",
                    started=self.probe.live_started,
                    release=self.probe.release_live,
                    result="live interim",
                ),
            )
        if call_number == 2:
            return cast(
                str,
                self.probe.run("final", started=self.probe.final_started, result="final text"),
            )
        return cast(str, self.probe.run(f"unexpected_live_{call_number}", result="queued live"))


class FinalOnlyOverlapTranscriber:
    def __init__(self, probe: InferenceOverlapProbe) -> None:
        self.probe = probe

    def transcribe_samples(self, samples: np.ndarray, *, sample_rate: int = 16_000) -> str:
        del samples, sample_rate
        return cast(
            str,
            self.probe.run("final", started=self.probe.final_started, result="final text"),
        )


class BlockingStreamSession:
    def __init__(self, probe: InferenceOverlapProbe) -> None:
        self.probe = probe
        self.feed_calls = 0
        self.stream_fallback_reason: str | None = None

    def feed(self, chunk: np.ndarray) -> bool:
        del chunk
        self.feed_calls += 1
        return cast(
            bool,
            self.probe.run(
                "stream_feed",
                started=self.probe.stream_feed_started,
                release=self.probe.release_stream_feed,
                result=True,
            ),
        )


class BlockingStreamingTranscriber:
    helper_active = True
    fallback_reason: str | None = None

    def __init__(self, probe: InferenceOverlapProbe) -> None:
        self.stream_session = BlockingStreamSession(probe)

    def start_session(self, _sample_rate: int) -> BlockingStreamSession:
        return self.stream_session


class FakeSealPathRuntime(SealPathRuntime):
    def __init__(self, text: str = "final text") -> None:
        super().__init__(
            sample_rate=16_000,
            tail_trimmer=SealPathTailTrimmer(vad_enabled=False, silence_floor_db=-40.0),
            release_device_cache=lambda _device: None,
        )
        self.text = text
        self.finalize_calls: list[np.ndarray] = []

    async def finalize(
        self,
        audio_samples: np.ndarray,
        transcribe,
        *,
        effective_device: str,
    ) -> SealPathFinalizationResult | SealPathFinalizationFailure:
        del transcribe, effective_device
        self.finalize_calls.append(audio_samples.copy())
        if audio_samples.size == 0:
            return SealPathFinalizationFailure(
                code="AUDIO_DEVICE",
                message="No audio captured for session",
            )
        audio_duration_raw = len(audio_samples) / self.sample_rate
        return SealPathFinalizationResult(
            text=self.text,
            audio_ms=int(audio_duration_raw * 1000),
            audio_duration_raw=audio_duration_raw,
            finalize_ms=0,
            infer_ms=7,
        )


def _build_orchestrator(
    *,
    streaming_enabled: bool = False,
    max_session_seconds: float = 90.0,
    max_session_samples: int | None = None,
    audio: Any | None = None,
    samples: np.ndarray | None = None,
    stream_chunks: list[np.ndarray] | None = None,
) -> SessionOrchestrator:
    orchestrator = cast(Any, SessionOrchestrator.__new__(SessionOrchestrator))
    audio_obj = (
        audio if audio is not None else FakeAudio(samples=samples, stream_chunks=stream_chunks)
    )
    sample_rate = int(getattr(audio_obj, "sample_rate", FakeAudio.sample_rate))
    settings = ServerSettings(
        device="cpu",
        streaming_enabled=streaming_enabled,
        overlay_events_enabled=True,
        max_session_seconds=max_session_seconds,
        max_session_samples=max_session_samples,
    )
    orchestrator.settings = settings
    orchestrator.sessions = SessionManager()
    orchestrator.audio = audio_obj
    orchestrator.model = object()
    orchestrator.transcriber = FakeTranscriber()
    orchestrator._session_lock = asyncio.Lock()
    orchestrator._inference_lock = asyncio.Lock()
    orchestrator.streaming_transcriber = FakeStreamingTranscriber() if streaming_enabled else None
    orchestrator._active_stream = None
    orchestrator._stream_drain_task = None
    orchestrator._stream_drain_running = False
    orchestrator._session_guard_task = None
    orchestrator._session_guard_running = False
    duration_limit_samples = max(1, int(max_session_seconds * sample_rate))
    explicit_sample_limit = (
        int(max_session_samples) if max_session_samples is not None else duration_limit_samples
    )
    orchestrator._session_sample_limit = max(1, min(duration_limit_samples, explicit_sample_limit))
    orchestrator._session_age_limit_ms = max(1, int(max_session_seconds * 1000))
    orchestrator._requested_device = "cpu"
    orchestrator._effective_device = "cpu"
    orchestrator._vad_enabled = False
    orchestrator.tail_trimmer = SealPathTailTrimmer(vad_enabled=False, silence_floor_db=-40.0)
    orchestrator.seal_path_runtime = FakeSealPathRuntime()

    async def fake_collect_interim_text_updates(
        _session_id: UUID, _ready_chunks: list[np.ndarray]
    ) -> list[str]:
        return []

    orchestrator._collect_interim_text_updates = fake_collect_interim_text_updates
    return cast(SessionOrchestrator, orchestrator)


def _stream_runtime(orchestrator: SessionOrchestrator) -> StreamPathRuntime:
    return orchestrator._stream_path_runtime_for_runtime()


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
        assert (
            _stream_runtime(orchestrator).facts(active_session=True).fallback_reason
            == "init_failed:ImportError"
        )
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


def test_stop_empty_capture_result_emits_audio_device_error() -> None:
    async def scenario() -> None:
        orchestrator = _build_orchestrator(samples=np.zeros((0,), dtype=np.float32))
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

        errors = _events_of_type(sink, SessionErrorEvent)
        assert errors
        assert errors[-1].code == "AUDIO_DEVICE"
        ended = _events_of_type(sink, SessionEndedEvent)
        assert ended
        assert ended[-1].reason == SessionEndReason.ERROR
        assert _events_of_type(sink, FinalResultEvent) == []

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


def test_phase6_resource_limit_soak_stops_retaining_audio_and_emits_limit_signal(
    monkeypatch: MonkeyPatch,
) -> None:
    async def scenario() -> None:
        sample_rate = 16_000
        sample_cap = 64
        chunk_frames = 16
        pre_start_chunks = 10
        post_start_chunks = 50
        audio = RecordingAudioInput(
            sample_rate=sample_rate,
            channels=1,
            pre_roll_seconds=0.002,
            max_session_samples=sample_cap,
        )
        pre_roll_chunk = np.ones((chunk_frames, 1), dtype=np.float32)
        for _ in range(pre_start_chunks):
            audio._callback(
                pre_roll_chunk,
                frames=chunk_frames,
                time=None,
                status=cast(Any, 0),
            )
        orchestrator = _build_orchestrator(
            max_session_seconds=90.0,
            max_session_samples=sample_cap,
            audio=audio,
        )
        seal_runtime = cast(FakeSealPathRuntime, orchestrator.seal_path_runtime)
        sink = RecordingEventSink()
        session_id = uuid4()
        limit_exceeded_after_feed = False

        async def fake_guard_sleep(_seconds: float) -> None:
            nonlocal limit_exceeded_after_feed
            chunk = np.ones((chunk_frames, 1), dtype=np.float32)
            for _ in range(post_start_chunks):
                audio._callback(chunk, frames=chunk_frames, time=None, status=cast(Any, 0))
            limit_exceeded_after_feed = audio.session_limit_exceeded()
            await asyncio.sleep(0)

        monkeypatch.setattr(orchestrator_module, "_REAL_ASYNCIO_SLEEP", fake_guard_sleep)

        await _start(orchestrator, sink, session_id)
        for _ in range(20):
            if orchestrator.sessions.active is None:
                break
            await asyncio.sleep(0)

        warnings = _events_of_type(sink, SessionWarningEvent)
        finals = _events_of_type(sink, FinalResultEvent)
        ended = _events_of_type(sink, SessionEndedEvent)
        capture_result = audio.last_capture_result
        assert capture_result is not None
        retained_samples = int(seal_runtime.finalize_calls[-1].size)
        evidence = {
            "active_session_after_guard": orchestrator.sessions.active is not None,
            "fed_samples": (pre_start_chunks + post_start_chunks) * chunk_frames,
            "limit_exceeded_after_feed": limit_exceeded_after_feed,
            "post_start_fed_samples": post_start_chunks * chunk_frames,
            "post_start_samples": capture_result.post_start_samples,
            "pre_roll_samples": capture_result.pre_roll_samples,
            "retained_samples": retained_samples,
            "sample_cap": sample_cap,
            "session_end_reason": str(ended[-1].reason) if ended else None,
            "warning_count": len(warnings),
        }
        print("resource_limit_soak_evidence=" + json.dumps(evidence, sort_keys=True))

        assert evidence["fed_samples"] > sample_cap
        assert limit_exceeded_after_feed is True
        assert capture_result.pre_roll_samples == 32
        assert capture_result.post_start_samples == 32
        assert retained_samples == sample_cap
        assert capture_result.captured_samples == sample_cap
        assert orchestrator.sessions.active is None
        assert warnings
        assert warnings[-1].warning == "approaching_limit"
        assert finals
        assert finals[-1].audio_ms == int((sample_cap * 1000) / sample_rate)
        assert ended
        assert ended[-1].reason == SessionEndReason.FINAL

    asyncio.run(scenario())


def test_phase6_stream_seal_live_interim_overlap_waits_for_in_flight_only() -> None:
    async def scenario() -> None:
        probe = InferenceOverlapProbe()
        ready_live_chunks = [
            np.full((400,), 0.2, dtype=np.float32),
            np.full((400,), 0.3, dtype=np.float32),
            np.full((400,), 0.4, dtype=np.float32),
        ]
        orchestrator = _build_orchestrator(
            streaming_enabled=True,
            stream_chunks=ready_live_chunks,
        )
        cast(Any, orchestrator).streaming_transcriber = FakeStreamingTranscriber(
            helper_active=False,
            fallback_reason="init_failed:stress",
        )
        tail_trimmer = SealPathTailTrimmer(vad_enabled=False, silence_floor_db=-40.0)
        orchestrator.tail_trimmer = tail_trimmer
        orchestrator.seal_path_runtime = SealPathRuntime(
            sample_rate=16_000,
            tail_trimmer=tail_trimmer,
            release_device_cache=lambda _device: None,
        )
        transcriber = LiveFinalOverlapTranscriber(probe)
        cast(Any, orchestrator).transcriber = transcriber
        sink = RecordingEventSink()
        session_id = uuid4()

        await _start(orchestrator, sink, session_id)
        assert await asyncio.to_thread(probe.live_started.wait, 1.0)

        stop_task = asyncio.create_task(
            orchestrator.stop(
                StopSessionIntent(
                    session_id=session_id,
                    owner_token=1,
                    event_sink=sink,
                    post_roll_secs=0.0,
                )
            )
        )
        for _ in range(20):
            if probe.final_started.is_set():
                break
            await asyncio.sleep(0)
        final_started_before_live_finished = probe.final_started.is_set()
        probe.release_live.set()
        await stop_task

        truth = orchestrator.runtime_truth(overlay_events_enabled=True)
        evidence = {
            "calls": transcriber.calls,
            "final_started_before_live_finished": final_started_before_live_finished,
            "live_chunks_processed": truth.interim_transcript_live_chunks_processed,
            "max_active_calls": probe.max_active_calls,
            "order": probe.order,
            "queued_live_chunks": len(ready_live_chunks)
            - int(truth.interim_transcript_live_chunks_processed or 0),
            "stop_replay_chunks_processed": truth.interim_transcript_stop_replay_chunks_processed,
        }
        print("inference_overlap_live_evidence=" + json.dumps(evidence, sort_keys=True))

        assert final_started_before_live_finished is False
        assert probe.order == ["live:start", "live:finish", "final:start", "final:finish"]
        assert probe.max_active_calls == 1
        assert transcriber.calls == 2
        assert truth.interim_transcript_live_chunks_processed == 1
        assert truth.interim_transcript_stop_replay_chunks_processed == 0
        assert _events_of_type(sink, FinalResultEvent)
        ended = _events_of_type(sink, SessionEndedEvent)
        assert ended
        assert ended[-1].reason == SessionEndReason.FINAL

    asyncio.run(scenario())


def test_stream_feed_cancellation_keeps_inference_gate_until_feed_finishes() -> None:
    async def scenario() -> None:
        probe = InferenceOverlapProbe()
        orchestrator = _build_orchestrator(
            streaming_enabled=True,
            stream_chunks=[np.full((400,), 0.2, dtype=np.float32)],
        )
        streaming_transcriber = BlockingStreamingTranscriber(probe)
        cast(Any, orchestrator).streaming_transcriber = streaming_transcriber
        tail_trimmer = SealPathTailTrimmer(vad_enabled=False, silence_floor_db=-40.0)
        orchestrator.tail_trimmer = tail_trimmer
        orchestrator.seal_path_runtime = SealPathRuntime(
            sample_rate=16_000,
            tail_trimmer=tail_trimmer,
            release_device_cache=lambda _device: None,
        )
        cast(Any, orchestrator).transcriber = FinalOnlyOverlapTranscriber(probe)
        sink = RecordingEventSink()
        session_id = uuid4()

        await _start(orchestrator, sink, session_id)
        assert await asyncio.to_thread(probe.stream_feed_started.wait, 1.0)

        stop_task = asyncio.create_task(
            orchestrator.stop(
                StopSessionIntent(
                    session_id=session_id,
                    owner_token=1,
                    event_sink=sink,
                    post_roll_secs=0.0,
                )
            )
        )
        try:
            for _ in range(50):
                if probe.final_started.is_set():
                    break
                await asyncio.sleep(0.001)
            final_started_before_stream_feed_finished = probe.final_started.is_set()
        finally:
            probe.release_stream_feed.set()
            await stop_task

        evidence = {
            "final_started_before_stream_feed_finished": (
                final_started_before_stream_feed_finished
            ),
            "max_active_calls": probe.max_active_calls,
            "order": probe.order,
            "stream_feed_calls": streaming_transcriber.stream_session.feed_calls,
        }
        print("inference_overlap_stream_feed_evidence=" + json.dumps(evidence, sort_keys=True))

        assert final_started_before_stream_feed_finished is False
        assert probe.order == [
            "stream_feed:start",
            "stream_feed:finish",
            "final:start",
            "final:finish",
        ]
        assert probe.max_active_calls == 1
        assert streaming_transcriber.stream_session.feed_calls == 1
        assert _events_of_type(sink, FinalResultEvent)

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
