"""Streaming helper truth signals: active/fallback state transitions."""

from __future__ import annotations

import asyncio
from types import SimpleNamespace
from typing import Any, cast
from uuid import uuid4

import numpy as np

from parakeet_stt_daemon.config import ServerSettings
from parakeet_stt_daemon.messages import StatusMessage
from parakeet_stt_daemon.runtime_truth_snapshot import (
    DeviceInfo,
    OverlayTransportFacts,
    RuntimeTruth,
    SealPathFacts,
    StreamPathFacts,
    TailTrimFacts,
    snapshot,
)
from parakeet_stt_daemon.session import (
    InterimTranscriptRuntime,
    InterimTranscriptRuntimeFacts,
    SealPathFinalizationResult,
    SealPathRuntime,
    Session,
    SessionManager,
    StreamPathRuntime,
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


class RuntimeTruthSourceTrap:
    def __init__(
        self,
        *,
        device_info: DeviceInfo | None = None,
        stream_path: StreamPathFacts | None = None,
        seal_path: SealPathFacts | None = None,
        tail_trim: TailTrimFacts | None = None,
        interim: InterimTranscriptRuntimeFacts | None = None,
        overlay_transport: OverlayTransportFacts | None = None,
    ) -> None:
        self._device_info = device_info or DeviceInfo(
            requested_device="cpu",
            effective_device="cpu",
        )
        self._stream_path = stream_path or StreamPathFacts(
            streaming_enabled=False,
            helper_active=False,
            helper_scope="live_session_only",
            helper_class_name=None,
            fallback_reason=None,
            chunk_secs=None,
            path_executed=False,
            chunks_processed=0,
        )
        self._seal_path = seal_path or SealPathFacts()
        self._tail_trim = tail_trim or TailTrimFacts(
            tail_trim_mode="rms",
            vad_enabled=False,
            vad_active=False,
            vad_fallback_reason=None,
        )
        self._interim = interim or InterimTranscriptRuntimeFacts(
            enabled=False,
            last_source=None,
            live_chunks_processed=0,
            live_updates_emitted=0,
            live_failed=False,
            stop_replay_chunks_processed=0,
            stop_replay_updates_emitted=0,
            stop_replay_failed=False,
            source_fallback_reason=None,
        )
        self._overlay_transport = overlay_transport or OverlayTransportFacts(enabled=False)

    def __getattr__(self, name: str) -> Any:
        raise AssertionError(f"runtime truth probed non-interface field {name}")

    def runtime_truth_device_info(self) -> DeviceInfo:
        return self._device_info

    def runtime_truth_stream_path_facts(self) -> StreamPathFacts:
        return self._stream_path

    def runtime_truth_seal_path_facts(self) -> SealPathFacts:
        return self._seal_path

    def runtime_truth_tail_trim_facts(self) -> TailTrimFacts:
        return self._tail_trim

    def runtime_truth_interim_transcript_facts(self) -> InterimTranscriptRuntimeFacts:
        return self._interim

    def runtime_truth_overlay_transport_facts(self) -> OverlayTransportFacts:
        return self._overlay_transport


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
    orchestrator._requested_device = "cpu"
    orchestrator._effective_device = "cpu"
    orchestrator._vad_enabled = vad_enabled
    orchestrator.tail_trimmer = SealPathTailTrimmer(
        vad_enabled=vad_enabled,
        silence_floor_db=float(orchestrator.settings.silence_floor_db),
        vad_adapter=FakeVadAdapter(),
        warmup_sample_rate=FakeAudio.sample_rate,
    )
    orchestrator.seal_path_runtime = SealPathRuntime(
        sample_rate=FakeAudio.sample_rate,
        tail_trimmer=orchestrator.tail_trimmer,
        release_device_cache=lambda _device: None,
    )
    return cast(SessionOrchestrator, orchestrator)


def _stream_runtime(orchestrator: SessionOrchestrator) -> StreamPathRuntime:
    return orchestrator._stream_path_runtime_for_runtime()


def _interim_runtime(orchestrator: SessionOrchestrator) -> InterimTranscriptRuntime:
    return orchestrator._interim_transcript_runtime_for_runtime()


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
    return orchestrator.runtime_truth(
        overlay_events_enabled=orchestrator.settings.overlay_events_enabled
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
    truth = snapshot(
        RuntimeTruthSourceTrap(
            device_info=DeviceInfo(requested_device=None, effective_device=None),
            stream_path=StreamPathFacts(
                streaming_enabled=True,
                helper_active=False,
                helper_scope="live_session_only",
                helper_class_name=None,
                fallback_reason="streaming_transcriber_unavailable",
                chunk_secs=None,
                path_executed=False,
                chunks_processed=0,
            ),
        )
    )

    assert truth.device is None
    assert truth.effective_device is None
    assert truth.chunk_secs is None
    assert truth.stream_fallback_reason == "streaming_transcriber_unavailable"


def test_runtime_truth_reads_stream_path_interface_instead_of_orchestrator_fields() -> None:
    truth = snapshot(
        RuntimeTruthSourceTrap(
            stream_path=StreamPathFacts(
                streaming_enabled=True,
                helper_active=True,
                helper_scope="live_session_only",
                helper_class_name="InterfaceHelper",
                fallback_reason="interface:fallback",
                chunk_secs=1.25,
                path_executed=True,
                chunks_processed=3,
            ),
        )
    )

    assert truth.stream_helper_active is True
    assert truth.stream_helper_class_name == "InterfaceHelper"
    assert truth.stream_path_executed is True
    assert truth.stream_chunks_processed == 3
    assert truth.stream_fallback_reason == "interface:fallback"
    assert truth.chunk_secs == 1.25


def test_runtime_truth_reads_interim_interface_instead_of_orchestrator_fields() -> None:
    interim_facts = InterimTranscriptRuntimeFacts(
        enabled=True,
        last_source="stop_replay",
        live_chunks_processed=2,
        live_updates_emitted=1,
        live_failed=False,
        stop_replay_chunks_processed=3,
        stop_replay_updates_emitted=2,
        stop_replay_failed=True,
        source_fallback_reason="stop_replay_transcribe_error:RuntimeError",
    )
    truth = snapshot(RuntimeTruthSourceTrap(interim=interim_facts))

    assert truth.interim_transcript_enabled is True
    assert truth.interim_transcript_last_source == "stop_replay"
    assert truth.interim_transcript_live_chunks_processed == 2
    assert truth.interim_transcript_stop_replay_chunks_processed == 3
    assert truth.interim_transcript_updates_emitted == 3
    assert truth.interim_transcript_live_updates_emitted == 1
    assert truth.interim_transcript_stop_replay_updates_emitted == 2
    assert truth.interim_transcript_live_failed is False
    assert truth.interim_transcript_stop_replay_failed is True
    assert (
        truth.interim_transcript_source_fallback_reason
        == "stop_replay_transcribe_error:RuntimeError"
    )


def test_runtime_truth_reads_seal_path_interface() -> None:
    truth = snapshot(
        RuntimeTruthSourceTrap(
            seal_path=SealPathFacts(
                finalization_mode="offline_seal",
                final_audio_source="canonical_session_audio",
            )
        )
    )

    assert truth.finalization_mode == "offline_seal"
    assert truth.final_audio_source == "canonical_session_audio"


def test_runtime_truth_reads_overlay_transport_interface() -> None:
    truth = snapshot(RuntimeTruthSourceTrap(overlay_transport=OverlayTransportFacts(enabled=True)))

    assert truth.overlay_events_enabled is True


def test_interim_runtime_syncs_cached_session_config() -> None:
    runtime = InterimTranscriptRuntime(sample_rate=16_000, enabled=False)
    session_id = uuid4()
    runtime.reset_session(session_id)

    assert runtime.session_runtime_facts(session_id).enabled is False

    runtime.sync_from_runtime(
        settings=SimpleNamespace(overlay_events_enabled=True),
        sample_rate=8_000,
    )

    assert runtime.session_runtime_facts(session_id).enabled is True


def test_stream_runtime_ignores_invalid_chunk_secs_values() -> None:
    for chunk_secs in ("not-a-number", "nan", "inf", float("nan"), float("inf"), True):
        runtime = StreamPathRuntime.from_settings(
            SimpleNamespace(streaming_enabled=True, chunk_secs=chunk_secs),
            streaming_transcriber=None,
        )

        assert runtime.facts(active_session=False).chunk_secs is None


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
    stream_runtime = _stream_runtime(server)
    stream_runtime.record_chunk_processed()
    stream_runtime.record_chunk_processed()
    stream_runtime.record_session_result(active_stream=None)

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
    _stream_runtime(server).record_session_result(active_stream=None)

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
    stream_runtime = _stream_runtime(server)
    stream_runtime.record_chunk_processed()
    stream_runtime.record_current_fallback("stream_chunk_failed:RuntimeError")
    stream_runtime.record_session_result(active_stream=None)

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
        interim_runtime = _interim_runtime(server)
        interim_runtime.reset_session(session_id)

        async def transcribe(_samples: np.ndarray) -> str:
            return "visible interim text"

        update = await interim_runtime.accept_live_chunk(
            session_id,
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

        interim_runtime.record_last_session(session_id)
        await server.sessions.clear(session_id, owner_token=1)
        interim_runtime.clear_session(session_id)

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
        interim_runtime = _interim_runtime(server)
        interim_runtime.reset_session(session_id)

        async def transcribe(_samples: np.ndarray) -> str:
            raise RuntimeError("interim source failed")

        update = await interim_runtime.accept_live_chunk(
            session_id,
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
    stream_runtime = _stream_runtime(server)
    stream_runtime.record_session_result(active_stream=None)
    stream_runtime.reset_current()
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


def test_seal_path_runtime_sync_preserves_injected_cache_releaser() -> None:
    """Existing Seal runtime refreshes without replacing injected cleanup hooks."""
    server = _build_server(streaming_enabled=False)
    calls: list[str] = []

    def release_device_cache(device: str) -> None:
        calls.append(device)

    runtime = SealPathRuntime(
        sample_rate=8_000,
        tail_trimmer=SealPathTailTrimmer(vad_enabled=False, silence_floor_db=-40.0),
        release_device_cache=release_device_cache,
    )
    server.seal_path_runtime = runtime

    synced = server._seal_path_runtime_for_runtime()
    synced.release_device_cache("cuda:0")

    assert synced is runtime
    assert synced.sample_rate == FakeAudio.sample_rate
    assert synced.tail_trimmer is server.tail_trimmer
    assert calls == ["cuda:0"]


def test_status_last_timings_populated_after_session() -> None:
    """Timing fields reflect last session values."""
    server = _build_server(streaming_enabled=False)
    server._seal_path_runtime_for_runtime().record_success(
        SealPathFinalizationResult(
            text="final text",
            audio_ms=2500,
            audio_duration_raw=2.5,
            finalize_ms=180,
            infer_ms=120,
        ),
        audio_stop_ms=12,
        send_ms=3,
    )

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
