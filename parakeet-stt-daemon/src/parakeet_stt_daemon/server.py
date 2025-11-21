"""FastAPI-based WebSocket server wrapping the Parakeet audio pipeline."""
from __future__ import annotations

from datetime import datetime, timezone
from uuid import UUID

from fastapi import FastAPI, WebSocket, WebSocketDisconnect
from fastapi.responses import JSONResponse
from loguru import logger

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
from .session import SessionManager, SessionState, SessionBusyError, SessionNotFoundError


class DaemonServer:
    """Coordinate session state and translate WebSocket messages into actions."""

    def __init__(self, settings: ServerSettings) -> None:
        self.settings = settings
        self.sessions = SessionManager()

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
        except Exception as exc:  # noqa: BLE001
            logger.exception("Unhandled error in WebSocket handler: {}", exc)
            await self._send_error(websocket, None, "UNEXPECTED", str(exc))

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

        response = SessionStarted(
            session_id=message.session_id,
            ts=datetime.now(tz=timezone.utc),
            mic_device=str(self.settings.mic_device) if self.settings.mic_device else None,
            lang=message.preferred_lang,
        )
        await websocket.send_json(response.model_dump())
        logger.info("Session {} started", session.session_id)

    async def _handle_stop(self, websocket: WebSocket, message: StopSession) -> None:
        logger.debug("stop_session received: {}", message)
        try:
            session = await self.sessions.stop_session(message.session_id)
        except SessionNotFoundError:
            await self._send_error(
                websocket, message.session_id, "SESSION_BUSY", "No matching active session"
            )
            return

        # Placeholder inference; the actual model wiring will replace this path.
        placeholder_text = "(parakeet model not connected yet)"
        completion = FinalResult(
            session_id=session.session_id,
            text=placeholder_text,
            latency_ms=0,
            audio_ms=session.audio_duration_ms,
            lang=self.settings.language,
            confidence=None,
        )
        await websocket.send_json(completion.model_dump())
        await self.sessions.clear(session.session_id)
        logger.info("Session {} completed", session.session_id)

    async def _handle_abort(self, websocket: WebSocket, message: AbortSession) -> None:
        logger.debug("abort_session received: {}", message)
        await self.sessions.clear(message.session_id)
        await self._send_error(
            websocket,
            message.session_id,
            "SESSION_BUSY",
            f"Session aborted: {message.reason}",
        )

    async def _send_error(
        self, websocket: WebSocket, session_id: UUID | None, code: str, message: str
    ) -> None:
        err = ErrorMessage(session_id=session_id, code=code, message=message)
        await websocket.send_json(err.model_dump())

    def status(self) -> StatusMessage:
        active = self.sessions.active
        state = active.state if active else SessionState.IDLE
        return StatusMessage(
            state=state.value,
            sessions_active=int(active is not None),
            gpu_mem_mb=None,
        )


def create_app(settings: ServerSettings) -> FastAPI:
    server = DaemonServer(settings)
    app = FastAPI(title="Parakeet STT Daemon", version="0.1.0")

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
