"""Session lifecycle tracking for push-to-talk interactions."""

from __future__ import annotations

import asyncio
import math
import time
from collections.abc import Awaitable
from dataclasses import dataclass, field
from datetime import UTC, datetime
from enum import StrEnum
from functools import partial
from typing import TYPE_CHECKING, Literal, Protocol
from uuid import UUID

from loguru import logger

from .overlay_interim import (
    InterimTranscriber,
    InterimTranscriptRuntimeFacts,
    OverlayInterimTranscriptSession,
)
from .tail_trim import SealPathTailTrimmer, TailTrimOutcome

if TYPE_CHECKING:
    import numpy as np


class SessionState(StrEnum):
    IDLE = "idle"
    LISTENING = "listening"
    PROCESSING = "processing"


@dataclass
class Session:
    session_id: UUID
    owner_token: int
    started_at: datetime = field(default_factory=lambda: datetime.now(tz=UTC))
    state: SessionState = SessionState.LISTENING
    last_updated: datetime = field(default_factory=lambda: datetime.now(tz=UTC))

    def mark_processing(self) -> None:
        self.state = SessionState.PROCESSING
        self.last_updated = datetime.now(tz=UTC)

    def mark_completed(self) -> None:
        self.state = SessionState.IDLE
        self.last_updated = datetime.now(tz=UTC)

    @property
    def audio_duration_ms(self) -> int:
        return int((datetime.now(tz=UTC) - self.started_at).total_seconds() * 1000)


class SessionBusyError(RuntimeError):
    pass


class SessionNotFoundError(RuntimeError):
    pass


StreamHelperScope = Literal["live_session_only"]
FinalizationMode = Literal["offline_seal"]
FinalAudioSource = Literal["canonical_session_audio"]
SealPathErrorCode = Literal["AUDIO_DEVICE", "MODEL"]


class SealPathTranscriber(Protocol):
    def __call__(self, samples: np.ndarray) -> Awaitable[str]: ...


class DeviceCacheReleaser(Protocol):
    def __call__(self, device: str, /) -> None: ...


@dataclass(frozen=True, slots=True)
class StreamPathRuntimeFacts:
    streaming_enabled: bool
    helper_active: bool
    helper_scope: StreamHelperScope
    helper_class_name: str | None
    fallback_reason: str | None
    chunk_secs: float | None
    path_executed: bool
    chunks_processed: int


@dataclass(frozen=True, slots=True)
class SealPathRuntimeFacts:
    finalization_mode: FinalizationMode = "offline_seal"
    final_audio_source: FinalAudioSource = "canonical_session_audio"


@dataclass(frozen=True, slots=True)
class SealPathRuntimeMetrics:
    audio_stop_ms: int | None = None
    finalize_ms: int | None = None
    infer_ms: int | None = None
    send_ms: int | None = None
    last_audio_ms: int | None = None
    last_infer_ms: int | None = None
    last_send_ms: int | None = None


@dataclass(frozen=True, slots=True)
class SealPathFinalizationResult:
    text: str
    audio_ms: int
    audio_duration_raw: float
    finalize_ms: int
    infer_ms: int
    tail_trim_ms: int = 0


@dataclass(frozen=True, slots=True)
class SealPathFinalizationFailure:
    code: SealPathErrorCode
    message: str
    exception: Exception | None = None


SealPathFinalizationOutcome = SealPathFinalizationResult | SealPathFinalizationFailure


class SealPathRuntime:
    """Own Seal path finalization and last final runtime facts."""

    def __init__(
        self,
        *,
        sample_rate: int,
        tail_trimmer: SealPathTailTrimmer,
        release_device_cache: DeviceCacheReleaser,
    ) -> None:
        self.sample_rate = int(sample_rate)
        self.tail_trimmer = tail_trimmer
        self.release_device_cache = release_device_cache
        self._metrics = SealPathRuntimeMetrics()
        self._last_failure: SealPathFinalizationFailure | None = None

    def sync_from_runtime(
        self,
        *,
        sample_rate: int,
        tail_trimmer: SealPathTailTrimmer,
        release_device_cache: DeviceCacheReleaser,
    ) -> None:
        self.sample_rate = int(sample_rate)
        self.tail_trimmer = tail_trimmer
        self.release_device_cache = release_device_cache

    @property
    def last_tail_trim_outcome(self) -> TailTrimOutcome:
        return self.tail_trimmer.last_outcome

    @property
    def last_failure(self) -> SealPathFinalizationFailure | None:
        return self._last_failure

    def facts(self) -> SealPathRuntimeFacts:
        return SealPathRuntimeFacts()

    def metrics(self) -> SealPathRuntimeMetrics:
        return self._metrics

    def prepare_tail_trimmer(self) -> None:
        self.tail_trimmer.prepare()

    async def finalize(
        self,
        audio_samples: np.ndarray,
        transcribe: SealPathTranscriber,
        *,
        effective_device: str,
    ) -> SealPathFinalizationOutcome:
        audio_duration_raw = len(audio_samples) / self.sample_rate
        audio_ms = int(audio_duration_raw * 1000)
        if audio_samples.size == 0:
            failure = SealPathFinalizationFailure(
                code="AUDIO_DEVICE",
                message="No audio captured for session",
            )
            self._last_failure = failure
            return failure

        finalize_started = time.perf_counter()
        cache_released = False
        try:
            loop = asyncio.get_running_loop()
            trim_outcome = await loop.run_in_executor(
                None,
                partial(self.tail_trimmer.trim, audio_samples, self.sample_rate),
            )
            trimmed = trim_outcome.samples
            tail_trim_ms = int((audio_samples.size - trimmed.size) * 1000 / self.sample_rate)
            if trimmed.size == 0:
                logger.info("Skipping offline transcription: silence trimming removed all samples")
                self._release_device_cache(effective_device)
                cache_released = True
                self._last_failure = None
                return SealPathFinalizationResult(
                    text="",
                    audio_ms=audio_ms,
                    audio_duration_raw=audio_duration_raw,
                    finalize_ms=int((time.perf_counter() - finalize_started) * 1000),
                    infer_ms=0,
                    tail_trim_ms=tail_trim_ms,
                )

            infer_started = time.perf_counter()
            try:
                text = await transcribe(trimmed)
                infer_ms = int((time.perf_counter() - infer_started) * 1000)
            finally:
                self._release_device_cache(effective_device)
                cache_released = True
            self._last_failure = None
            return SealPathFinalizationResult(
                text=text,
                audio_ms=audio_ms,
                audio_duration_raw=audio_duration_raw,
                finalize_ms=int((time.perf_counter() - finalize_started) * 1000),
                infer_ms=infer_ms,
                tail_trim_ms=tail_trim_ms,
            )
        except Exception as exc:  # noqa: BLE001 - returned to orchestrator as model failure
            if not cache_released:
                self._release_device_cache(effective_device)
            failure = SealPathFinalizationFailure(
                code="MODEL",
                message="Transcription failed",
                exception=exc,
            )
            self._last_failure = failure
            return failure

    def _release_device_cache(self, effective_device: str) -> None:
        try:
            self.release_device_cache(effective_device)
        except Exception as exc:  # noqa: BLE001 - cache cleanup is best effort
            logger.warning("Failed to release device cache for {}: {}", effective_device, exc)

    def record_success(
        self,
        result: SealPathFinalizationResult,
        *,
        audio_stop_ms: int,
        send_ms: int,
    ) -> None:
        self._metrics = SealPathRuntimeMetrics(
            audio_stop_ms=audio_stop_ms,
            finalize_ms=result.finalize_ms,
            infer_ms=result.infer_ms,
            send_ms=send_ms,
            last_audio_ms=result.audio_ms,
            last_infer_ms=result.infer_ms,
            last_send_ms=send_ms,
        )


class StreamPathRuntime:
    """Own Stream path runtime truth for the active and last Daemon Session."""

    def __init__(
        self,
        *,
        streaming_enabled: bool,
        chunk_secs: object,
        streaming_transcriber: object | None,
    ) -> None:
        self.streaming_enabled = bool(streaming_enabled)
        self.chunk_secs = _chunk_secs_or_none(chunk_secs)
        self.streaming_transcriber = streaming_transcriber
        self._current_chunks_processed = 0
        self._current_fallback_reason: str | None = None
        self._last_path_executed = False
        self._last_chunks_processed = 0
        self._last_fallback_reason: str | None = None

    @classmethod
    def from_settings(
        cls,
        settings: object,
        streaming_transcriber: object | None,
    ) -> StreamPathRuntime:
        return cls(
            streaming_enabled=bool(getattr(settings, "streaming_enabled", False)),
            chunk_secs=getattr(settings, "chunk_secs", None),
            streaming_transcriber=streaming_transcriber,
        )

    def sync_from_runtime(
        self,
        *,
        settings: object,
        streaming_transcriber: object | None,
    ) -> None:
        self.streaming_enabled = bool(getattr(settings, "streaming_enabled", False))
        self.chunk_secs = _chunk_secs_or_none(getattr(settings, "chunk_secs", None))
        self.streaming_transcriber = streaming_transcriber

    def should_start_drain_loop(self) -> bool:
        return self.streaming_enabled or self.streaming_transcriber is not None

    def reset_current(self) -> None:
        self._current_chunks_processed = 0
        self._current_fallback_reason = None

    def helper_available(self) -> bool:
        transcriber = self.streaming_transcriber
        return transcriber is not None and bool(getattr(transcriber, "helper_active", False))

    def fallback_reason_from_runtime(self) -> str | None:
        if not self.streaming_enabled:
            return None
        transcriber = self.streaming_transcriber
        if transcriber is None:
            return "streaming_transcriber_unavailable"
        fallback_reason = getattr(transcriber, "fallback_reason", None)
        if fallback_reason is not None:
            return str(fallback_reason)
        if not bool(getattr(transcriber, "helper_active", False)):
            return "streaming_helper_inactive"
        return None

    def record_current_fallback(self, reason: object | None) -> None:
        self._current_fallback_reason = str(reason) if reason is not None else None

    def record_chunk_processed(self) -> None:
        self._current_chunks_processed += 1

    def record_session_result(self, *, active_stream: object | None) -> None:
        chunks_processed = self._current_chunks_processed
        fallback_reason = self._current_fallback_reason
        if active_stream is not None and fallback_reason is None:
            stream_reason = getattr(active_stream, "stream_fallback_reason", None)
            fallback_reason = str(stream_reason) if stream_reason is not None else None
        if self.streaming_enabled:
            if fallback_reason is None:
                fallback_reason = self.fallback_reason_from_runtime()
            if chunks_processed <= 0 and fallback_reason is None:
                fallback_reason = "stream_path_not_exercised:no_chunks"
        else:
            fallback_reason = None
        self._last_chunks_processed = max(0, int(chunks_processed))
        self._last_path_executed = chunks_processed > 0
        self._last_fallback_reason = fallback_reason

    def facts(self, *, active_session: bool) -> StreamPathRuntimeFacts:
        if not self.streaming_enabled:
            return StreamPathRuntimeFacts(
                streaming_enabled=False,
                helper_active=False,
                helper_scope="live_session_only",
                helper_class_name=None,
                fallback_reason=None,
                chunk_secs=None,
                path_executed=False,
                chunks_processed=0,
            )

        transcriber = self.streaming_transcriber
        if transcriber is None:
            return StreamPathRuntimeFacts(
                streaming_enabled=True,
                helper_active=False,
                helper_scope="live_session_only",
                helper_class_name=None,
                fallback_reason="streaming_transcriber_unavailable",
                chunk_secs=self.chunk_secs,
                path_executed=False,
                chunks_processed=0,
            )

        if active_session:
            chunks_processed = self._current_chunks_processed
            path_executed = chunks_processed > 0
            fallback_reason = self._current_fallback_reason
        else:
            chunks_processed = self._last_chunks_processed
            path_executed = self._last_path_executed
            fallback_reason = self._last_fallback_reason

        if fallback_reason is None:
            transcriber_fallback = getattr(transcriber, "fallback_reason", None)
            fallback_reason = (
                str(transcriber_fallback) if transcriber_fallback is not None else None
            )

        return StreamPathRuntimeFacts(
            streaming_enabled=True,
            helper_active=bool(getattr(transcriber, "helper_active", False)),
            helper_scope="live_session_only",
            helper_class_name=_string_or_none(getattr(transcriber, "_helper_class_name", None)),
            fallback_reason=fallback_reason,
            chunk_secs=self.chunk_secs,
            path_executed=path_executed,
            chunks_processed=max(0, int(chunks_processed)),
        )


class InterimTranscriptRuntime:
    """Own interim transcript source runtime truth for Daemon Sessions."""

    def __init__(self, *, sample_rate: int, enabled: bool) -> None:
        self.sample_rate = int(sample_rate)
        self.enabled = bool(enabled)
        self._by_session: dict[UUID, OverlayInterimTranscriptSession] = {}
        self._last_facts: InterimTranscriptRuntimeFacts | None = None

    @classmethod
    def from_settings(cls, settings: object, *, sample_rate: int) -> InterimTranscriptRuntime:
        return cls(
            sample_rate=sample_rate,
            enabled=bool(getattr(settings, "overlay_events_enabled", False)),
        )

    def sync_from_runtime(self, *, settings: object, sample_rate: int) -> None:
        self.sample_rate = int(sample_rate)
        self.enabled = bool(getattr(settings, "overlay_events_enabled", False))
        for session in self._by_session.values():
            session.sync_runtime_config(
                sample_rate=self.sample_rate,
                enabled=self.enabled,
            )

    def reset_session(self, session_id: UUID) -> None:
        self._by_session[session_id] = self._new_session(session_id)

    def clear_session(self, session_id: UUID) -> None:
        self._by_session.pop(session_id, None)

    def record_last_session(self, session_id: UUID) -> None:
        session = self._by_session.get(session_id)
        self._last_facts = session.runtime_facts if session is not None else self.empty_facts()

    def session_runtime_facts(self, session_id: UUID) -> InterimTranscriptRuntimeFacts:
        return self._session(session_id).runtime_facts

    def facts(self, *, active_session_id: UUID | None) -> InterimTranscriptRuntimeFacts:
        if active_session_id is not None:
            return self._session(active_session_id).runtime_facts
        return self._last_facts if self._last_facts is not None else self.empty_facts()

    async def accept_live_chunk(
        self,
        session_id: UUID,
        chunk: np.ndarray,
        transcribe: InterimTranscriber,
    ) -> str | None:
        return await self._session(session_id).accept_live_chunk(chunk, transcribe)

    async def collect_stop_replay_updates(
        self,
        session_id: UUID,
        ready_chunks: list[np.ndarray],
        transcribe: InterimTranscriber,
    ) -> list[str]:
        return await self._session(session_id).collect_stop_replay_updates(
            ready_chunks,
            transcribe,
        )

    def empty_facts(self) -> InterimTranscriptRuntimeFacts:
        return InterimTranscriptRuntimeFacts(
            enabled=self.enabled,
            last_source=None,
            live_chunks_processed=0,
            live_updates_emitted=0,
            live_failed=False,
            stop_replay_chunks_processed=0,
            stop_replay_updates_emitted=0,
            stop_replay_failed=False,
            source_fallback_reason=None,
        )

    def _session(self, session_id: UUID) -> OverlayInterimTranscriptSession:
        session = self._by_session.get(session_id)
        if session is None:
            session = self._new_session(session_id)
            self._by_session[session_id] = session
        return session

    def _new_session(self, session_id: UUID) -> OverlayInterimTranscriptSession:
        return OverlayInterimTranscriptSession(
            session_id=session_id,
            sample_rate=self.sample_rate,
            enabled=self.enabled,
        )


class SessionManager:
    """Coordinate access to the single active session allowed by the daemon."""

    def __init__(self) -> None:
        self._active: Session | None = None
        self._lock = asyncio.Lock()

    @property
    def active(self) -> Session | None:
        return self._active

    async def start_session(self, session_id: UUID, *, owner_token: int) -> Session:
        async with self._lock:
            if self._active and self._active.state != SessionState.IDLE:
                raise SessionBusyError("A session is already active")
            self._active = Session(session_id=session_id, owner_token=owner_token)
            return self._active

    async def stop_session(self, session_id: UUID, *, owner_token: int | None = None) -> Session:
        async with self._lock:
            if not self._active or self._active.session_id != session_id:
                raise SessionNotFoundError("No matching active session")
            if owner_token is not None and self._active.owner_token != owner_token:
                raise SessionNotFoundError("No matching active session")
            session = self._active
            session.mark_processing()
            return session

    async def clear(self, session_id: UUID, *, owner_token: int | None = None) -> None:
        async with self._lock:
            if (
                self._active
                and self._active.session_id == session_id
                and (owner_token is None or self._active.owner_token == owner_token)
            ):
                self._active.mark_completed()
                self._active = None


__all__ = [
    "InterimTranscriptRuntime",
    "InterimTranscriptRuntimeFacts",
    "SealPathErrorCode",
    "SealPathFinalizationFailure",
    "SealPathFinalizationOutcome",
    "SealPathFinalizationResult",
    "SealPathRuntime",
    "SealPathRuntimeFacts",
    "SealPathRuntimeMetrics",
    "SealPathTranscriber",
    "Session",
    "SessionBusyError",
    "SessionManager",
    "SessionNotFoundError",
    "SessionState",
    "StreamPathRuntime",
    "StreamPathRuntimeFacts",
]


def _chunk_secs_or_none(value: object) -> float | None:
    if value is None:
        return None
    if isinstance(value, bool):
        return None
    if not isinstance(value, str | int | float):
        return None
    try:
        parsed = float(value)
    except (TypeError, ValueError):
        return None
    if not math.isfinite(parsed):
        return None
    return parsed


def _string_or_none(value: object) -> str | None:
    if value is None:
        return None
    return str(value)
