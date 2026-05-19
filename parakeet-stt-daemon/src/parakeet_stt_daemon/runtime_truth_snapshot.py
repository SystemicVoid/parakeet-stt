"""Single Runtime truth snapshot for Daemon status and logs."""

from __future__ import annotations

import math
from dataclasses import dataclass
from typing import Any, Literal

from .messages import StatusMessage
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
    vad_active: bool
    vad_fallback_reason: str | None


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
            "overlay_events_enabled": self.overlay_events_enabled,
        }


def snapshot(
    orchestrator: Any,
    *,
    stream_path: StreamPathFacts | None = None,
    seal_path: SealPathFacts | None = None,
    last_trim_outcome: object | None = None,
    device_info: DeviceInfo | None = None,
    overlay_events_enabled: bool | None = None,
) -> RuntimeTruth:
    settings = orchestrator.settings
    stream_path = stream_path or _stream_path_facts(orchestrator)
    seal_path = seal_path or SealPathFacts()
    tail_trim = _tail_trim_facts(orchestrator, last_trim_outcome)
    device_info = device_info or _device_info(orchestrator)
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
        vad_enabled=bool(getattr(orchestrator, "_vad_enabled", False)),
        vad_active=tail_trim.vad_active,
        vad_fallback_reason=tail_trim.vad_fallback_reason,
        overlay_events_enabled=(
            bool(getattr(settings, "overlay_events_enabled", False))
            if overlay_events_enabled is None
            else overlay_events_enabled
        ),
        chunk_secs=stream_path.chunk_secs,
    )


def format_log_record(record: dict[str, object]) -> str:
    return ", ".join(f"{key}={_format_log_value(value)}" for key, value in record.items())


def _device_info(orchestrator: Any) -> DeviceInfo:
    settings = orchestrator.settings
    requested = _string_or_none(
        getattr(orchestrator, "_requested_device", getattr(settings, "device", None))
    )
    effective = _string_or_none(getattr(orchestrator, "_effective_device", requested))
    return DeviceInfo(requested_device=requested, effective_device=effective)


def _stream_path_facts(orchestrator: Any) -> StreamPathFacts:
    settings = orchestrator.settings
    streaming_enabled = bool(getattr(settings, "streaming_enabled", False))
    chunk_secs = _chunk_secs_or_none(settings)
    streaming_transcriber = getattr(orchestrator, "streaming_transcriber", None)
    if not streaming_enabled:
        return StreamPathFacts(
            streaming_enabled=False,
            helper_active=False,
            helper_scope="live_session_only",
            helper_class_name=None,
            fallback_reason=None,
            chunk_secs=None,
            path_executed=False,
            chunks_processed=0,
        )
    if streaming_transcriber is None:
        return StreamPathFacts(
            streaming_enabled=True,
            helper_active=False,
            helper_scope="live_session_only",
            helper_class_name=None,
            fallback_reason="streaming_transcriber_unavailable",
            chunk_secs=chunk_secs,
            path_executed=False,
            chunks_processed=0,
        )
    sessions = getattr(orchestrator, "sessions", None)
    active_session = getattr(sessions, "active", None)
    if active_session is not None:
        chunks_processed = _non_negative_int(
            getattr(orchestrator, "_current_stream_chunks_processed", 0)
        )
        path_executed = chunks_processed > 0
        current_fallback_reason = getattr(orchestrator, "_current_stream_fallback_reason", None)
    else:
        path_executed = bool(getattr(orchestrator, "_last_stream_path_executed", False))
        chunks_processed = _non_negative_int(
            getattr(orchestrator, "_last_stream_chunks_processed", 0)
        )
        current_fallback_reason = None
    fallback_reason = getattr(streaming_transcriber, "fallback_reason", None)
    last_fallback_reason = (
        getattr(orchestrator, "_last_stream_fallback_reason", None)
        if active_session is None
        else None
    )
    if current_fallback_reason is not None:
        fallback_reason = current_fallback_reason
    elif last_fallback_reason is not None:
        fallback_reason = last_fallback_reason
    return StreamPathFacts(
        streaming_enabled=True,
        helper_active=bool(getattr(streaming_transcriber, "helper_active", False)),
        helper_scope="live_session_only",
        helper_class_name=_string_or_none(
            getattr(streaming_transcriber, "_helper_class_name", None)
        ),
        fallback_reason=fallback_reason,
        chunk_secs=chunk_secs,
        path_executed=path_executed,
        chunks_processed=chunks_processed,
    )


def _tail_trim_facts(orchestrator: Any, last_trim_outcome: object | None) -> TailTrimFacts:
    outcome = last_trim_outcome
    if outcome is None:
        tail_trimmer = getattr(orchestrator, "tail_trimmer", None)
        outcome = getattr(tail_trimmer, "last_outcome", None)
    if outcome is None and hasattr(orchestrator, "_tail_trimmer_for_runtime"):
        outcome = orchestrator._tail_trimmer_for_runtime().last_outcome
    return TailTrimFacts(
        tail_trim_mode=getattr(outcome, "tail_trim_mode", "rms"),
        vad_active=bool(getattr(outcome, "vad_active", False)),
        vad_fallback_reason=getattr(outcome, "vad_fallback_reason", None),
    )


def _format_log_value(value: object) -> str:
    if value is None:
        return "null"
    if isinstance(value, bool):
        return "true" if value else "false"
    return str(value)


def _string_or_none(value: object) -> str | None:
    if value is None:
        return None
    return str(value)


def _non_negative_int(value: object) -> int:
    if isinstance(value, bool):
        return 0
    if isinstance(value, int):
        parsed = value
    elif isinstance(value, float):
        if not math.isfinite(value):
            return 0
        parsed = int(value)
    elif isinstance(value, str):
        try:
            parsed = int(value)
        except ValueError:
            return 0
    else:
        return 0
    return max(0, parsed)


def _chunk_secs_or_none(settings: object) -> float | None:
    value = getattr(settings, "chunk_secs", None)
    if value is None:
        return None
    if isinstance(value, bool):
        return None
    try:
        parsed = float(value)
    except (TypeError, ValueError):
        return None
    if not math.isfinite(parsed):
        return None
    return parsed


class RuntimeTruthSnapshot:
    snapshot = staticmethod(snapshot)


__all__ = [
    "DeviceInfo",
    "RuntimeTruth",
    "RuntimeTruthMetrics",
    "RuntimeTruthSnapshot",
    "RuntimeTruthState",
    "SealPathFacts",
    "StreamPathFacts",
    "TailTrimFacts",
    "format_log_record",
    "snapshot",
]
