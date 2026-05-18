"""Streaming helper truth signals: active/fallback state transitions."""

from __future__ import annotations

import asyncio
from typing import Any, cast
from uuid import uuid4

import numpy as np

from parakeet_stt_daemon.config import ServerSettings
from parakeet_stt_daemon.messages import StatusMessage
from parakeet_stt_daemon.session import SessionManager
from parakeet_stt_daemon.session_orchestrator import SessionOrchestrator
from parakeet_stt_daemon.tail_trim import SealPathTailTrimmer


class FakeAudio:
    sample_rate = 16_000

    def __init__(self) -> None:
        self.abort_calls = 0
        self.start_calls = 0

    def start_session(self) -> None:
        self.start_calls += 1

    def abort_session(self) -> None:
        self.abort_calls += 1

    def take_stream_chunks(self) -> list[object]:
        return []


class FakeStreamingTranscriber:
    def __init__(
        self,
        *,
        helper_active: bool = True,
        fallback_reason: str | None = None,
        helper_class_name: str | None = None,
    ) -> None:
        self.helper_active = helper_active
        self.fallback_reason = fallback_reason
        self._helper_class_name = helper_class_name

    def start_session(self, _sample_rate: int) -> object:
        return object()


class FakeVadAdapter:
    def __init__(self, *, prepare_error: Exception | None = None) -> None:
        self.prepare_error = prepare_error

    def prepare(self) -> None:
        if self.prepare_error is not None:
            raise self.prepare_error

    def trim(self, samples: np.ndarray, sample_rate: int) -> np.ndarray:
        del sample_rate
        return samples.astype(np.float32, copy=False)


def _build_server(
    *,
    streaming_enabled: bool = True,
    streaming_transcriber: Any = None,
    vad_enabled: bool = False,
) -> SessionOrchestrator:
    orchestrator = cast(Any, SessionOrchestrator.__new__(SessionOrchestrator))
    orchestrator.settings = ServerSettings(
        device="cpu",
        status_enabled=True,
        streaming_enabled=streaming_enabled,
        vad_enabled=vad_enabled,
    )
    orchestrator.sessions = SessionManager()
    orchestrator.audio = FakeAudio()
    orchestrator.model = object()
    orchestrator.transcriber = object()
    orchestrator._session_lock = asyncio.Lock()
    orchestrator._inference_lock = asyncio.Lock()
    orchestrator.streaming_transcriber = streaming_transcriber
    orchestrator._active_stream = None
    orchestrator._stream_drain_task = None
    orchestrator._stream_drain_running = False
    orchestrator._requested_device = "cpu"
    orchestrator._effective_device = "cpu"
    orchestrator._last_audio_ms = None
    orchestrator._last_audio_stop_ms = None
    orchestrator._last_finalize_ms = None
    orchestrator._last_infer_ms = None
    orchestrator._last_send_ms = None
    orchestrator._live_interim_audio = np.zeros((0,), dtype=np.float32)
    orchestrator._live_interim_failed = False
    orchestrator._overlay_interim_stabilizer_by_session = {}
    orchestrator._vad_enabled = vad_enabled
    orchestrator.tail_trimmer = SealPathTailTrimmer(
        vad_enabled=vad_enabled,
        silence_floor_db=float(orchestrator.settings.silence_floor_db),
        vad_adapter=FakeVadAdapter(),
        warmup_sample_rate=FakeAudio.sample_rate,
    )
    return cast(SessionOrchestrator, orchestrator)


def _status(orchestrator: SessionOrchestrator) -> StatusMessage:
    return orchestrator.status(
        overlay_events_enabled=orchestrator.settings.overlay_events_enabled,
        overlay_events_emitted=0,
        overlay_events_dropped=0,
    )


def test_status_streaming_disabled_by_config() -> None:
    """When streaming is disabled by config, helper fields reflect that."""
    server = _build_server(streaming_enabled=False)

    status = _status(server)

    assert status.streaming_enabled is False
    assert status.stream_helper_active is False
    assert status.stream_helper_scope == "live_session_only"
    assert status.stream_fallback_reason is None
    assert status.finalization_mode == "offline_seal"
    assert status.final_audio_source == "canonical_session_audio"
    assert status.tail_trim_mode == "rms"
    assert status.chunk_secs is None
    assert status.vad_enabled is False
    assert status.vad_active is False
    assert status.vad_fallback_reason is None


def test_status_streaming_enabled_helper_active() -> None:
    """When streaming is enabled and helper initialized, truth is active."""
    transcriber = FakeStreamingTranscriber(
        helper_active=True,
        helper_class_name="FrameBatchChunkedRNNT",
    )
    server = _build_server(streaming_transcriber=transcriber)

    status = _status(server)

    assert status.streaming_enabled is True
    assert status.stream_helper_active is True
    assert status.stream_helper_scope == "live_session_only"
    assert status.stream_fallback_reason is None
    assert status.finalization_mode == "offline_seal"
    assert status.final_audio_source == "canonical_session_audio"
    assert status.tail_trim_mode == "rms"


def test_status_vad_enabled_pending_load_is_explicit() -> None:
    server = _build_server(streaming_enabled=False, vad_enabled=True)

    status = _status(server)

    assert status.vad_enabled is True
    assert status.vad_active is False
    assert status.vad_fallback_reason == "load_not_attempted"
    assert status.tail_trim_mode == "rms"


def test_status_vad_enabled_and_loaded_is_active() -> None:
    server = _build_server(streaming_enabled=False, vad_enabled=True)
    server.prepare_vad()

    status = _status(server)

    assert status.vad_enabled is True
    assert status.vad_active is True
    assert status.vad_fallback_reason is None
    assert status.tail_trim_mode == "vad"


def test_prepare_vad_marks_missing_dependency_explicitly() -> None:
    server = _build_server(streaming_enabled=False, vad_enabled=True)
    exc = ModuleNotFoundError("No module named 'onnxruntime'")
    exc.name = "onnxruntime"
    server.tail_trimmer = SealPathTailTrimmer(
        vad_enabled=True,
        silence_floor_db=float(server.settings.silence_floor_db),
        vad_adapter=FakeVadAdapter(prepare_error=exc),
        warmup_sample_rate=FakeAudio.sample_rate,
    )

    server.prepare_vad()

    assert server._vad_active() is False
    assert server._vad_fallback_reason() == "load_failed:missing_dependency:onnxruntime"


def test_status_streaming_enabled_helper_inactive() -> None:
    """When streaming is enabled but helper failed, truth shows fallback."""
    transcriber = FakeStreamingTranscriber(
        helper_active=False,
        fallback_reason="import_failed:ImportError",
    )
    server = _build_server(streaming_transcriber=transcriber)

    status = _status(server)

    assert status.streaming_enabled is True
    assert status.stream_helper_active is False
    assert status.stream_helper_scope == "live_session_only"
    assert status.stream_fallback_reason == "import_failed:ImportError"
    assert status.finalization_mode == "offline_seal"
    assert status.final_audio_source == "canonical_session_audio"


def test_status_streaming_enabled_transcriber_none() -> None:
    """When streaming_transcriber is None despite enabled config."""
    server = _build_server(streaming_transcriber=None)

    status = _status(server)

    assert status.streaming_enabled is True
    assert status.stream_helper_active is False
    assert status.stream_helper_scope == "live_session_only"
    assert status.stream_fallback_reason == "streaming_transcriber_unavailable"


def test_stream_helper_active_reflects_transcriber_state() -> None:
    """_stream_helper_active() delegates to transcriber.helper_active."""
    transcriber = FakeStreamingTranscriber(helper_active=True)
    server = _build_server(streaming_transcriber=transcriber)
    assert server._stream_helper_active() is True

    transcriber.helper_active = False
    assert server._stream_helper_active() is False


def test_stream_fallback_reason_init_failed() -> None:
    """Fallback reason captures the init failure class."""
    transcriber = FakeStreamingTranscriber(
        helper_active=False,
        fallback_reason="init_failed:RuntimeError",
    )
    server = _build_server(streaming_transcriber=transcriber)

    assert server._stream_fallback_reason() == "init_failed:RuntimeError"


def test_stream_fallback_reason_none_when_active() -> None:
    """No fallback reason when helper is active."""
    transcriber = FakeStreamingTranscriber(helper_active=True)
    server = _build_server(streaming_transcriber=transcriber)

    assert server._stream_fallback_reason() is None


def test_status_includes_active_session_age_when_session_active() -> None:
    """active_session_age_ms is populated when a session exists."""

    async def scenario() -> None:
        server = _build_server(streaming_enabled=False)
        session_id = uuid4()
        await server.sessions.start_session(session_id, owner_token=1)

        status = _status(server)
        assert status.active_session_age_ms is not None
        assert status.active_session_age_ms >= 0

    asyncio.run(scenario())


def test_status_no_active_session_age_when_idle() -> None:
    """active_session_age_ms is None when no session is active."""
    server = _build_server(streaming_enabled=False)

    status = _status(server)
    assert status.active_session_age_ms is None


def test_status_last_timings_none_before_first_session() -> None:
    """Timing fields are None before any session completes."""
    server = _build_server(streaming_enabled=False)

    status = _status(server)
    assert status.audio_stop_ms is None
    assert status.finalize_ms is None
    assert status.infer_ms is None
    assert status.send_ms is None
    assert status.last_audio_ms is None
    assert status.last_infer_ms is None
    assert status.last_send_ms is None


def test_status_last_timings_populated_after_session() -> None:
    """Timing fields reflect last session values."""
    server = _build_server(streaming_enabled=False)
    server._last_audio_ms = 2500
    server._last_audio_stop_ms = 12
    server._last_finalize_ms = 180
    server._last_infer_ms = 120
    server._last_send_ms = 3

    status = _status(server)
    assert status.audio_stop_ms == 12
    assert status.finalize_ms == 180
    assert status.infer_ms == 120
    assert status.send_ms == 3
    assert status.last_audio_ms == 2500
    assert status.last_infer_ms == 120
    assert status.last_send_ms == 3
