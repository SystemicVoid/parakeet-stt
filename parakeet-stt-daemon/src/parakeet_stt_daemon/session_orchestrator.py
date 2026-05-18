"""Session orchestration for the Daemon Stream path and Seal path.

The SessionOrchestrator owns the Daemon's in-process Session lifecycle. It
does not know about WebSockets; callers translate transport messages into
intents and receive Stream path and Seal path progress through an EventSink.
"""

from __future__ import annotations

import asyncio
import math
import time
from dataclasses import dataclass
from datetime import UTC, datetime
from functools import partial
from typing import Literal
from uuid import UUID

import numpy as np
from loguru import logger

from .audio import AudioInput
from .config import ServerSettings
from .events import (
    AudioLevelEvent,
    ErrorCode,
    EventSink,
    EventSinkClosed,
    FinalResultEvent,
    InterimStateEvent,
    InterimTextEvent,
    SessionEndedEvent,
    SessionErrorEvent,
    SessionStartedEvent,
    SessionWarningEvent,
)
from .messages import (
    InterimStateValue,
    SessionEndReason,
)
from .model import (
    ParakeetStreamingSession,
    ParakeetStreamingTranscriber,
    ParakeetTranscriber,
    _release_cuda_cache,
    load_parakeet_model,
)
from .overlay_interim import (
    OverlayInterimTranscriptContext,
    OverlayInterimTranscriptStabilizer,
    append_overlay_interim_context,
)
from .runtime_truth_snapshot import (
    RuntimeTruth,
    RuntimeTruthMetrics,
    RuntimeTruthState,
    format_log_record,
)
from .runtime_truth_snapshot import (
    snapshot as runtime_truth_snapshot,
)
from .session import Session, SessionBusyError, SessionManager, SessionNotFoundError, SessionState
from .tail_trim import SealPathTailTrimmer

SESSION_GUARD_POLL_SECS = 0.1
SESSION_GUARD_WARNING_FRACTION = 0.8  # emit warning at 80% of limit
_REAL_ASYNCIO_SLEEP = asyncio.sleep


@dataclass(frozen=True, slots=True)
class StartSessionIntent:
    session_id: UUID
    owner_token: int
    event_sink: EventSink
    preferred_lang: str | None = "auto"


@dataclass(frozen=True, slots=True)
class StopSessionIntent:
    session_id: UUID
    owner_token: int
    event_sink: EventSink
    post_roll_secs: float = 0.25


@dataclass(frozen=True, slots=True)
class AbortSessionIntent:
    session_id: UUID
    owner_token: int
    event_sink: EventSink
    reason: Literal["timeout", "user", "error"]


class SessionOrchestrator:
    """Coordinate Session lifecycle and Daemon runtime paths."""

    def __init__(self, settings: ServerSettings) -> None:
        self.settings = settings
        self.sessions = SessionManager()
        sample_rate = 16_000
        duration_limit_samples = max(1, int(settings.max_session_seconds * sample_rate))
        explicit_sample_limit = (
            int(settings.max_session_samples)
            if settings.max_session_samples is not None
            else duration_limit_samples
        )
        self._session_sample_limit = max(1, min(duration_limit_samples, explicit_sample_limit))
        self._session_age_limit_ms = max(1, int(settings.max_session_seconds * 1000))
        self.audio = AudioInput(
            sample_rate=sample_rate,
            channels=1,
            dtype="float32",
            device=settings.mic_device,
            max_session_samples=self._session_sample_limit,
        )
        self.model = load_parakeet_model(device=settings.device)
        self.transcriber = ParakeetTranscriber(self.model)
        self._requested_device = str(settings.device)
        self._effective_device = str(
            getattr(self.model, "_parakeet_effective_device", self._requested_device)
        )
        self._session_lock = asyncio.Lock()
        self._inference_lock = asyncio.Lock()
        self.streaming_transcriber: ParakeetStreamingTranscriber | None = (
            ParakeetStreamingTranscriber(
                self.model,
                chunk_secs=settings.chunk_secs,
                right_context_secs=settings.right_context_secs,
                left_context_secs=settings.left_context_secs,
                batch_size=settings.batch_size,
            )
            if settings.streaming_enabled
            else None
        )
        self._active_stream: ParakeetStreamingSession | None = None
        self._stream_drain_task: asyncio.Task | None = None
        self._stream_drain_running = False
        self._session_guard_task: asyncio.Task | None = None
        self._session_guard_running = False
        self._last_audio_ms: int | None = None
        self._last_audio_stop_ms: int | None = None
        self._last_finalize_ms: int | None = None
        self._last_infer_ms: int | None = None
        self._last_send_ms: int | None = None
        self._live_interim_audio = np.zeros((0,), dtype=np.float32)
        self._live_interim_failed = False
        self._vad_enabled = bool(settings.vad_enabled)
        self.tail_trimmer = SealPathTailTrimmer(
            vad_enabled=self._vad_enabled,
            silence_floor_db=float(settings.silence_floor_db),
            warmup_sample_rate=sample_rate,
        )
        if settings.streaming_enabled:
            chunk_samples = int(settings.chunk_secs * self.audio.sample_rate)
            self.audio.configure_stream_chunk_size(chunk_samples)
        self._overlay_interim_stabilizer_by_session: dict[
            UUID, OverlayInterimTranscriptStabilizer
        ] = {}

    async def start(self, intent: StartSessionIntent) -> None:
        await self._handle_start(intent)

    async def stop(self, intent: StopSessionIntent) -> None:
        await self._handle_stop(intent)

    async def abort(self, intent: AbortSessionIntent) -> None:
        await self._handle_abort(intent)

    async def _handle_start(self, message: StartSessionIntent) -> None:
        logger.debug("start_session received: {}", message)
        owner_token = message.owner_token
        event_sink = message.event_sink
        try:
            session = await self.sessions.start_session(message.session_id, owner_token=owner_token)
        except SessionBusyError:
            await self._send_error(
                event_sink, message.session_id, "SESSION_BUSY", "A session is already active"
            )
            return
        try:
            self._live_interim_audio = np.zeros((0,), dtype=np.float32)
            self._live_interim_failed = False
            self._clear_overlay_session_runtime(message.session_id)
            self.audio.start_session()
            if self.streaming_transcriber:
                self._active_stream = self.streaming_transcriber.start_session(
                    self.audio.sample_rate
                )
                self._start_stream_drain_loop(event_sink, message.session_id)
            self._start_session_guard_loop(event_sink, message.session_id, owner_token=owner_token)

            await event_sink.emit(
                SessionStartedEvent(
                    session_id=message.session_id,
                    ts=datetime.now(tz=UTC),
                    mic_device=str(self.settings.mic_device) if self.settings.mic_device else None,
                    lang=message.preferred_lang,
                )
            )
            await self._emit_interim_state(
                event_sink,
                message.session_id,
                state=InterimStateValue.LISTENING,
            )
        except EventSinkClosed:
            await self._cleanup_active_session(
                "start_session event sink disconnected",
                expected_session_id=message.session_id,
                expected_owner_token=owner_token,
                require_session_match=True,
                require_owner_match=True,
            )
            raise
        except Exception as exc:  # noqa: BLE001
            logger.exception("Failed to start session {}: {}", message.session_id, exc)
            await self._cleanup_active_session(
                f"start_session rollback: {exc.__class__.__name__}",
                expected_session_id=message.session_id,
                expected_owner_token=owner_token,
                require_session_match=True,
                require_owner_match=True,
            )
            try:
                await self._send_error(
                    event_sink,
                    message.session_id,
                    "UNEXPECTED",
                    "Failed to start session",
                )
            except Exception as send_exc:  # noqa: BLE001
                logger.debug("Failed to send start_session error response: {}", send_exc)
            return
        logger.info("Session {} started", session.session_id)

    async def _handle_stop(self, message: StopSessionIntent) -> None:
        logger.debug("stop_session received: {}", message)
        await self._stop_active_session(
            message.event_sink,
            message.session_id,
            owner_token=message.owner_token,
            post_roll_secs=message.post_roll_secs,
        )

    async def _stop_active_session(
        self,
        event_sink: EventSink,
        session_id: UUID,
        *,
        owner_token: int,
        post_roll_secs: float,
    ) -> None:
        if post_roll_secs > 0:
            await asyncio.sleep(post_roll_secs)  # brief post-roll to capture tail audio
        async with self._session_lock_for_runtime():
            try:
                session = await self.sessions.stop_session(session_id, owner_token=owner_token)
            except SessionNotFoundError:
                await self._send_error(
                    event_sink, session_id, "SESSION_NOT_FOUND", "No matching active session"
                )
                return
            self._stop_session_guard_loop()
            await self._emit_interim_state(
                event_sink,
                session.session_id,
                state=InterimStateValue.PROCESSING,
            )
            audio_stop_started = time.perf_counter()
            audio_samples, ready_chunks, _tail = self.audio.stop_session_with_streaming()
            await self._stop_stream_drain_loop()
            # Final correctness must come from the capture layer's canonical buffer,
            # not whatever the drain task managed to mirror into `_active_stream`.
            self._active_stream = None
            audio_stop_ms = int((time.perf_counter() - audio_stop_started) * 1000)
            audio_duration_raw = len(audio_samples) / self.audio.sample_rate
            audio_ms = int(audio_duration_raw * 1000)

            if audio_samples.size == 0:
                await self._send_error(
                    event_sink,
                    session.session_id,
                    "AUDIO_DEVICE",
                    "No audio captured for session",
                )
                await self._emit_session_ended(
                    event_sink, session.session_id, reason=SessionEndReason.ERROR
                )
                await self.sessions.clear(session.session_id, owner_token=owner_token)
                self._clear_overlay_session_runtime(session.session_id)
                self._live_interim_audio = np.zeros((0,), dtype=np.float32)
                self._live_interim_failed = False
                return

            finalize_ms: int | None = None
            infer_ms: int | None = None
            try:
                interim_updates = await self._collect_interim_text_updates(
                    session.session_id,
                    ready_chunks,
                )
                flushed_interim = self._overlay_interim_stabilizer(
                    session.session_id
                ).flush_pending_tail()
                if flushed_interim is not None:
                    interim_updates.append(flushed_interim.text)
                if interim_updates:
                    await self._emit_interim_state(
                        event_sink,
                        session.session_id,
                        state=InterimStateValue.INTERIM,
                    )
                    for interim_text in interim_updates:
                        await self._emit_interim_text(
                            event_sink,
                            session.session_id,
                            text=interim_text,
                        )
                await self._emit_interim_state(
                    event_sink,
                    session.session_id,
                    state=InterimStateValue.FINALIZING,
                )
                finalize_started = time.perf_counter()
                text, infer_ms = await self._finalise_transcription(audio_samples)
                finalize_ms = int((time.perf_counter() - finalize_started) * 1000)
            except Exception as exc:  # noqa: BLE001
                logger.exception("Failed to transcribe session {}: {}", session.session_id, exc)
                await self._send_error(
                    event_sink, session.session_id, "MODEL", "Transcription failed"
                )
                await self._emit_session_ended(
                    event_sink, session.session_id, reason=SessionEndReason.ERROR
                )
                await self.sessions.clear(session.session_id, owner_token=owner_token)
                self._clear_overlay_session_runtime(session.session_id)
                self._live_interim_audio = np.zeros((0,), dtype=np.float32)
                self._live_interim_failed = False
                return

            latency_ms = int((datetime.now(tz=UTC) - session.last_updated).total_seconds() * 1000)
            send_started = datetime.now(tz=UTC)
            runtime_truth = self.runtime_truth(
                overlay_events_enabled=self.settings.overlay_events_enabled
            )
            await event_sink.emit(
                FinalResultEvent(
                    session_id=session.session_id,
                    text=text,
                    latency_ms=latency_ms,
                    audio_ms=audio_ms,
                    lang=self.settings.language,
                    confidence=None,
                    tail_trim_mode=runtime_truth.tail_trim_mode,
                    vad_active=runtime_truth.vad_active,
                    vad_fallback_reason=runtime_truth.vad_fallback_reason,
                )
            )
            send_ms = int((datetime.now(tz=UTC) - send_started).total_seconds() * 1000)
            await self._emit_session_ended(
                event_sink, session.session_id, reason=SessionEndReason.FINAL
            )
            await self.sessions.clear(session.session_id, owner_token=owner_token)
            self._clear_overlay_session_runtime(session.session_id)
            self._live_interim_audio = np.zeros((0,), dtype=np.float32)
            self._live_interim_failed = False
            self._last_audio_ms = audio_ms
            self._last_audio_stop_ms = audio_stop_ms
            self._last_finalize_ms = finalize_ms
            self._last_infer_ms = infer_ms
            self._last_send_ms = send_ms

            # Diagnostic logging for truncation investigation
            text_len = len(text)
            chars_per_sec = text_len / audio_duration_raw if audio_duration_raw > 0 else 0
            logger.info(
                "Session {} completed: audio_raw={:.2f}s, audio_ms={}, audio_stop_ms={}, "
                "latency_ms={}, finalize_ms={}, infer_ms={}, send_ms={}, text_len={}, "
                "chars_per_sec={:.1f}, runtime_truth={}",
                session.session_id,
                audio_duration_raw,
                audio_ms,
                audio_stop_ms,
                latency_ms,
                finalize_ms,
                infer_ms,
                send_ms,
                text_len,
                chars_per_sec,
                format_log_record(runtime_truth.to_log_record()),
            )

    async def _handle_abort(self, message: AbortSessionIntent) -> None:
        logger.debug("abort_session received: {}", message)
        cleaned = await self._cleanup_active_session(
            f"abort_session requested ({message.reason})",
            expected_session_id=message.session_id,
            expected_owner_token=message.owner_token,
            require_session_match=True,
            require_owner_match=True,
        )
        if cleaned:
            await self._emit_session_ended(
                message.event_sink, message.session_id, reason=SessionEndReason.ABORT
            )
            code = "SESSION_ABORTED"
            error_message = f"Session aborted: {message.reason}"
        else:
            code = "SESSION_NOT_FOUND"
            error_message = "No matching active session"
        await self._send_error(message.event_sink, message.session_id, code, error_message)

    async def _cleanup_active_session(
        self,
        reason: str,
        expected_session_id: UUID | None = None,
        expected_owner_token: int | None = None,
        *,
        require_session_match: bool = False,
        require_owner_match: bool = False,
        event_sink: EventSink | None = None,
        session_end_reason: SessionEndReason | None = None,
    ) -> bool:
        """Reset all runtime state tied to an active session."""
        async with self._session_lock_for_runtime():
            active = self.sessions.active
            if require_session_match:
                if expected_session_id is None and active is not None:
                    logger.debug(
                        "Skipping cleanup with no expected session (active session is {})",
                        active.session_id,
                    )
                    return False
                if expected_session_id is not None and (
                    active is None or active.session_id != expected_session_id
                ):
                    logger.debug(
                        "Skipping cleanup for session {} (active session is {})",
                        expected_session_id,
                        active.session_id if active else None,
                    )
                    return False
            if require_owner_match:
                if expected_owner_token is None and active is not None:
                    logger.debug(
                        "Skipping cleanup with no expected owner (active owner is {})",
                        active.owner_token,
                    )
                    return False
                if expected_owner_token is not None and (
                    active is None or active.owner_token != expected_owner_token
                ):
                    logger.debug(
                        "Skipping cleanup for owner {} (active owner is {})",
                        expected_owner_token,
                        active.owner_token if active else None,
                    )
                    return False
            if (
                active is not None
                and expected_session_id is not None
                and active.session_id != expected_session_id
            ):
                logger.debug(
                    "Skipping cleanup for session {} (active session is {})",
                    expected_session_id,
                    active.session_id,
                )
                return False
            if (
                active is not None
                and expected_owner_token is not None
                and active.owner_token != expected_owner_token
            ):
                logger.debug(
                    "Skipping cleanup for owner {} (active owner is {})",
                    expected_owner_token,
                    active.owner_token,
                )
                return False

            active_session_id = active.session_id if active else None
            active_owner_token = active.owner_token if active else None
            if active_session_id is not None:
                logger.warning("Cleaning up active session {} ({})", active_session_id, reason)
            else:
                logger.debug("Cleaning residual runtime state with no active session ({})", reason)
            self.audio.abort_session()
            await self._stop_stream_drain_loop()
            self._stop_session_guard_loop()
            if active_session_id is not None:
                await self.sessions.clear(active_session_id, owner_token=active_owner_token)
                self._clear_overlay_session_runtime(active_session_id)
            self._active_stream = None
            self._live_interim_audio = np.zeros((0,), dtype=np.float32)
            self._live_interim_failed = False
            if active_session_id is not None and event_sink is not None and session_end_reason:
                await self._emit_session_ended(
                    event_sink, active_session_id, reason=session_end_reason
                )
            return active_session_id is not None

    def _audio_session_limit_exceeded(self) -> bool:
        checker = getattr(self.audio, "session_limit_exceeded", None)
        if not callable(checker):
            return False
        try:
            return bool(checker())
        except Exception as exc:  # noqa: BLE001
            logger.debug("Failed checking audio session limit state: {}", exc)
            return False

    def _session_guard_warning_due(self, session: Session) -> bool:
        """Return True when session has reached the warning threshold (80% of limit)."""
        effective_limit_ms = self._effective_session_limit_ms()
        if effective_limit_ms is None:
            return False
        threshold_ms = int(effective_limit_ms * SESSION_GUARD_WARNING_FRACTION)
        return session.audio_duration_ms >= threshold_ms

    def _session_remaining_seconds(self, session: Session) -> float:
        effective_limit_ms = self._effective_session_limit_ms()
        if effective_limit_ms is None:
            return 0.0
        return max(0.0, (effective_limit_ms - session.audio_duration_ms) / 1000.0)

    def _effective_session_limit_ms(self) -> int | None:
        limits_ms: list[int] = []
        session_age_limit_ms = getattr(self, "_session_age_limit_ms", None)
        if session_age_limit_ms is not None:
            limits_ms.append(max(1, int(session_age_limit_ms)))

        explicit_sample_limit = getattr(self.settings, "max_session_samples", None)
        sample_rate = int(getattr(self.audio, "sample_rate", 0))
        session_sample_limit = getattr(self, "_session_sample_limit", None)
        if (
            explicit_sample_limit is not None
            and session_sample_limit is not None
            and sample_rate > 0
        ):
            sample_limit_ms = math.ceil((int(session_sample_limit) * 1000) / sample_rate)
            limits_ms.append(max(1, sample_limit_ms))

        return min(limits_ms) if limits_ms else None

    def _effective_session_limit_seconds(self) -> float:
        effective_limit_ms = self._effective_session_limit_ms()
        if effective_limit_ms is None:
            return 0.0
        return effective_limit_ms / 1000.0

    def _start_session_guard_loop(
        self,
        event_sink: EventSink,
        session_id: UUID,
        *,
        owner_token: int,
    ) -> None:
        if getattr(self, "_session_guard_task", None) is not None:
            return
        self._session_guard_running = True
        self._session_guard_warning_emitted = False

        async def _guard() -> None:
            try:
                while bool(getattr(self, "_session_guard_running", False)):
                    active = self.sessions.active
                    if active is None or active.session_id != session_id:
                        break

                    if self._audio_session_limit_exceeded():
                        logger.info(
                            "Session {} reached audio sample limit; auto-stopping",
                            session_id,
                        )
                        if not self._session_guard_warning_emitted:
                            self._session_guard_warning_emitted = True
                            await self._emit_session_warning(event_sink, session_id, active)
                        await self._stop_active_session(
                            event_sink,
                            session_id,
                            owner_token=owner_token,
                            post_roll_secs=0.0,
                        )
                        break

                    # The wall-clock guard remains a hard ceiling even if the audio
                    # callback has not yet accumulated the equivalent sample cap.
                    session_age_limit_ms = getattr(self, "_session_age_limit_ms", None)
                    duration_limit_reached = session_age_limit_ms is not None and (
                        active.audio_duration_ms >= int(session_age_limit_ms)
                    )
                    if duration_limit_reached:
                        if not self._session_guard_warning_emitted:
                            self._session_guard_warning_emitted = True
                            await self._emit_session_warning(event_sink, session_id, active)
                        logger.info(
                            "Session {} reached wall-clock limit; auto-stopping",
                            session_id,
                        )
                        await self._stop_active_session(
                            event_sink,
                            session_id,
                            owner_token=owner_token,
                            post_roll_secs=0.0,
                        )
                        break

                    # At 80%: emit warning so overlay turns amber.
                    if not self._session_guard_warning_emitted and self._session_guard_warning_due(
                        active
                    ):
                        self._session_guard_warning_emitted = True
                        await self._emit_session_warning(event_sink, session_id, active)

                    await _REAL_ASYNCIO_SLEEP(SESSION_GUARD_POLL_SECS)
            except asyncio.CancelledError:
                raise
            finally:
                if getattr(self, "_session_guard_task", None) is asyncio.current_task():
                    self._session_guard_task = None
                self._session_guard_running = False

        self._session_guard_task = asyncio.create_task(_guard())

    def _stop_session_guard_loop(self) -> None:
        task = getattr(self, "_session_guard_task", None)
        if task is None:
            return
        self._session_guard_running = False
        self._session_guard_task = None
        if not task.done() and task is not asyncio.current_task():
            task.cancel()

    async def _send_error(
        self, event_sink: EventSink, session_id: UUID | None, code: ErrorCode, message: str
    ) -> None:
        await event_sink.emit(SessionErrorEvent(session_id=session_id, code=code, message=message))

    def _session_lock_for_runtime(self) -> asyncio.Lock:
        lock = getattr(self, "_session_lock", None)
        if lock is None:
            lock = asyncio.Lock()
            self._session_lock = lock
        return lock

    def _inference_lock_for_runtime(self) -> asyncio.Lock:
        lock = getattr(self, "_inference_lock", None)
        if lock is None:
            lock = asyncio.Lock()
            self._inference_lock = lock
        return lock

    async def _transcribe_samples_serialized(self, samples: np.ndarray) -> str:
        loop = asyncio.get_running_loop()
        async with self._inference_lock_for_runtime():
            inference = loop.run_in_executor(
                None,
                partial(
                    self.transcriber.transcribe_samples,
                    samples,
                    sample_rate=self.audio.sample_rate,
                ),
            )
            try:
                return await asyncio.shield(inference)
            except asyncio.CancelledError:
                try:
                    await inference
                except Exception as exc:  # noqa: BLE001
                    logger.debug(
                        "Cancelled inference finished with {} before releasing gate",
                        exc.__class__.__name__,
                    )
                raise

    def _clear_overlay_session_runtime(self, session_id: UUID) -> None:
        overlay_stabilizers = getattr(self, "_overlay_interim_stabilizer_by_session", None)
        if isinstance(overlay_stabilizers, dict):
            overlay_stabilizers.pop(session_id, None)

    def _overlay_interim_stabilizer(
        self,
        session_id: UUID,
    ) -> OverlayInterimTranscriptStabilizer:
        overlay_stabilizers = getattr(self, "_overlay_interim_stabilizer_by_session", None)
        if not isinstance(overlay_stabilizers, dict):
            overlay_stabilizers = {}
            self._overlay_interim_stabilizer_by_session = overlay_stabilizers
        stabilizer = overlay_stabilizers.get(session_id)
        if stabilizer is None:
            stabilizer = OverlayInterimTranscriptStabilizer()
            overlay_stabilizers[session_id] = stabilizer
        return stabilizer

    def _overlay_interim_context(
        self,
        session_id: UUID,
        context_samples: int,
    ) -> OverlayInterimTranscriptContext:
        return OverlayInterimTranscriptContext(
            session_id=session_id,
            context_samples=context_samples,
            sample_rate=int(self.audio.sample_rate),
        )

    async def _emit_interim_state(
        self,
        event_sink: EventSink,
        session_id: UUID,
        *,
        state: InterimStateValue,
    ) -> None:
        await event_sink.emit(InterimStateEvent(session_id=session_id, state=state))

    async def _emit_audio_level(
        self,
        event_sink: EventSink,
        session_id: UUID,
        rms: float,
    ) -> None:
        await event_sink.emit(AudioLevelEvent(session_id=session_id, rms=rms))

    async def _collect_interim_text_updates(
        self,
        session_id: UUID,
        ready_chunks: list[np.ndarray],
    ) -> list[str]:
        if not self.settings.overlay_events_enabled:
            return []
        if not ready_chunks:
            return []

        rolling_audio = np.zeros((0,), dtype=np.float32)
        updates: list[str] = []

        for chunk in ready_chunks:
            chunk_audio = np.asarray(chunk, dtype=np.float32).reshape(-1)
            if chunk_audio.size == 0:
                continue
            rolling_audio = append_overlay_interim_context(rolling_audio, chunk_audio)
            stabilizer = self._overlay_interim_stabilizer(session_id)
            source_seq = stabilizer.next_source_seq("stop_replay")
            context = self._overlay_interim_context(session_id, int(rolling_audio.size))
            try:
                candidate = await self._transcribe_samples_serialized(rolling_audio)
            except Exception as exc:  # noqa: BLE001
                logger.debug(
                    "Incremental interim source unavailable for this session: {}",
                    exc.__class__.__name__,
                )
                stabilizer.record_skip(
                    source="stop_replay",
                    source_seq=source_seq,
                    context=context,
                    reason="transcribe_error",
                    error_class=exc.__class__.__name__,
                )
                break
            stabilized = stabilizer.accept(
                "stop_replay",
                source_seq,
                candidate,
                context,
            )
            if stabilized is not None:
                updates.append(stabilized.text)
        return updates

    async def _emit_interim_text(
        self,
        event_sink: EventSink,
        session_id: UUID,
        *,
        text: str,
    ) -> None:
        await event_sink.emit(InterimTextEvent(session_id=session_id, text=text))

    async def _emit_session_ended(
        self,
        event_sink: EventSink,
        session_id: UUID,
        *,
        reason: SessionEndReason,
    ) -> None:
        await event_sink.emit(SessionEndedEvent(session_id=session_id, reason=reason))

    async def _emit_session_warning(
        self,
        event_sink: EventSink,
        session_id: UUID,
        session: Session,
    ) -> None:
        remaining = self._session_remaining_seconds(session)
        limit = self._effective_session_limit_seconds()
        await event_sink.emit(
            SessionWarningEvent(
                session_id=session_id,
                remaining_seconds=remaining,
                limit_seconds=limit,
            )
        )

    async def _emit_live_interim_from_chunk(
        self,
        event_sink: EventSink,
        session_id: UUID,
        chunk: np.ndarray,
    ) -> None:
        if not self.settings.overlay_events_enabled:
            return
        if self._live_interim_failed:
            return
        chunk_audio = np.asarray(chunk, dtype=np.float32).reshape(-1)
        if chunk_audio.size == 0:
            return
        self._live_interim_audio = append_overlay_interim_context(
            self._live_interim_audio,
            chunk_audio,
        )
        stabilizer = self._overlay_interim_stabilizer(session_id)
        source_seq = stabilizer.next_source_seq("live")
        context = self._overlay_interim_context(session_id, int(self._live_interim_audio.size))
        try:
            candidate = await self._transcribe_samples_serialized(self._live_interim_audio)
        except Exception as exc:  # noqa: BLE001
            logger.debug(
                "Live incremental interim source unavailable for this session: {}",
                exc.__class__.__name__,
            )
            stabilizer.record_skip(
                source="live",
                source_seq=source_seq,
                context=context,
                reason="transcribe_error",
                error_class=exc.__class__.__name__,
            )
            self._live_interim_failed = True
            return
        stabilized = stabilizer.accept(
            "live",
            source_seq,
            candidate,
            context,
        )
        if stabilized is not None:
            await self._emit_interim_text(event_sink, session_id, text=stabilized.text)

    def runtime_truth(self, *, overlay_events_enabled: bool) -> RuntimeTruth:
        return runtime_truth_snapshot(
            self,
            last_trim_outcome=self._tail_trimmer_for_runtime().last_outcome,
            overlay_events_enabled=overlay_events_enabled,
        )

    def runtime_status_state(self) -> RuntimeTruthState:
        active = self.sessions.active
        return RuntimeTruthState(
            state=active.state if active else SessionState.IDLE,
            sessions_active=int(active is not None),
            active_session_age_ms=active.audio_duration_ms if active else None,
        )

    def runtime_status_metrics(
        self,
        *,
        overlay_events_emitted: int,
        overlay_events_dropped: int,
    ) -> RuntimeTruthMetrics:
        return RuntimeTruthMetrics(
            gpu_mem_mb=self._gpu_mem_mb(),
            overlay_events_emitted=overlay_events_emitted,
            overlay_events_dropped=overlay_events_dropped,
            audio_stop_ms=getattr(self, "_last_audio_stop_ms", None),
            finalize_ms=getattr(self, "_last_finalize_ms", None),
            infer_ms=getattr(self, "_last_infer_ms", None),
            send_ms=getattr(self, "_last_send_ms", None),
            last_audio_ms=getattr(self, "_last_audio_ms", None),
            last_infer_ms=getattr(self, "_last_infer_ms", None),
            last_send_ms=getattr(self, "_last_send_ms", None),
        )

    def prepare_vad(self) -> None:
        self._tail_trimmer_for_runtime().prepare()

    def _tail_trimmer_for_runtime(self) -> SealPathTailTrimmer:
        tail_trimmer = getattr(self, "tail_trimmer", None)
        if not isinstance(tail_trimmer, SealPathTailTrimmer):
            tail_trimmer = SealPathTailTrimmer(
                vad_enabled=bool(getattr(self, "_vad_enabled", False)),
                silence_floor_db=float(getattr(self.settings, "silence_floor_db", -40.0)),
                warmup_sample_rate=int(getattr(self.audio, "sample_rate", 16_000)),
            )
            self.tail_trimmer = tail_trimmer
        return tail_trimmer

    def _gpu_mem_mb(self) -> int | None:
        try:
            import torch
        except ImportError:  # pragma: no cover - inference extra not installed
            return None

        effective_device = str(getattr(self, "_effective_device", ""))
        if not effective_device.startswith("cuda"):
            return None
        if not torch.cuda.is_available():
            return None

        device_index: int | None = None
        if ":" in effective_device:
            _, suffix = effective_device.split(":", 1)
            if suffix.isdigit():
                device_index = int(suffix)

        reserved_bytes = torch.cuda.memory_reserved(device_index or 0)
        return int(reserved_bytes / (1024 * 1024))

    async def _finalise_transcription(self, audio_samples: np.ndarray) -> tuple[str, int]:
        # The full capture buffer is the only authoritative source for final decode.
        loop = asyncio.get_running_loop()
        tail_trimmer = self._tail_trimmer_for_runtime()
        trim_outcome = await loop.run_in_executor(
            None,
            partial(tail_trimmer.trim, audio_samples, self.audio.sample_rate),
        )
        trimmed = trim_outcome.samples
        effective_device = str(getattr(self, "_effective_device", self.settings.device))
        if trimmed.size == 0:
            logger.info("Skipping offline transcription: silence trimming removed all samples")
            _release_cuda_cache(effective_device)
            return "", 0
        infer_started = time.perf_counter()
        try:
            text = await self._transcribe_samples_serialized(trimmed)
            infer_ms = int((time.perf_counter() - infer_started) * 1000)
            return text, infer_ms
        finally:
            _release_cuda_cache(effective_device)

    def _start_stream_drain_loop(self, event_sink: EventSink, session_id: UUID) -> None:
        if self._stream_drain_task is not None:
            return
        self._stream_drain_running = True

        async def _drain() -> None:
            while self._stream_drain_running:
                audio_levels = self.audio.take_audio_levels()
                if audio_levels:
                    await self._emit_audio_level(event_sink, session_id, max(audio_levels))
                chunks = self.audio.take_stream_chunks()
                if self._active_stream:
                    for chunk in chunks:
                        self._active_stream.feed(chunk)
                        await self._emit_live_interim_from_chunk(event_sink, session_id, chunk)
                await asyncio.sleep(0.05)

        self._stream_drain_task = asyncio.create_task(_drain())

    async def _stop_stream_drain_loop(self) -> None:
        if self._stream_drain_task is None:
            return
        self._stream_drain_running = False
        task = self._stream_drain_task
        self._stream_drain_task = None
        if task is asyncio.current_task():
            return
        if not task.done():
            task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            return
        except Exception as exc:  # noqa: BLE001
            logger.debug(
                "Stream drain loop stopped with {} during shutdown",
                exc.__class__.__name__,
            )


__all__ = [
    "AbortSessionIntent",
    "SessionOrchestrator",
    "StartSessionIntent",
    "StopSessionIntent",
]
