"""FastAPI WebSocket adapter for the Parakeet Session orchestrator."""

from __future__ import annotations

import asyncio
from contextlib import asynccontextmanager
from uuid import UUID

from fastapi import FastAPI, WebSocket, WebSocketDisconnect
from fastapi.responses import JSONResponse
from loguru import logger

from .config import DAEMON_HEALTH_PATH, DAEMON_STATUS_PATH, DAEMON_WS_PATH, ServerSettings
from .events import (
    ErrorCode,
    EventSinkClosed,
    SessionErrorEvent,
    WebSocketEventSink,
    WebSocketEventSinkState,
)
from .messages import (
    AbortSession,
    ClientMessageType,
    ParsedMessage,
    SessionEndReason,
    StartSession,
    StatusMessage,
    StopSession,
    parse_client_message,
)
from .model import _release_cuda_cache
from .runtime_truth_snapshot import format_log_record
from .session_orchestrator import (
    AbortSessionIntent,
    SessionOrchestrator,
    StartSessionIntent,
    StopSessionIntent,
)


class DaemonServer:
    """Adapt WebSocket messages to the in-process SessionOrchestrator."""

    def __init__(self, settings: ServerSettings) -> None:
        self.settings = settings
        self.orchestrator = SessionOrchestrator(settings)
        self.event_sinks = WebSocketEventSinkState(
            overlay_events_enabled=settings.overlay_events_enabled
        )
        self._websocket_send_locks: dict[int, asyncio.Lock] = {}

    async def handle_websocket(self, websocket: WebSocket) -> None:
        owner_token = self._owner_token_for_websocket(websocket)
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
        except (WebSocketDisconnect, EventSinkClosed):
            logger.info("WebSocket client disconnected: {}", websocket.client)
            active = self.orchestrator.sessions.active
            expected_session_id = active.session_id if active else None
            await self.orchestrator._cleanup_active_session(
                "websocket disconnected",
                expected_session_id=expected_session_id,
                expected_owner_token=owner_token,
                require_session_match=True,
                require_owner_match=True,
                event_sink=self._event_sink(websocket),
                session_end_reason=SessionEndReason.ABORT,
            )
        except Exception as exc:  # noqa: BLE001
            logger.exception("Unhandled error in WebSocket handler: {}", exc)
            await self.orchestrator._cleanup_active_session(
                f"websocket handler exception: {exc.__class__.__name__}",
                expected_owner_token=owner_token,
                require_owner_match=True,
                event_sink=self._event_sink(websocket),
                session_end_reason=SessionEndReason.ABORT,
            )
            try:
                await self._send_error(websocket, None, "UNEXPECTED", str(exc))
            except Exception as send_exc:  # noqa: BLE001
                logger.debug("Failed to send error after websocket handler exception: {}", send_exc)
        finally:
            self._websocket_send_locks.pop(id(websocket), None)

    async def _dispatch(self, websocket: WebSocket, parsed: ParsedMessage) -> None:
        owner_token = self._owner_token_for_websocket(websocket)
        event_sink = self._event_sink(websocket)
        if parsed.kind is ClientMessageType.START_SESSION:
            assert isinstance(parsed.model, StartSession)
            await self.orchestrator.start(
                StartSessionIntent(
                    session_id=parsed.model.session_id,
                    owner_token=owner_token,
                    event_sink=event_sink,
                    preferred_lang=parsed.model.preferred_lang,
                )
            )
        elif parsed.kind is ClientMessageType.STOP_SESSION:
            assert isinstance(parsed.model, StopSession)
            await self.orchestrator.stop(
                StopSessionIntent(
                    session_id=parsed.model.session_id,
                    owner_token=owner_token,
                    event_sink=event_sink,
                    post_roll_secs=0.25,
                )
            )
        elif parsed.kind is ClientMessageType.ABORT_SESSION:
            assert isinstance(parsed.model, AbortSession)
            await self.orchestrator.abort(
                AbortSessionIntent(
                    session_id=parsed.model.session_id,
                    owner_token=owner_token,
                    event_sink=event_sink,
                    reason=parsed.model.reason,
                )
            )
        else:  # pragma: no cover
            await self._send_error(websocket, None, "INVALID_REQUEST", "Unsupported message")

    def _owner_token_for_websocket(self, websocket: WebSocket) -> int:
        return id(websocket)

    def _send_lock_for_websocket(self, websocket: WebSocket) -> asyncio.Lock:
        lock = self._websocket_send_locks.get(id(websocket))
        if lock is None:
            lock = asyncio.Lock()
            self._websocket_send_locks[id(websocket)] = lock
        return lock

    def _event_sink(self, websocket: WebSocket) -> WebSocketEventSink:
        return self.event_sinks.sink(
            websocket=websocket,
            send_lock=lambda: self._send_lock_for_websocket(websocket),
        )

    async def _send_error(
        self, websocket: WebSocket, session_id: UUID | None, code: ErrorCode, message: str
    ) -> None:
        await self._event_sink(websocket).emit(
            SessionErrorEvent(session_id=session_id, code=code, message=message)
        )

    def status(self) -> StatusMessage:
        runtime_truth = self.orchestrator.runtime_truth(
            overlay_events_enabled=self.event_sinks.overlay_events_enabled,
        )
        return runtime_truth.to_status(
            self.orchestrator.runtime_status_state(),
            self.orchestrator.runtime_status_metrics(
                overlay_events_emitted=self.event_sinks.overlay_events_emitted,
                overlay_events_dropped=self.event_sinks.overlay_events_dropped,
            ),
        )


def create_app(settings: ServerSettings) -> FastAPI:
    server = DaemonServer(settings)

    @asynccontextmanager
    async def lifespan(app: FastAPI):
        del app
        orchestrator = server.orchestrator
        audio_started = False
        try:
            logger.info("Starting audio capture")
            orchestrator.audio.start()
            audio_started = True
            logger.info("Warming Parakeet model on {}", settings.device)
            await asyncio.to_thread(orchestrator.transcriber.warmup)
            logger.info("Model warmup completed")
            _release_cuda_cache(settings.device)
            if orchestrator._vad_enabled:
                await asyncio.to_thread(orchestrator.prepare_vad)
            runtime_truth = orchestrator.runtime_truth(
                overlay_events_enabled=server.event_sinks.overlay_events_enabled
            )
            runtime_degraded = runtime_truth.degraded
            _log = logger.warning if runtime_degraded else logger.info
            _log(
                "Runtime truth: {}",
                format_log_record(runtime_truth.to_log_record()),
            )
            yield
        finally:
            if audio_started:
                logger.info("Stopping audio capture")
                orchestrator.audio.stop()

    app = FastAPI(title="Parakeet STT Daemon", version="0.2.0", lifespan=lifespan)

    @app.get(DAEMON_HEALTH_PATH)
    async def health() -> dict[str, str]:
        return {"status": "ok"}

    if settings.status_enabled:

        @app.get(DAEMON_STATUS_PATH)
        async def status() -> JSONResponse:
            return JSONResponse(server.status().model_dump())

    @app.websocket(DAEMON_WS_PATH)
    async def websocket_endpoint(websocket: WebSocket) -> None:
        await server.handle_websocket(websocket)

    return app


__all__ = ["DaemonServer", "create_app"]
