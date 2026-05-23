"""Single Runtime truth snapshot for Daemon status and logs."""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal, Protocol

from .messages import StatusMessage
from .overlay_interim import InterimTranscriptRuntimeFacts, InterimTranscriptSource
from .session import SessionState
from .tail_trim import TailTrimMode

StreamHelperScope = Literal["live_session_only"]
FinalizationMode = Literal["offline_seal"]
FinalAudioSource = Literal["canonical_session_audio"]


@dataclass(frozen=True, slots=True)
class DeviceInfo:
    requested_device: str | None
    effective_device: str | None


@dataclass(frozen=True, slots=True)
class StreamPathFacts:
    streaming_enabled: bool
    helper_active: bool
    helper_scope: StreamHelperScope
    helper_class_name: str | None
    fallback_reason: str | None
    chunk_secs: float | None
    path_executed: bool
    chunks_processed: int


@dataclass(frozen=True, slots=True)
class SealPathFacts:
    finalization_mode: FinalizationMode = "offline_seal"
    final_audio_source: FinalAudioSource = "canonical_session_audio"


@dataclass(frozen=True, slots=True)
class TailTrimFacts:
    tail_trim_mode: TailTrimMode
    vad_enabled: bool
    vad_active: bool
    vad_fallback_reason: str | None


@dataclass(frozen=True, slots=True)
class OverlayTransportFacts:
    enabled: bool


@dataclass(frozen=True, slots=True)
class RuntimeTruthState:
    state: SessionState
    sessions_active: int
    active_session_age_ms: int | None


@dataclass(frozen=True, slots=True)
class RuntimeTruthMetrics:
    gpu_mem_mb: int | None = None
    overlay_events_emitted: int | None = None
    overlay_events_dropped: int | None = None
    audio_stop_ms: int | None = None
    finalize_ms: int | None = None
    infer_ms: int | None = None
    send_ms: int | None = None
    last_audio_ms: int | None = None
    last_infer_ms: int | None = None
    last_send_ms: int | None = None


class RuntimeTruthSource(Protocol):
    """Typed source for Daemon runtime truth facts."""

    def runtime_truth_device_info(self) -> DeviceInfo: ...

    def runtime_truth_stream_path_facts(self) -> StreamPathFacts: ...

    def runtime_truth_seal_path_facts(self) -> SealPathFacts: ...

    def runtime_truth_tail_trim_facts(self) -> TailTrimFacts: ...

    def runtime_truth_interim_transcript_facts(self) -> InterimTranscriptRuntimeFacts: ...

    def runtime_truth_overlay_transport_facts(self) -> OverlayTransportFacts: ...


@dataclass(frozen=True, slots=True)
class RuntimeTruth:
    device: str | None
    effective_device: str | None
    streaming_enabled: bool
    stream_helper_active: bool
    stream_helper_scope: StreamHelperScope
    stream_helper_class_name: str | None
    stream_fallback_reason: str | None
    stream_path_executed: bool
    stream_chunks_processed: int
    finalization_mode: FinalizationMode
    final_audio_source: FinalAudioSource
    tail_trim_mode: TailTrimMode
    vad_enabled: bool
    vad_active: bool
    vad_fallback_reason: str | None
    interim_transcript_enabled: bool
    interim_transcript_last_source: InterimTranscriptSource | None
    interim_transcript_live_chunks_processed: int
    interim_transcript_stop_replay_chunks_processed: int
    interim_transcript_updates_emitted: int
    interim_transcript_live_updates_emitted: int
    interim_transcript_stop_replay_updates_emitted: int
    interim_transcript_live_failed: bool
    interim_transcript_stop_replay_failed: bool
    interim_transcript_source_fallback_reason: str | None
    overlay_events_enabled: bool
    chunk_secs: float | None

    @property
    def degraded(self) -> bool:
        stream_degraded = self.streaming_enabled and (
            not self.stream_helper_active or self.stream_fallback_reason is not None
        )
        return stream_degraded or (self.vad_enabled and not self.vad_active)

    def to_status(self, state: RuntimeTruthState, metrics: RuntimeTruthMetrics) -> StatusMessage:
        return StatusMessage(
            state=state.state,
            sessions_active=state.sessions_active,
            gpu_mem_mb=metrics.gpu_mem_mb,
            device=self.device,
            effective_device=self.effective_device,
            streaming_enabled=self.streaming_enabled,
            stream_helper_active=self.stream_helper_active,
            stream_helper_scope=self.stream_helper_scope,
            stream_fallback_reason=self.stream_fallback_reason,
            stream_path_executed=self.stream_path_executed,
            stream_chunks_processed=self.stream_chunks_processed,
            finalization_mode=self.finalization_mode,
            final_audio_source=self.final_audio_source,
            tail_trim_mode=self.tail_trim_mode,
            vad_enabled=self.vad_enabled,
            vad_active=self.vad_active,
            vad_fallback_reason=self.vad_fallback_reason,
            interim_transcript_enabled=self.interim_transcript_enabled,
            interim_transcript_last_source=self.interim_transcript_last_source,
            interim_transcript_live_chunks_processed=(
                self.interim_transcript_live_chunks_processed
            ),
            interim_transcript_stop_replay_chunks_processed=(
                self.interim_transcript_stop_replay_chunks_processed
            ),
            interim_transcript_updates_emitted=self.interim_transcript_updates_emitted,
            interim_transcript_live_updates_emitted=(self.interim_transcript_live_updates_emitted),
            interim_transcript_stop_replay_updates_emitted=(
                self.interim_transcript_stop_replay_updates_emitted
            ),
            interim_transcript_live_failed=self.interim_transcript_live_failed,
            interim_transcript_stop_replay_failed=self.interim_transcript_stop_replay_failed,
            interim_transcript_source_fallback_reason=(
                self.interim_transcript_source_fallback_reason
            ),
            overlay_events_enabled=self.overlay_events_enabled,
            overlay_events_emitted=metrics.overlay_events_emitted,
            overlay_events_dropped=metrics.overlay_events_dropped,
            chunk_secs=self.chunk_secs,
            active_session_age_ms=state.active_session_age_ms,
            audio_stop_ms=metrics.audio_stop_ms,
            finalize_ms=metrics.finalize_ms,
            infer_ms=metrics.infer_ms,
            send_ms=metrics.send_ms,
            last_audio_ms=metrics.last_audio_ms,
            last_infer_ms=metrics.last_infer_ms,
            last_send_ms=metrics.last_send_ms,
        )

    def to_log_record(self) -> dict[str, object]:
        return {
            "device_requested": self.device,
            "device_effective": self.effective_device,
            "streaming_enabled": self.streaming_enabled,
            "live_session_helper_active": self.stream_helper_active,
            "live_session_helper_scope": self.stream_helper_scope,
            "live_session_helper_class": self.stream_helper_class_name,
            "stream_fallback_reason": self.stream_fallback_reason,
            "stream_path_executed": self.stream_path_executed,
            "stream_chunks_processed": self.stream_chunks_processed,
            "finalization_mode": self.finalization_mode,
            "final_audio_source": self.final_audio_source,
            "tail_trim_mode": self.tail_trim_mode,
            "vad_enabled": self.vad_enabled,
            "vad_active": self.vad_active,
            "vad_fallback_reason": self.vad_fallback_reason,
            "interim_transcript_enabled": self.interim_transcript_enabled,
            "interim_transcript_last_source": self.interim_transcript_last_source,
            "interim_transcript_live_chunks_processed": (
                self.interim_transcript_live_chunks_processed
            ),
            "interim_transcript_stop_replay_chunks_processed": (
                self.interim_transcript_stop_replay_chunks_processed
            ),
            "interim_transcript_updates_emitted": self.interim_transcript_updates_emitted,
            "interim_transcript_live_updates_emitted": (
                self.interim_transcript_live_updates_emitted
            ),
            "interim_transcript_stop_replay_updates_emitted": (
                self.interim_transcript_stop_replay_updates_emitted
            ),
            "interim_transcript_live_failed": self.interim_transcript_live_failed,
            "interim_transcript_stop_replay_failed": self.interim_transcript_stop_replay_failed,
            "interim_transcript_source_fallback_reason": (
                self.interim_transcript_source_fallback_reason
            ),
            "overlay_events_enabled": self.overlay_events_enabled,
        }


def snapshot(
    source: RuntimeTruthSource,
) -> RuntimeTruth:
    device_info = source.runtime_truth_device_info()
    stream_path = source.runtime_truth_stream_path_facts()
    seal_path = source.runtime_truth_seal_path_facts()
    tail_trim = source.runtime_truth_tail_trim_facts()
    interim = source.runtime_truth_interim_transcript_facts()
    overlay_transport = source.runtime_truth_overlay_transport_facts()
    return RuntimeTruth(
        device=device_info.requested_device,
        effective_device=device_info.effective_device,
        streaming_enabled=stream_path.streaming_enabled,
        stream_helper_active=stream_path.helper_active,
        stream_helper_scope=stream_path.helper_scope,
        stream_helper_class_name=stream_path.helper_class_name,
        stream_fallback_reason=stream_path.fallback_reason,
        stream_path_executed=stream_path.path_executed,
        stream_chunks_processed=stream_path.chunks_processed,
        finalization_mode=seal_path.finalization_mode,
        final_audio_source=seal_path.final_audio_source,
        tail_trim_mode=tail_trim.tail_trim_mode,
        vad_enabled=tail_trim.vad_enabled,
        vad_active=tail_trim.vad_active,
        vad_fallback_reason=tail_trim.vad_fallback_reason,
        interim_transcript_enabled=interim.enabled,
        interim_transcript_last_source=interim.last_source,
        interim_transcript_live_chunks_processed=interim.live_chunks_processed,
        interim_transcript_stop_replay_chunks_processed=interim.stop_replay_chunks_processed,
        interim_transcript_updates_emitted=interim.updates_emitted,
        interim_transcript_live_updates_emitted=interim.live_updates_emitted,
        interim_transcript_stop_replay_updates_emitted=interim.stop_replay_updates_emitted,
        interim_transcript_live_failed=interim.live_failed,
        interim_transcript_stop_replay_failed=interim.stop_replay_failed,
        interim_transcript_source_fallback_reason=interim.source_fallback_reason,
        overlay_events_enabled=overlay_transport.enabled,
        chunk_secs=stream_path.chunk_secs,
    )


def format_log_record(record: dict[str, object]) -> str:
    return ", ".join(f"{key}={_format_log_value(value)}" for key, value in record.items())


def _format_log_value(value: object) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


class RuntimeTruthSnapshot:
    snapshot = staticmethod(snapshot)


__all__ = [
    "DeviceInfo",
    "OverlayTransportFacts",
    "RuntimeTruth",
    "RuntimeTruthMetrics",
    "RuntimeTruthSnapshot",
    "RuntimeTruthSource",
    "RuntimeTruthState",
    "SealPathFacts",
    "StreamPathFacts",
    "TailTrimFacts",
    "format_log_record",
    "snapshot",
]
