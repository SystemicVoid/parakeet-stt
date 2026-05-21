"""Streaming helper truth signals: active/fallback state transitions."""

from __future__ import annotations

import asyncio
from types import SimpleNamespace
from typing import Any, cast
from uuid import uuid4

import numpy as np

from parakeet_stt_daemon.config import ServerSettings
from parakeet_stt_daemon.messages import StatusMessage
from parakeet_stt_daemon.runtime_truth_snapshot import RuntimeTruth, snapshot
from parakeet_stt_daemon.session import (
    Session,
    SessionManager,
    StreamPathRuntime,
    StreamPathRuntimeFacts,
)
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


class StreamPrivateFieldTrap(SimpleNamespace):
    def __getattr__(self, name: str) -> Any:
        private_stream_prefixes = ("_current_" + "stream", "_last_" + "stream")
        if name.startswith(private_stream_prefixes):
            raise AssertionError(f"runtime truth probed old Stream path field {name}")
        raise AttributeError(name)


def _build_server(
    *,
    streaming_enabled: bool = True,
    streaming_transcriber: Any = None,
    overlay_events_enabled: bool = False,
    vad_enabled: bool = False,
) -> SessionOrchestrator:
    orchestrator = cast(Any, SessionOrchestrator.__new__(SessionOrchestrator))
    orchestrator.settings = ServerSettings(
        device="cpu",
        status_enabled=True,
        streaming_enabled=streaming_enabled,
        overlay_events_enabled=overlay_events_enabled,
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
    orchestrator.stream_path_runtime = StreamPathRuntime.from_settings(
        orchestrator.settings,
        streaming_transcriber,
    )
    orchestrator._last_interim_transcript_runtime_facts = None
    orchestrator._requested_device = "cpu"
    orchestrator._effective_device = "cpu"
    orchestrator._last_audio_ms = None
    orchestrator._last_audio_stop_ms = None
    orchestrator._last_finalize_ms = None
    orchestrator._last_infer_ms = None
    orchestrator._last_send_ms = None
    orchestrator._interim_transcript_by_session = {}
    orchestrator._vad_enabled = vad_enabled
    orchestrator.tail_trimmer = SealPathTailTrimmer(
        vad_enabled=vad_enabled,
        silence_floor_db=float(orchestrator.settings.silence_floor_db),
        vad_adapter=FakeVadAdapter(),
        warmup_sample_rate=FakeAudio.sample_rate,
    )
    return cast(SessionOrchestrator, orchestrator)


def _status(orchestrator: SessionOrchestrator) -> StatusMessage:
    truth = _truth(orchestrator)
    return truth.to_status(
        orchestrator.runtime_status_state(),
        orchestrator.runtime_status_metrics(
            overlay_events_emitted=0,
            overlay_events_dropped=0,
        ),
    )


def _truth(orchestrator: SessionOrchestrator) -> RuntimeTruth:
    return snapshot(
        orchestrator,
        last_trim_outcome=orchestrator.tail_trimmer.last_outcome,
        overlay_events_enabled=orchestrator.settings.overlay_events_enabled,
    )


def test_status_streaming_disabled_by_config() -> None:
    """When streaming is disabled by config, helper fields reflect that."""
    server = _build_server(streaming_enabled=False)

    status = _status(server)

    assert status.streaming_enabled is False
    assert status.stream_helper_active is False
    assert status.stream_helper_scope == "live_session_only"
    assert status.stream_fallback_reason is None
    assert status.stream_path_executed is False
    assert status.stream_chunks_processed == 0
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
    assert status.stream_path_executed is False
    assert status.stream_chunks_processed == 0
    assert status.finalization_mode == "offline_seal"
    assert status.final_audio_source == "canonical_session_audio"
    assert status.tail_trim_mode == "rms"


def test_runtime_truth_log_record_contains_helper_expected_fields() -> None:
    transcriber = FakeStreamingTranscriber(helper_active=True, helper_class_name="FakeHelper")
    server = _build_server(streaming_transcriber=transcriber)

    truth = _truth(server)
    status = _status(server)
    log_record = truth.to_log_record()

    assert status.model_dump(include=set(_helper_expected_status_fields())) == {
        "device": "cpu",
        "effective_device": "cpu",
        "streaming_enabled": True,
        "stream_helper_active": True,
        "stream_helper_scope": "live_session_only",
        "stream_fallback_reason": None,
        "stream_path_executed": False,
        "stream_chunks_processed": 0,
        "finalization_mode": "offline_seal",
        "final_audio_source": "canonical_session_audio",
        "tail_trim_mode": "rms",
        "vad_enabled": False,
        "vad_active": False,
        "vad_fallback_reason": None,
        "interim_transcript_enabled": False,
        "interim_transcript_last_source": None,
        "interim_transcript_live_chunks_processed": 0,
        "interim_transcript_stop_replay_chunks_processed": 0,
        "interim_transcript_updates_emitted": 0,
        "interim_transcript_live_updates_emitted": 0,
        "interim_transcript_stop_replay_updates_emitted": 0,
        "interim_transcript_live_failed": False,
        "interim_transcript_stop_replay_failed": False,
        "interim_transcript_source_fallback_reason": None,
        "overlay_events_enabled": False,
    }
    assert log_record["live_session_helper_active"] is True
    assert log_record["live_session_helper_class"] == "FakeHelper"
    assert log_record["stream_path_executed"] is False
    assert log_record["stream_chunks_processed"] == 0
    assert log_record["finalization_mode"] == "offline_seal"
    assert log_record["tail_trim_mode"] == "rms"
    assert log_record["interim_transcript_enabled"] is False
    assert log_record["interim_transcript_updates_emitted"] == 0


def test_runtime_truth_preserves_missing_optional_device_and_chunk_values() -> None:
    orchestrator = cast(
        SessionOrchestrator,
        SimpleNamespace(
            settings=SimpleNamespace(
                streaming_enabled=True, chunk_secs=None, overlay_events_enabled=False
            ),
            streaming_transcriber=None,
        ),
    )
    truth = snapshot(
        orchestrator,
        last_trim_outcome=SimpleNamespace(
            tail_trim_mode="rms",
            vad_active=False,
            vad_fallback_reason=None,
        ),
    )

    assert truth.device is None
    assert truth.effective_device is None
    assert truth.chunk_secs is None
    assert truth.stream_fallback_reason == "streaming_transcriber_unavailable"


def test_runtime_truth_reads_stream_path_interface_instead_of_orchestrator_fields() -> None:
    runtime_facts = StreamPathRuntimeFacts(
        streaming_enabled=True,
        helper_active=True,
        helper_scope="live_session_only",
        helper_class_name="InterfaceHelper",
        fallback_reason="interface:fallback",
        chunk_secs=1.25,
        path_executed=True,
        chunks_processed=3,
    )
    orchestrator = cast(
        SessionOrchestrator,
        StreamPrivateFieldTrap(
            settings=SimpleNamespace(
                streaming_enabled=True,
                chunk_secs=9.99,
                overlay_events_enabled=False,
            ),
            streaming_transcriber=None,
            stream_path_runtime_facts_for_runtime=lambda: runtime_facts,
        ),
    )

    truth = snapshot(
        orchestrator,
        last_trim_outcome=SimpleNamespace(
            tail_trim_mode="rms",
            vad_active=False,
            vad_fallback_reason=None,
        ),
    )

    assert truth.stream_helper_active is True
    assert truth.stream_helper_class_name == "InterfaceHelper"
    assert truth.stream_path_executed is True
    assert truth.stream_chunks_processed == 3
    assert truth.stream_fallback_reason == "interface:fallback"
    assert truth.chunk_secs == 1.25


def test_runtime_truth_ignores_invalid_chunk_secs_values() -> None:
    for chunk_secs in ("not-a-number", "nan", "inf", float("nan"), float("inf"), True):
        orchestrator = cast(
            SessionOrchestrator,
            SimpleNamespace(
                settings=SimpleNamespace(
                    streaming_enabled=True,
                    chunk_secs=chunk_secs,
                    overlay_events_enabled=False,
                ),
                streaming_transcriber=None,
            ),
        )

        truth = snapshot(
            orchestrator,
            last_trim_outcome=SimpleNamespace(
                tail_trim_mode="rms",
                vad_active=False,
                vad_fallback_reason=None,
            ),
        )

        assert truth.chunk_secs is None


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

    truth = _truth(server)
    assert truth.vad_active is False
    assert truth.vad_fallback_reason == "load_failed:missing_dependency:onnxruntime"


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
    assert status.stream_path_executed is False
    assert status.stream_chunks_processed == 0
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
    assert status.stream_path_executed is False
    assert status.stream_chunks_processed == 0


def test_stream_helper_active_reflects_transcriber_state() -> None:
    """RuntimeTruth delegates Stream path activity to transcriber.helper_active."""
    transcriber = FakeStreamingTranscriber(helper_active=True)
    server = _build_server(streaming_transcriber=transcriber)
    assert _truth(server).stream_helper_active is True

    transcriber.helper_active = False
    assert _truth(server).stream_helper_active is False


def test_stream_fallback_reason_init_failed() -> None:
    """Fallback reason captures the init failure class."""
    transcriber = FakeStreamingTranscriber(
        helper_active=False,
        fallback_reason="init_failed:RuntimeError",
    )
    server = _build_server(streaming_transcriber=transcriber)

    assert _truth(server).stream_fallback_reason == "init_failed:RuntimeError"


def test_stream_fallback_reason_none_when_active() -> None:
    """No fallback reason when helper is active."""
    transcriber = FakeStreamingTranscriber(helper_active=True)
    server = _build_server(streaming_transcriber=transcriber)

    assert _truth(server).stream_fallback_reason is None


def test_status_reports_stream_path_execution_from_last_session() -> None:
    transcriber = FakeStreamingTranscriber(helper_active=True)
    server = _build_server(streaming_transcriber=transcriber)
    server.stream_path_runtime.record_chunk_processed()
    server.stream_path_runtime.record_chunk_processed()
    server.stream_path_runtime.record_session_result(active_stream=None)

    status = _status(server)
    log_record = _truth(server).to_log_record()

    assert status.streaming_enabled is True
    assert status.stream_helper_active is True
    assert status.stream_path_executed is True
    assert status.stream_chunks_processed == 2
    assert status.stream_fallback_reason is None
    assert log_record["stream_path_executed"] is True
    assert log_record["stream_chunks_processed"] == 2


def test_status_reports_fallback_when_helper_exists_but_no_stream_work_ran() -> None:
    transcriber = FakeStreamingTranscriber(helper_active=True)
    server = _build_server(streaming_transcriber=transcriber)
    server.stream_path_runtime.record_session_result(active_stream=None)

    status = _status(server)
    log_record = _truth(server).to_log_record()

    assert status.streaming_enabled is True
    assert status.stream_helper_active is True
    assert status.stream_path_executed is False
    assert status.stream_chunks_processed == 0
    assert status.stream_fallback_reason == "stream_path_not_exercised:no_chunks"
    assert log_record["stream_fallback_reason"] == "stream_path_not_exercised:no_chunks"


def test_status_reports_fallback_even_after_stream_path_executed() -> None:
    transcriber = FakeStreamingTranscriber(helper_active=True)
    server = _build_server(streaming_transcriber=transcriber)
    server.stream_path_runtime.record_chunk_processed()
    server.stream_path_runtime.record_current_fallback("stream_chunk_failed:RuntimeError")
    server.stream_path_runtime.record_session_result(active_stream=None)

    status = _status(server)
    truth = _truth(server)

    assert status.stream_path_executed is True
    assert status.stream_chunks_processed == 1
    assert status.stream_fallback_reason == "stream_chunk_failed:RuntimeError"
    assert truth.degraded is True


def test_status_and_logs_split_interim_truth_from_stream_path_fallback() -> None:
    async def scenario() -> None:
        transcriber = FakeStreamingTranscriber(
            helper_active=False,
            fallback_reason="streaming_helper_inactive",
        )
        server = _build_server(
            streaming_transcriber=transcriber,
            overlay_events_enabled=True,
        )
        session_id = uuid4()
        await server.sessions.start_session(session_id, owner_token=1)
        server._reset_interim_transcript_session(session_id)

        async def transcribe(_samples: np.ndarray) -> str:
            return "visible interim text"

        update = await server._interim_transcript_session(session_id).accept_live_chunk(
            np.full((400,), 0.2, dtype=np.float32),
            transcribe,
        )
        assert update == "visible interim text"

        status = _status(server)
        log_record = _truth(server).to_log_record()

        assert status.streaming_enabled is True
        assert status.stream_helper_active is False
        assert status.stream_path_executed is False
        assert status.stream_chunks_processed == 0
        assert status.stream_fallback_reason == "streaming_helper_inactive"
        assert status.overlay_events_emitted == 0
        assert status.overlay_events_dropped == 0
        assert status.interim_transcript_enabled is True
        assert status.interim_transcript_last_source == "live"
        assert status.interim_transcript_live_chunks_processed == 1
        assert status.interim_transcript_stop_replay_chunks_processed == 0
        assert status.interim_transcript_updates_emitted == 1
        assert status.interim_transcript_live_updates_emitted == 1
        assert status.interim_transcript_stop_replay_updates_emitted == 0
        assert status.interim_transcript_source_fallback_reason is None
        assert log_record["stream_path_executed"] is False
        assert log_record["stream_chunks_processed"] == 0
        assert log_record["stream_fallback_reason"] == "streaming_helper_inactive"
        assert log_record["interim_transcript_enabled"] is True
        assert log_record["interim_transcript_last_source"] == "live"
        assert log_record["interim_transcript_updates_emitted"] == 1

        server._record_last_interim_transcript_runtime(session_id)
        await server.sessions.clear(session_id, owner_token=1)
        server._clear_overlay_session_runtime(session_id)

        idle_status = _status(server)
        idle_log_record = _truth(server).to_log_record()

        assert idle_status.interim_transcript_last_source == "live"
        assert idle_status.interim_transcript_updates_emitted == 1
        assert idle_log_record["interim_transcript_last_source"] == "live"
        assert idle_log_record["interim_transcript_updates_emitted"] == 1

    asyncio.run(scenario())


def test_status_reports_interim_source_fallback_reason() -> None:
    async def scenario() -> None:
        server = _build_server(streaming_enabled=False, overlay_events_enabled=True)
        session_id = uuid4()
        await server.sessions.start_session(session_id, owner_token=1)
        server._reset_interim_transcript_session(session_id)

        async def transcribe(_samples: np.ndarray) -> str:
            raise RuntimeError("interim source failed")

        update = await server._interim_transcript_session(session_id).accept_live_chunk(
            np.full((400,), 0.2, dtype=np.float32),
            transcribe,
        )

        assert update is None
        status = _status(server)
        log_record = _truth(server).to_log_record()

        assert status.interim_transcript_enabled is True
        assert status.interim_transcript_live_chunks_processed == 1
        assert status.interim_transcript_updates_emitted == 0
        assert status.interim_transcript_live_failed is True
        assert status.interim_transcript_stop_replay_failed is False
        assert (
            status.interim_transcript_source_fallback_reason == "live_transcribe_error:RuntimeError"
        )
        assert log_record["interim_transcript_live_failed"] is True
        assert log_record["interim_transcript_stop_replay_failed"] is False
        assert (
            log_record["interim_transcript_source_fallback_reason"]
            == "live_transcribe_error:RuntimeError"
        )

    asyncio.run(scenario())


def test_active_session_does_not_inherit_previous_stream_fallback() -> None:
    transcriber = FakeStreamingTranscriber(helper_active=True)
    server = _build_server(streaming_transcriber=transcriber)
    server.stream_path_runtime.record_session_result(active_stream=None)
    server.stream_path_runtime.reset_current()
    cast(Any, server.sessions)._active = Session(session_id=uuid4(), owner_token=1)

    status = _status(server)

    assert status.stream_path_executed is False
    assert status.stream_chunks_processed == 0
    assert status.stream_fallback_reason is None


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


def _helper_expected_status_fields() -> tuple[str, ...]:
    return (
        "device",
        "effective_device",
        "streaming_enabled",
        "stream_helper_active",
        "stream_helper_scope",
        "stream_fallback_reason",
        "stream_path_executed",
        "stream_chunks_processed",
        "finalization_mode",
        "final_audio_source",
        "tail_trim_mode",
        "vad_enabled",
        "vad_active",
        "vad_fallback_reason",
        "interim_transcript_enabled",
        "interim_transcript_last_source",
        "interim_transcript_live_chunks_processed",
        "interim_transcript_stop_replay_chunks_processed",
        "interim_transcript_updates_emitted",
        "interim_transcript_live_updates_emitted",
        "interim_transcript_stop_replay_updates_emitted",
        "interim_transcript_live_failed",
        "interim_transcript_stop_replay_failed",
        "interim_transcript_source_fallback_reason",
        "overlay_events_enabled",
    )
