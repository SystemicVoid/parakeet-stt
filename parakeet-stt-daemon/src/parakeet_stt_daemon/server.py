"""FastAPI-based WebSocket server wrapping the Parakeet audio pipeline."""

from __future__ import annotations

import asyncio
import time
from contextlib import asynccontextmanager
from datetime import UTC, datetime
from functools import partial
from typing import Literal
from uuid import UUID

import numpy as np
from fastapi import FastAPI, WebSocket, WebSocketDisconnect
from fastapi.responses import JSONResponse
from loguru import logger

from .audio import AudioInput
from .config import ServerSettings
from .messages import (
    AbortSession,
    AudioLevelMessage,
    ClientMessageType,
    ErrorMessage,
    FinalResult,
    InterimStateMessage,
    InterimStateValue,
    InterimTextMessage,
    ParsedMessage,
    SessionEndedMessage,
    SessionStarted,
    StartSession,
    StatusMessage,
    StopSession,
    parse_client_message,
)
from .model import (
    ParakeetStreamingSession,
    ParakeetStreamingTranscriber,
    ParakeetTranscriber,
    load_parakeet_model,
)
from .session import SessionBusyError, SessionManager, SessionNotFoundError, SessionState

ErrorCode = Literal[
    "SESSION_BUSY",
    "SESSION_NOT_FOUND",
    "SESSION_ABORTED",
    "AUDIO_DEVICE",
    "MODEL",
    "INVALID_REQUEST",
    "UNEXPECTED",
]
OVERLAY_INTERIM_CONTEXT_WINDOW_SECS = 2.0
OVERLAY_SESSION_STATE_CACHE_LIMIT = 128


class DaemonServer:
    """Coordinate session state and translate WebSocket messages into actions."""

    def __init__(self, settings: ServerSettings) -> None:
        self.settings = settings
        self.sessions = SessionManager()
        self.audio = AudioInput(
            sample_rate=16_000,
            channels=1,
            dtype="float32",
            device=settings.mic_device,
        )
        self.model = load_parakeet_model(device=settings.device)
        self.transcriber = ParakeetTranscriber(self.model)
        self._requested_device = str(settings.device)
        self._effective_device = str(
            getattr(self.model, "_parakeet_effective_device", self._requested_device)
        )
        self._transcribe_lock = asyncio.Lock()
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
        self._last_audio_ms: int | None = None
        self._last_audio_stop_ms: int | None = None
        self._last_finalize_ms: int | None = None
        self._last_infer_ms: int | None = None
        self._last_send_ms: int | None = None
        self._live_interim_audio = np.zeros((0,), dtype=np.float32)
        self._live_interim_failed = False
        self._vad_model: object | None = None
        self._vad_enabled = bool(settings.vad_enabled)
        self._vad_failure_reason: str | None = None
        self._vad_load_attempted = False
        if settings.streaming_enabled:
            chunk_samples = int(settings.chunk_secs * self.audio.sample_rate)
            self.audio.configure_stream_chunk_size(chunk_samples)
        self._overlay_event_seq_by_session: dict[UUID, int] = {}
        self._overlay_last_interim_text_by_session: dict[UUID, str] = {}
        self._overlay_state_by_session: dict[UUID, str] = {}
        self._overlay_events_emitted = 0
        self._overlay_events_dropped = 0
        self._websocket_send_locks: dict[int, asyncio.Lock] = {}

    async def handle_websocket(self, websocket: WebSocket) -> None:
        await websocket.accept()
        if self.settings.shared_secret:
            header_secret = websocket.headers.get("x-parakeet-secret")
            if header_secret != self.settings.shared_secret:
                await websocket.close(code=4401)
                return

        logger.info("WebSocket client connected from {}", websocket.client)
        try:
            while True:
                raw = await websocket.receive_json()
                try:
                    parsed = parse_client_message(raw)
                except Exception as exc:  # noqa: BLE001
                    logger.warning("Failed to parse client message: {}", exc)
                    await self._send_error(websocket, None, "INVALID_REQUEST", str(exc))
                    continue

                await self._dispatch(websocket, parsed)
        except WebSocketDisconnect:
            logger.info("WebSocket client disconnected: {}", websocket.client)
            active = self.sessions.active
            expected_session_id = active.session_id if active else None
            await self._cleanup_active_session(
                "websocket disconnected",
                expected_session_id=expected_session_id,
                require_session_match=True,
            )
        except Exception as exc:  # noqa: BLE001
            logger.exception("Unhandled error in WebSocket handler: {}", exc)
            await self._cleanup_active_session(
                f"websocket handler exception: {exc.__class__.__name__}"
            )
            try:
                await self._send_error(websocket, None, "UNEXPECTED", str(exc))
            except Exception as send_exc:  # noqa: BLE001
                logger.debug("Failed to send error after websocket handler exception: {}", send_exc)
        finally:
            websocket_send_locks = getattr(self, "_websocket_send_locks", None)
            if isinstance(websocket_send_locks, dict):
                websocket_send_locks.pop(id(websocket), None)

    async def _dispatch(self, websocket: WebSocket, parsed: ParsedMessage) -> None:
        if parsed.kind is ClientMessageType.START_SESSION:
            assert isinstance(parsed.model, StartSession)
            await self._handle_start(websocket, parsed.model)
        elif parsed.kind is ClientMessageType.STOP_SESSION:
            assert isinstance(parsed.model, StopSession)
            await self._handle_stop(websocket, parsed.model)
        elif parsed.kind is ClientMessageType.ABORT_SESSION:
            assert isinstance(parsed.model, AbortSession)
            await self._handle_abort(websocket, parsed.model)
        else:  # pragma: no cover
            await self._send_error(websocket, None, "INVALID_REQUEST", "Unsupported message")

    async def _handle_start(self, websocket: WebSocket, message: StartSession) -> None:
        logger.debug("start_session received: {}", message)
        try:
            session = await self.sessions.start_session(message.session_id)
        except SessionBusyError:
            await self._send_error(
                websocket, message.session_id, "SESSION_BUSY", "A session is already active"
            )
            return
        try:
            self._live_interim_audio = np.zeros((0,), dtype=np.float32)
            self._live_interim_failed = False
            self._clear_overlay_session_runtime(message.session_id)
            self._set_overlay_session_state(message.session_id, "active")
            self.audio.start_session()
            if self.streaming_transcriber:
                self._active_stream = self.streaming_transcriber.start_session(
                    self.audio.sample_rate
                )
                self._start_stream_drain_loop(websocket, message.session_id)

            response = SessionStarted(
                session_id=message.session_id,
                ts=datetime.now(tz=UTC),
                mic_device=str(self.settings.mic_device) if self.settings.mic_device else None,
                lang=message.preferred_lang,
            )
            await self._send_message(websocket, response.model_dump(mode="json"))
            await self._emit_interim_state(
                websocket,
                message.session_id,
                state=InterimStateValue.LISTENING,
            )
        except WebSocketDisconnect:
            await self._cleanup_active_session(
                "start_session websocket disconnected",
                expected_session_id=message.session_id,
            )
            raise
        except Exception as exc:  # noqa: BLE001
            logger.exception("Failed to start session {}: {}", message.session_id, exc)
            await self._cleanup_active_session(
                f"start_session rollback: {exc.__class__.__name__}",
                expected_session_id=message.session_id,
            )
            try:
                await self._send_error(
                    websocket,
                    message.session_id,
                    "UNEXPECTED",
                    "Failed to start session",
                )
            except Exception as send_exc:  # noqa: BLE001
                logger.debug("Failed to send start_session error response: {}", send_exc)
            return
        logger.info("Session {} started", session.session_id)

    async def _handle_stop(self, websocket: WebSocket, message: StopSession) -> None:
        logger.debug("stop_session received: {}", message)
        await asyncio.sleep(0.25)  # brief post-roll to capture tail audio before stopping
        async with self._transcribe_lock:
            try:
                session = await self.sessions.stop_session(message.session_id)
            except SessionNotFoundError:
                await self._send_error(
                    websocket, message.session_id, "SESSION_NOT_FOUND", "No matching active session"
                )
                return
            await self._emit_interim_state(
                websocket,
                session.session_id,
                state=InterimStateValue.PROCESSING,
            )
            audio_stop_started = time.perf_counter()
            audio_samples, ready_chunks, _tail = self.audio.stop_session_with_streaming()
            self._stop_stream_drain_loop()
            # Final correctness must come from the capture layer's canonical buffer,
            # not whatever the drain task managed to mirror into `_active_stream`.
            self._active_stream = None
            audio_stop_ms = int((time.perf_counter() - audio_stop_started) * 1000)
            audio_duration_raw = len(audio_samples) / self.audio.sample_rate
            audio_ms = int(audio_duration_raw * 1000)

            if audio_samples.size == 0:
                self._set_overlay_session_state(session.session_id, "terminal")
                await self._send_error(
                    websocket, session.session_id, "AUDIO_DEVICE", "No audio captured for session"
                )
                await self._emit_session_ended(websocket, session.session_id, reason="error")
                await self.sessions.clear(session.session_id)
                self._clear_overlay_session_runtime(session.session_id)
                self._live_interim_audio = np.zeros((0,), dtype=np.float32)
                self._live_interim_failed = False
                return

            finalize_ms: int | None = None
            infer_ms: int | None = None
            try:
                interim_updates = await self._collect_interim_text_updates(ready_chunks)
                if interim_updates:
                    await self._emit_interim_state(
                        websocket,
                        session.session_id,
                        state=InterimStateValue.INTERIM,
                    )
                    for interim_text in interim_updates:
                        await self._emit_interim_text(
                            websocket,
                            session.session_id,
                            text=interim_text,
                        )
                await self._emit_interim_state(
                    websocket,
                    session.session_id,
                    state=InterimStateValue.FINALIZING,
                )
                finalize_started = time.perf_counter()
                text, infer_ms = await self._finalise_transcription(audio_samples)
                finalize_ms = int((time.perf_counter() - finalize_started) * 1000)
            except Exception as exc:  # noqa: BLE001
                logger.exception("Failed to transcribe session {}: {}", session.session_id, exc)
                self._set_overlay_session_state(session.session_id, "terminal")
                await self._send_error(
                    websocket, session.session_id, "MODEL", "Transcription failed"
                )
                await self._emit_session_ended(websocket, session.session_id, reason="error")
                await self.sessions.clear(session.session_id)
                self._clear_overlay_session_runtime(session.session_id)
                self._live_interim_audio = np.zeros((0,), dtype=np.float32)
                self._live_interim_failed = False
                return

            latency_ms = int((datetime.now(tz=UTC) - session.last_updated).total_seconds() * 1000)
            completion = FinalResult(
                session_id=session.session_id,
                text=text,
                latency_ms=latency_ms,
                audio_ms=audio_ms,
                lang=self.settings.language,
                confidence=None,
            )
            self._set_overlay_session_state(session.session_id, "terminal")
            send_started = datetime.now(tz=UTC)
            await self._send_message(websocket, completion.model_dump(mode="json"))
            send_ms = int((datetime.now(tz=UTC) - send_started).total_seconds() * 1000)
            await self._emit_session_ended(websocket, session.session_id, reason="final")
            await self.sessions.clear(session.session_id)
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
                "chars_per_sec={:.1f}, live_session_helper_active={}, "
                "live_session_helper_scope={}, stream_fallback_reason={}, "
                "finalization_mode={}, final_audio_source={}, tail_trim_mode={}, "
                "vad_enabled={}, vad_active={}, vad_fallback_reason={}",
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
                self._stream_helper_active(),
                self._stream_helper_scope(),
                self._stream_fallback_reason(),
                self._finalization_mode(),
                self._final_audio_source(),
                self._tail_trim_mode(),
                bool(getattr(self, "_vad_enabled", False)),
                self._vad_active(),
                self._vad_fallback_reason(),
            )

    async def _handle_abort(self, websocket: WebSocket, message: AbortSession) -> None:
        logger.debug("abort_session received: {}", message)
        cleaned = await self._cleanup_active_session(
            f"abort_session requested ({message.reason})",
            expected_session_id=message.session_id,
            require_session_match=True,
        )
        if cleaned:
            await self._emit_session_ended(websocket, message.session_id, reason="abort")
            code = "SESSION_ABORTED"
            error_message = f"Session aborted: {message.reason}"
        else:
            code = "SESSION_NOT_FOUND"
            error_message = "No matching active session"
        await self._send_error(websocket, message.session_id, code, error_message)

    async def _cleanup_active_session(
        self,
        reason: str,
        expected_session_id: UUID | None = None,
        *,
        require_session_match: bool = False,
    ) -> bool:
        """Reset all runtime state tied to an active session."""
        async with self._transcribe_lock:
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

            active_session_id = active.session_id if active else None
            if active_session_id is not None:
                logger.warning("Cleaning up active session {} ({})", active_session_id, reason)
                self._set_overlay_session_state(active_session_id, "terminal")
            else:
                logger.debug("Cleaning residual runtime state with no active session ({})", reason)
            self.audio.abort_session()
            self._stop_stream_drain_loop()
            if active_session_id is not None:
                await self.sessions.clear(active_session_id)
                self._clear_overlay_session_runtime(active_session_id)
            self._active_stream = None
            self._live_interim_audio = np.zeros((0,), dtype=np.float32)
            self._live_interim_failed = False
            return active_session_id is not None

    def _append_overlay_interim_context(
        self,
        existing: np.ndarray,
        chunk_audio: np.ndarray,
    ) -> np.ndarray:
        if existing.size == 0:
            combined = np.array(chunk_audio, copy=True)
        else:
            combined = np.concatenate((existing, chunk_audio))
        max_samples = max(1, int(self.audio.sample_rate * OVERLAY_INTERIM_CONTEXT_WINDOW_SECS))
        if combined.size > max_samples:
            return combined[-max_samples:]
        return combined

    async def _send_error(
        self, websocket: WebSocket, session_id: UUID | None, code: ErrorCode, message: str
    ) -> None:
        err = ErrorMessage(session_id=session_id, code=code, message=message)
        await self._send_message(websocket, err.model_dump(mode="json"))

    def _send_lock_for_websocket(self, websocket: WebSocket) -> asyncio.Lock:
        websocket_send_locks = getattr(self, "_websocket_send_locks", None)
        if not isinstance(websocket_send_locks, dict):
            websocket_send_locks = {}
            self._websocket_send_locks = websocket_send_locks
        lock = websocket_send_locks.get(id(websocket))
        if lock is None:
            lock = asyncio.Lock()
            websocket_send_locks[id(websocket)] = lock
        return lock

    async def _send_message(self, websocket: WebSocket, payload: dict) -> None:
        async with self._send_lock_for_websocket(websocket):
            await websocket.send_json(payload)

    def _set_overlay_session_state(self, session_id: UUID, state: str) -> None:
        overlay_states = getattr(self, "_overlay_state_by_session", None)
        if not isinstance(overlay_states, dict):
            overlay_states = {}
            self._overlay_state_by_session = overlay_states
        overlay_states.pop(session_id, None)
        overlay_states[session_id] = state
        while len(overlay_states) > OVERLAY_SESSION_STATE_CACHE_LIMIT:
            oldest = next(iter(overlay_states))
            overlay_states.pop(oldest)

    def _overlay_session_state(self, session_id: UUID) -> str | None:
        overlay_states = getattr(self, "_overlay_state_by_session", None)
        if not isinstance(overlay_states, dict):
            return None
        return overlay_states.get(session_id)

    def _clear_overlay_session_runtime(self, session_id: UUID) -> None:
        overlay_seq_by_session = getattr(self, "_overlay_event_seq_by_session", None)
        if isinstance(overlay_seq_by_session, dict):
            overlay_seq_by_session.pop(session_id, None)
        overlay_last_text = getattr(self, "_overlay_last_interim_text_by_session", None)
        if isinstance(overlay_last_text, dict):
            overlay_last_text.pop(session_id, None)

    def _next_overlay_seq(self, session_id: UUID) -> int:
        overlay_seq_by_session = getattr(self, "_overlay_event_seq_by_session", None)
        if not isinstance(overlay_seq_by_session, dict):
            overlay_seq_by_session = {}
            self._overlay_event_seq_by_session = overlay_seq_by_session
        current = overlay_seq_by_session.get(session_id, 0)
        overlay_seq_by_session[session_id] = current + 1
        return current

    async def _send_overlay_message(
        self,
        websocket: WebSocket,
        session_id: UUID,
        payload: dict,
        *,
        allow_terminal: bool = False,
        next_state: str | None = None,
    ) -> bool:
        if not self.settings.overlay_events_enabled:
            return False
        try:
            async with self._send_lock_for_websocket(websocket):
                current_state = self._overlay_session_state(session_id)
                if current_state == "ended" or (current_state == "terminal" and not allow_terminal):
                    self._overlay_events_dropped = (
                        int(getattr(self, "_overlay_events_dropped", 0)) + 1
                    )
                    logger.debug(
                        "Dropping overlay event {} for session {} after terminal transition",
                        payload.get("type"),
                        session_id,
                    )
                    return False
                await websocket.send_json(payload)
            if next_state is not None:
                self._set_overlay_session_state(session_id, next_state)
            self._overlay_events_emitted = int(getattr(self, "_overlay_events_emitted", 0)) + 1
            return True
        except Exception as exc:  # noqa: BLE001
            # Overlay events are display-only signals and must never break
            # transcription/injection flow.
            self._overlay_events_dropped = int(getattr(self, "_overlay_events_dropped", 0)) + 1
            logger.debug("Dropping overlay event after send failure: {}", exc)
            return False

    async def _emit_interim_state(
        self,
        websocket: WebSocket,
        session_id: UUID,
        *,
        state: InterimStateValue,
    ) -> None:
        if not self.settings.overlay_events_enabled:
            return
        message = InterimStateMessage(
            session_id=session_id,
            seq=self._next_overlay_seq(session_id),
            state=state,
        )
        await self._send_overlay_message(websocket, session_id, message.model_dump(mode="json"))

    async def _emit_audio_level(
        self,
        websocket: WebSocket,
        session_id: UUID,
        rms: float,
    ) -> None:
        if not self.settings.overlay_events_enabled:
            return
        if not np.isfinite(rms):
            return
        level_db = 20.0 * np.log10(max(rms, 1e-6))
        if not np.isfinite(level_db):
            return
        message = AudioLevelMessage(session_id=session_id, level_db=level_db)
        await self._send_overlay_message(websocket, session_id, message.model_dump(mode="json"))

    async def _collect_interim_text_updates(self, ready_chunks: list[np.ndarray]) -> list[str]:
        if not self.settings.overlay_events_enabled:
            return []
        if not ready_chunks:
            return []

        loop = asyncio.get_running_loop()
        rolling_audio = np.zeros((0,), dtype=np.float32)
        updates: list[str] = []
        last_text = ""

        for chunk in ready_chunks:
            chunk_audio = np.asarray(chunk, dtype=np.float32).reshape(-1)
            if chunk_audio.size == 0:
                continue
            rolling_audio = self._append_overlay_interim_context(rolling_audio, chunk_audio)
            try:
                candidate = await loop.run_in_executor(
                    None,
                    partial(
                        self.transcriber.transcribe_samples,
                        rolling_audio,
                        sample_rate=self.audio.sample_rate,
                    ),
                )
            except Exception as exc:  # noqa: BLE001
                logger.debug(
                    "Incremental interim source unavailable for this session: {}",
                    exc.__class__.__name__,
                )
                break
            normalized = " ".join(candidate.split()).strip()
            if not normalized or normalized == last_text:
                continue
            updates.append(normalized)
            last_text = normalized
        return updates

    async def _emit_interim_text(
        self,
        websocket: WebSocket,
        session_id: UUID,
        *,
        text: str,
    ) -> None:
        if not self.settings.overlay_events_enabled:
            return
        normalized = " ".join(text.split()).strip()
        if not normalized:
            return
        last_by_session = getattr(self, "_overlay_last_interim_text_by_session", None)
        if isinstance(last_by_session, dict) and last_by_session.get(session_id) == normalized:
            return
        message = InterimTextMessage(
            session_id=session_id,
            seq=self._next_overlay_seq(session_id),
            text=normalized,
        )
        if await self._send_overlay_message(websocket, session_id, message.model_dump(mode="json")):
            if not isinstance(last_by_session, dict):
                self._overlay_last_interim_text_by_session = {}
                last_by_session = self._overlay_last_interim_text_by_session
            last_by_session[session_id] = normalized

    async def _emit_session_ended(
        self,
        websocket: WebSocket,
        session_id: UUID,
        *,
        reason: Literal["final", "abort", "error"],
    ) -> None:
        if not self.settings.overlay_events_enabled:
            return
        message = SessionEndedMessage(session_id=session_id, reason=reason)
        await self._send_overlay_message(
            websocket,
            session_id,
            message.model_dump(mode="json"),
            allow_terminal=True,
            next_state="ended",
        )
        overlay_last_text = getattr(self, "_overlay_last_interim_text_by_session", None)
        if isinstance(overlay_last_text, dict):
            overlay_last_text.pop(session_id, None)

    async def _emit_live_interim_from_chunk(
        self,
        websocket: WebSocket,
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
        self._live_interim_audio = self._append_overlay_interim_context(
            self._live_interim_audio,
            chunk_audio,
        )
        loop = asyncio.get_running_loop()
        try:
            candidate = await loop.run_in_executor(
                None,
                partial(
                    self.transcriber.transcribe_samples,
                    self._live_interim_audio,
                    sample_rate=self.audio.sample_rate,
                ),
            )
        except Exception as exc:  # noqa: BLE001
            logger.debug(
                "Live incremental interim source unavailable for this session: {}",
                exc.__class__.__name__,
            )
            self._live_interim_failed = True
            return
        await self._emit_interim_text(websocket, session_id, text=candidate)

    def status(self) -> StatusMessage:
        active = self.sessions.active
        state = active.state if active else SessionState.IDLE
        requested_device = getattr(self, "_requested_device", str(self.settings.device))
        effective_device = getattr(self, "_effective_device", requested_device)
        return StatusMessage(
            state=state.value,
            sessions_active=int(active is not None),
            gpu_mem_mb=self._gpu_mem_mb(),
            device=requested_device,
            effective_device=effective_device,
            streaming_enabled=self.settings.streaming_enabled,
            stream_helper_active=self._stream_helper_active(),
            stream_helper_scope=self._stream_helper_scope(),
            stream_fallback_reason=self._stream_fallback_reason(),
            finalization_mode=self._finalization_mode(),
            final_audio_source=self._final_audio_source(),
            tail_trim_mode=self._tail_trim_mode(),
            vad_enabled=bool(getattr(self, "_vad_enabled", False)),
            vad_active=self._vad_active(),
            vad_fallback_reason=self._vad_fallback_reason(),
            overlay_events_enabled=self.settings.overlay_events_enabled,
            overlay_events_emitted=getattr(self, "_overlay_events_emitted", 0),
            overlay_events_dropped=getattr(self, "_overlay_events_dropped", 0),
            chunk_secs=self.settings.chunk_secs if self.settings.streaming_enabled else None,
            active_session_age_ms=active.audio_duration_ms if active else None,
            audio_stop_ms=getattr(self, "_last_audio_stop_ms", None),
            finalize_ms=getattr(self, "_last_finalize_ms", None),
            infer_ms=getattr(self, "_last_infer_ms", None),
            send_ms=getattr(self, "_last_send_ms", None),
            last_audio_ms=getattr(self, "_last_audio_ms", None),
            last_infer_ms=getattr(self, "_last_infer_ms", None),
            last_send_ms=getattr(self, "_last_send_ms", None),
        )

    def _stream_helper_active(self) -> bool:
        if not self.settings.streaming_enabled:
            return False
        if self.streaming_transcriber is None:
            return False
        return self.streaming_transcriber.helper_active

    def _stream_helper_scope(self) -> Literal["live_session_only"]:
        return "live_session_only"

    def _stream_fallback_reason(self) -> str | None:
        if not self.settings.streaming_enabled:
            return None
        if self.streaming_transcriber is None:
            return "streaming_transcriber_unavailable"
        return self.streaming_transcriber.fallback_reason

    def _finalization_mode(self) -> Literal["offline_seal"]:
        return "offline_seal"

    def _final_audio_source(self) -> Literal["canonical_session_audio"]:
        return "canonical_session_audio"

    def _tail_trim_mode(self) -> Literal["rms", "vad"]:
        return "vad" if self._vad_active() else "rms"

    def _vad_active(self) -> bool:
        if not bool(getattr(self, "_vad_enabled", False)):
            return False
        return getattr(self, "_vad_model", None) is not None and self._vad_fallback_reason() is None

    def _vad_fallback_reason(self) -> str | None:
        if not bool(getattr(self, "_vad_enabled", False)):
            return None
        failure_reason = getattr(self, "_vad_failure_reason", None)
        if failure_reason is not None:
            return str(failure_reason)
        if getattr(self, "_vad_model", None) is not None:
            return None
        if not bool(getattr(self, "_vad_load_attempted", False)):
            return "load_not_attempted"
        return "model_unavailable"

    def _format_vad_failure_reason(
        self,
        stage: Literal["load_failed", "runtime_failed", "warmup_failed"],
        exc: Exception,
    ) -> str:
        if isinstance(exc, ModuleNotFoundError):
            missing_dependency = exc.name or "unknown"
            return f"{stage}:missing_dependency:{missing_dependency}"
        return f"{stage}:{exc.__class__.__name__}"

    def _load_vad_model(self) -> object:
        from silero_vad import load_silero_vad

        return load_silero_vad(onnx=True)

    def _ensure_vad_ready(self) -> bool:
        if not self._vad_enabled:
            return False
        if (
            getattr(self, "_vad_model", None) is not None
            and getattr(self, "_vad_failure_reason", None) is None
        ):
            return True
        if getattr(self, "_vad_failure_reason", None) is not None:
            return False

        self._vad_load_attempted = True
        try:
            self._vad_model = self._load_vad_model()
        except (ImportError, OSError, RuntimeError, TypeError, ValueError) as exc:
            self._vad_failure_reason = self._format_vad_failure_reason("load_failed", exc)
            logger.warning("Silero VAD unavailable; falling back to RMS trim: {}", exc)
            return False
        return True

    def _run_vad_inference(self, samples: np.ndarray, sample_rate: int) -> np.ndarray:
        import torch
        from silero_vad import get_speech_timestamps

        if self._vad_model is None:  # pragma: no cover - guarded by _ensure_vad_ready
            raise RuntimeError("Silero VAD model not loaded")

        audio = samples.astype(np.float32, copy=False)
        waveform = torch.from_numpy(audio)
        speech_spans = get_speech_timestamps(
            waveform,
            self._vad_model,
            sampling_rate=sample_rate,
        )
        if not speech_spans:
            return np.zeros((0,), dtype=np.float32)
        end_sample = int(speech_spans[-1].get("end", 0))
        end_sample = max(0, min(end_sample, audio.size))
        return audio[:end_sample]

    def prepare_vad(self) -> None:
        if not self._vad_enabled:
            return
        if not self._ensure_vad_ready():
            return
        try:
            _ = self._run_vad_inference(
                np.zeros((self.audio.sample_rate,), dtype=np.float32),
                self.audio.sample_rate,
            )
        except (ImportError, OSError, RuntimeError, TypeError, ValueError) as exc:
            self._vad_failure_reason = self._format_vad_failure_reason("warmup_failed", exc)
            self._vad_model = None
            logger.warning("Silero VAD warmup failed; disabling VAD tail trim: {}", exc)

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
        trimmed = await loop.run_in_executor(
            None,
            partial(self._trim_tail_silence, audio_samples, self.audio.sample_rate),
        )
        if trimmed.size == 0:
            logger.info("Skipping offline transcription: silence trimming removed all samples")
            return "", 0
        infer_started = time.perf_counter()
        text = await loop.run_in_executor(
            None,
            partial(
                self.transcriber.transcribe_samples,
                trimmed,
                sample_rate=self.audio.sample_rate,
            ),
        )
        infer_ms = int((time.perf_counter() - infer_started) * 1000)
        return text, infer_ms

    def _start_stream_drain_loop(self, websocket: WebSocket, session_id: UUID) -> None:
        if self._stream_drain_task is not None:
            return
        self._stream_drain_running = True

        async def _drain() -> None:
            while self._stream_drain_running:
                audio_levels = self.audio.take_audio_levels()
                if audio_levels:
                    await self._emit_audio_level(websocket, session_id, max(audio_levels))
                chunks = self.audio.take_stream_chunks()
                if self._active_stream:
                    for chunk in chunks:
                        self._active_stream.feed(chunk)
                        await self._emit_live_interim_from_chunk(websocket, session_id, chunk)
                await asyncio.sleep(0.05)

        self._stream_drain_task = asyncio.create_task(_drain())

    def _stop_stream_drain_loop(self) -> None:
        if self._stream_drain_task is None:
            return
        self._stream_drain_running = False
        task = self._stream_drain_task
        self._stream_drain_task = None
        if not task.done():
            task.cancel()

    def _trim_tail_silence(
        self, samples: np.ndarray, sample_rate: int, window_ms: int = 50
    ) -> np.ndarray:
        if samples.size == 0:
            return samples
        if bool(getattr(self, "_vad_enabled", False)):
            trimmed = self._trim_tail_with_vad(samples, sample_rate)
            if trimmed is not None:
                return trimmed

        return self._trim_tail_with_rms(samples, sample_rate, window_ms)

    def _trim_tail_with_vad(self, samples: np.ndarray, sample_rate: int) -> np.ndarray | None:
        if not bool(getattr(self, "_vad_enabled", False)):
            return None
        if not self._ensure_vad_ready():
            return None

        try:
            return self._run_vad_inference(samples, sample_rate)
        except (ImportError, OSError, RuntimeError, TypeError, ValueError) as exc:
            self._vad_failure_reason = self._format_vad_failure_reason("runtime_failed", exc)
            self._vad_model = None
            logger.warning("Silero VAD tail trim failed; falling back to RMS trim: {}", exc)
            return None

    def _trim_tail_with_rms(
        self, samples: np.ndarray, sample_rate: int, window_ms: int = 50
    ) -> np.ndarray:
        window = max(1, int(sample_rate * window_ms / 1000))
        # Clamp to mono array
        audio = samples.astype(np.float32, copy=False)
        idx = audio.size
        floor_db = float(self.settings.silence_floor_db)
        while idx > 0:
            start = max(0, idx - window)
            window_slice = audio[start:idx]
            rms = np.sqrt(np.mean(window_slice**2))
            db = 20 * np.log10(max(rms, 1e-6))
            if db > floor_db:
                break
            idx = start
        return audio[:idx]


def create_app(settings: ServerSettings) -> FastAPI:
    server = DaemonServer(settings)

    @asynccontextmanager
    async def lifespan(app: FastAPI):
        logger.info("Starting audio capture")
        server.audio.start()
        logger.info("Warming Parakeet model on {}", settings.device)
        try:
            await asyncio.to_thread(server.transcriber.warmup)
        except Exception as exc:  # noqa: BLE001
            logger.warning("Model warmup skipped: {}", exc)
        if server._vad_enabled:
            await asyncio.to_thread(server.prepare_vad)
        runtime_degraded = (
            server.settings.streaming_enabled and not server._stream_helper_active()
        ) or (server._vad_enabled and not server._vad_active())
        _log = logger.warning if runtime_degraded else logger.info
        _log(
            "Runtime truth: device_requested={}, device_effective={}, streaming_enabled={}, "
            "live_session_helper_active={}, live_session_helper_scope={}, "
            "stream_fallback_reason={}, finalization_mode={}, final_audio_source={}, "
            "tail_trim_mode={}, vad_enabled={}, vad_active={}, vad_fallback_reason={}, "
            "overlay_events_enabled={}",
            server._requested_device,
            server._effective_device,
            server.settings.streaming_enabled,
            server._stream_helper_active(),
            server._stream_helper_scope(),
            server._stream_fallback_reason(),
            server._finalization_mode(),
            server._final_audio_source(),
            server._tail_trim_mode(),
            server._vad_enabled,
            server._vad_active(),
            server._vad_fallback_reason(),
            server.settings.overlay_events_enabled,
        )
        yield
        logger.info("Stopping audio capture")
        server.audio.stop()

    app = FastAPI(title="Parakeet STT Daemon", version="0.1.0", lifespan=lifespan)

    @app.get("/healthz")
    async def health() -> dict[str, str]:
        return {"status": "ok"}

    if settings.status_enabled:

        @app.get("/status")
        async def status() -> JSONResponse:
            return JSONResponse(server.status().model_dump())

    @app.websocket("/ws")
    async def websocket_endpoint(websocket: WebSocket) -> None:
        await server.handle_websocket(websocket)

    return app


__all__ = ["create_app", "DaemonServer"]
