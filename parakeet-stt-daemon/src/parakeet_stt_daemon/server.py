"""FastAPI-based WebSocket server wrapping the Parakeet audio pipeline."""

from __future__ import annotations

import asyncio
import tempfile
import wave
from contextlib import asynccontextmanager
from datetime import UTC, datetime
from pathlib import Path
from typing import Literal
from uuid import UUID

import numpy as np
from fastapi import FastAPI, WebSocket, WebSocketDisconnect
from fastapi.responses import JSONResponse
from loguru import logger

try:
    import soundfile as sf
except ImportError:  # pragma: no cover - inference extra not installed
    sf = None  # type: ignore

from .audio import AudioInput
from .config import ServerSettings
from .messages import (
    AbortSession,
    ClientMessageType,
    ErrorMessage,
    FinalResult,
    ParsedMessage,
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

ErrorCode = Literal["SESSION_BUSY", "AUDIO_DEVICE", "MODEL", "UNEXPECTED"]


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
        self._last_infer_ms: int | None = None
        self._last_send_ms: int | None = None
        if settings.streaming_enabled:
            chunk_samples = int(settings.chunk_secs * self.audio.sample_rate)
            self.audio.configure_stream_chunk_size(chunk_samples)

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
                    await self._send_error(websocket, None, "UNEXPECTED", str(exc))
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
            await self._send_error(websocket, None, "UNEXPECTED", "Unsupported message")

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
            self.audio.start_session()
            if self.streaming_transcriber:
                self._active_stream = self.streaming_transcriber.start_session(
                    self.audio.sample_rate
                )
                self._start_stream_drain_loop()

            response = SessionStarted(
                session_id=message.session_id,
                ts=datetime.now(tz=UTC),
                mic_device=str(self.settings.mic_device) if self.settings.mic_device else None,
                lang=message.preferred_lang,
            )
            await websocket.send_json(response.model_dump(mode="json"))
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
                    websocket, message.session_id, "SESSION_BUSY", "No matching active session"
                )
                return
            audio_samples, ready_chunks, tail = self.audio.stop_session_with_streaming()
            self._stop_stream_drain_loop()
            audio_duration_raw = len(audio_samples) / self.audio.sample_rate
            audio_ms = int(audio_duration_raw * 1000)

            if audio_samples.size == 0:
                await self._send_error(
                    websocket, session.session_id, "AUDIO_DEVICE", "No audio captured for session"
                )
                await self.sessions.clear(session.session_id)
                return

            infer_ms: int | None = None
            try:
                infer_started = datetime.now(tz=UTC)
                text = await self._finalise_transcription(audio_samples, ready_chunks, tail)
                infer_ms = int((datetime.now(tz=UTC) - infer_started).total_seconds() * 1000)
            except Exception as exc:  # noqa: BLE001
                logger.exception("Failed to transcribe session {}: {}", session.session_id, exc)
                await self._send_error(
                    websocket, session.session_id, "MODEL", "Transcription failed"
                )
                await self.sessions.clear(session.session_id)
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
            send_started = datetime.now(tz=UTC)
            await websocket.send_json(completion.model_dump(mode="json"))
            send_ms = int((datetime.now(tz=UTC) - send_started).total_seconds() * 1000)
            await self.sessions.clear(session.session_id)
            self._last_audio_ms = audio_ms
            self._last_infer_ms = infer_ms
            self._last_send_ms = send_ms

            # Diagnostic logging for truncation investigation
            text_len = len(text)
            chars_per_sec = text_len / audio_duration_raw if audio_duration_raw > 0 else 0
            logger.info(
                "Session {} completed: audio_raw={:.2f}s, audio_ms={}, latency_ms={}, infer_ms={}, "
                "send_ms={}, text_len={}, chars_per_sec={:.1f}, stream_helper_active={}, "
                "stream_fallback_reason={}",
                session.session_id,
                audio_duration_raw,
                audio_ms,
                latency_ms,
                infer_ms,
                send_ms,
                text_len,
                chars_per_sec,
                self._stream_helper_active(),
                self._stream_fallback_reason(),
            )

    async def _handle_abort(self, websocket: WebSocket, message: AbortSession) -> None:
        logger.debug("abort_session received: {}", message)
        await self._cleanup_active_session(
            f"abort_session requested ({message.reason})",
            expected_session_id=message.session_id,
        )
        await self._send_error(
            websocket,
            message.session_id,
            "SESSION_BUSY",
            f"Session aborted: {message.reason}",
        )

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
            else:
                logger.debug("Cleaning residual runtime state with no active session ({})", reason)
            self.audio.abort_session()
            self._stop_stream_drain_loop()
            if active_session_id is not None:
                await self.sessions.clear(active_session_id)
            self._active_stream = None
            return active_session_id is not None

    async def _send_error(
        self, websocket: WebSocket, session_id: UUID | None, code: ErrorCode, message: str
    ) -> None:
        err = ErrorMessage(session_id=session_id, code=code, message=message)
        await websocket.send_json(err.model_dump(mode="json"))

    def status(self) -> StatusMessage:
        active = self.sessions.active
        state = active.state if active else SessionState.IDLE
        requested_device = getattr(self, "_requested_device", str(self.settings.device))
        effective_device = getattr(self, "_effective_device", requested_device)
        return StatusMessage(
            state=state.value,
            sessions_active=int(active is not None),
            gpu_mem_mb=None,
            device=requested_device,
            effective_device=effective_device,
            streaming_enabled=self.settings.streaming_enabled,
            stream_helper_active=self._stream_helper_active(),
            stream_fallback_reason=self._stream_fallback_reason(),
            chunk_secs=self.settings.chunk_secs if self.settings.streaming_enabled else None,
            active_session_age_ms=active.audio_duration_ms if active else None,
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

    def _stream_fallback_reason(self) -> str | None:
        if not self.settings.streaming_enabled:
            return None
        if self.streaming_transcriber is None:
            return "streaming_transcriber_unavailable"
        return self.streaming_transcriber.fallback_reason

    def _write_wav(self, samples: np.ndarray) -> Path:
        path = Path(tempfile.NamedTemporaryFile(suffix=".wav", delete=False).name)
        if sf is not None:
            sf.write(path, samples, self.audio.sample_rate)
        else:  # pragma: no cover - fallback for dev environments
            self._write_wav_fallback(path, samples)
        return path

    def _write_wav_fallback(self, path: Path, samples: np.ndarray) -> None:
        pcm = (np.clip(samples, -1.0, 1.0) * 32767).astype("<i2")
        with wave.open(str(path), "wb") as wf:
            wf.setnchannels(1)
            wf.setsampwidth(2)
            wf.setframerate(self.audio.sample_rate)
            wf.writeframes(pcm.tobytes())

    async def _finalise_transcription(
        self, audio_samples: np.ndarray, ready_chunks: list[np.ndarray], tail: np.ndarray
    ) -> str:
        if self.streaming_transcriber and self._active_stream:
            try:
                for chunk in ready_chunks:
                    self._active_stream.feed(chunk)
                if tail.size:
                    trimmed_tail = self._trim_tail_silence(tail, self.audio.sample_rate)
                    if trimmed_tail.size:
                        self._active_stream.feed(trimmed_tail)
                return self._active_stream.finalize()
            finally:
                self._active_stream = None

        # Offline fallback: write temp wav and transcribe.
        trimmed = self._trim_tail_silence(audio_samples, self.audio.sample_rate)
        audio_path = self._write_wav(trimmed)
        try:
            return await asyncio.get_running_loop().run_in_executor(
                None, self.transcriber.transcribe_wav, str(audio_path)
            )
        finally:
            audio_path.unlink(missing_ok=True)

    def _start_stream_drain_loop(self) -> None:
        if self._stream_drain_task is not None:
            return
        self._stream_drain_running = True

        async def _drain() -> None:
            while self._stream_drain_running:
                chunks = self.audio.take_stream_chunks()
                if self._active_stream:
                    for chunk in chunks:
                        self._active_stream.feed(chunk)
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
        streaming_degraded = (
            server.settings.streaming_enabled and not server._stream_helper_active()
        )
        _log = logger.warning if streaming_degraded else logger.info
        _log(
            "Runtime truth: device_requested={}, device_effective={}, streaming_enabled={}, "
            "stream_helper_active={}, stream_fallback_reason={}",
            server._requested_device,
            server._effective_device,
            server.settings.streaming_enabled,
            server._stream_helper_active(),
            server._stream_fallback_reason(),
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
