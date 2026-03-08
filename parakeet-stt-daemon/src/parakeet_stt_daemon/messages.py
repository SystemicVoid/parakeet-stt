"""Typed message definitions for the WebSocket protocol.

These models intentionally mirror the SPEC.md contract and provide a
single place to evolve the message schema.
"""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
from enum import Enum
from typing import Literal
from uuid import UUID

from pydantic import BaseModel, ConfigDict, Field
from .session import SessionState

Timestamp = datetime


class ClientMessageType(str, Enum):
    START_SESSION = "start_session"
    STOP_SESSION = "stop_session"
    ABORT_SESSION = "abort_session"


class ServerMessageType(str, Enum):
    SESSION_STARTED = "session_started"
    FINAL_RESULT = "final_result"
    ERROR = "error"
    STATUS = "status"
    INTERIM_STATE = "interim_state"
    INTERIM_TEXT = "interim_text"
    SESSION_ENDED = "session_ended"
    AUDIO_LEVEL = "audio_level"


class InterimStateValue(str, Enum):
    LISTENING = "listening"
    PROCESSING = "processing"
    INTERIM = "interim"
    FINALIZING = "finalizing"


class StartSession(BaseModel):
    model_config = ConfigDict(extra="forbid")

    type: Literal[ClientMessageType.START_SESSION]
    session_id: UUID
    mode: Literal["push_to_talk"] = "push_to_talk"
    preferred_lang: str | None = Field(default="auto")
    timestamp: Timestamp


class StopSession(BaseModel):
    model_config = ConfigDict(extra="forbid")

    type: Literal[ClientMessageType.STOP_SESSION]
    session_id: UUID
    timestamp: Timestamp


class AbortSession(BaseModel):
    model_config = ConfigDict(extra="forbid")

    type: Literal[ClientMessageType.ABORT_SESSION]
    session_id: UUID
    reason: Literal["timeout", "user", "error"]
    timestamp: Timestamp


ClientMessage = StartSession | StopSession | AbortSession


class SessionStarted(BaseModel):
    model_config = ConfigDict(extra="forbid")

    type: Literal[ServerMessageType.SESSION_STARTED] = Field(
        default=ServerMessageType.SESSION_STARTED
    )
    session_id: UUID
    ts: Timestamp
    mic_device: str | None
    lang: str | None


class FinalResult(BaseModel):
    model_config = ConfigDict(extra="forbid")

    type: Literal[ServerMessageType.FINAL_RESULT] = Field(default=ServerMessageType.FINAL_RESULT)
    session_id: UUID
    text: str
    latency_ms: int
    audio_ms: int
    lang: str | None = Field(default="auto")
    confidence: float | None = None


class ErrorMessage(BaseModel):
    model_config = ConfigDict(extra="forbid")

    type: Literal[ServerMessageType.ERROR] = Field(default=ServerMessageType.ERROR)
    session_id: UUID | None = None
    code: Literal[
        "SESSION_BUSY",
        "SESSION_NOT_FOUND",
        "SESSION_ABORTED",
        "AUDIO_DEVICE",
        "MODEL",
        "INVALID_REQUEST",
        "UNEXPECTED",
    ]
    message: str


class StatusMessage(BaseModel):
    model_config = ConfigDict(extra="forbid")

    type: Literal[ServerMessageType.STATUS] = Field(default=ServerMessageType.STATUS)
    state: SessionState
    sessions_active: int
    gpu_mem_mb: int | None = None
    device: str | None = None
    effective_device: str | None = None
    streaming_enabled: bool | None = None
    stream_helper_active: bool | None = None
    stream_helper_scope: Literal["live_session_only"] | None = None
    stream_fallback_reason: str | None = None
    finalization_mode: Literal["offline_seal"] | None = None
    final_audio_source: Literal["canonical_session_audio"] | None = None
    tail_trim_mode: Literal["rms", "vad"] | None = None
    vad_enabled: bool | None = None
    vad_active: bool | None = None
    vad_fallback_reason: str | None = None
    overlay_events_enabled: bool | None = None
    overlay_events_emitted: int | None = None
    overlay_events_dropped: int | None = None
    chunk_secs: float | None = None
    active_session_age_ms: int | None = None
    audio_stop_ms: int | None = None
    finalize_ms: int | None = None
    infer_ms: int | None = None
    send_ms: int | None = None
    last_audio_ms: int | None = None
    last_infer_ms: int | None = None
    last_send_ms: int | None = None


class InterimStateMessage(BaseModel):
    model_config = ConfigDict(extra="forbid")

    type: Literal[ServerMessageType.INTERIM_STATE] = Field(default=ServerMessageType.INTERIM_STATE)
    session_id: UUID
    seq: int = Field(ge=0)
    state: InterimStateValue


class InterimTextMessage(BaseModel):
    model_config = ConfigDict(extra="forbid")

    type: Literal[ServerMessageType.INTERIM_TEXT] = Field(default=ServerMessageType.INTERIM_TEXT)
    session_id: UUID
    seq: int = Field(ge=0)
    text: str


class AudioLevelMessage(BaseModel):
    model_config = ConfigDict(extra="forbid")

    type: Literal[ServerMessageType.AUDIO_LEVEL] = Field(default=ServerMessageType.AUDIO_LEVEL)
    session_id: UUID
    level_db: float


class SessionEndedMessage(BaseModel):
    model_config = ConfigDict(extra="forbid")

    type: Literal[ServerMessageType.SESSION_ENDED] = Field(default=ServerMessageType.SESSION_ENDED)
    session_id: UUID
    reason: Literal["final", "abort", "error"] | None = None


ServerMessage = (
    SessionStarted
    | FinalResult
    | ErrorMessage
    | StatusMessage
    | InterimStateMessage
    | InterimTextMessage
    | AudioLevelMessage
    | SessionEndedMessage
)


@dataclass(frozen=True)
class ParsedMessage:
    kind: ClientMessageType
    model: ClientMessage


def parse_client_message(data: dict) -> ParsedMessage:
    """Parse raw JSON into a typed client message."""
    msg_type = data.get("type")
    if msg_type == ClientMessageType.START_SESSION:
        model = StartSession.model_validate(data)
    elif msg_type == ClientMessageType.STOP_SESSION:
        model = StopSession.model_validate(data)
    elif msg_type == ClientMessageType.ABORT_SESSION:
        model = AbortSession.model_validate(data)
    else:
        raise ValueError(f"Unsupported message type: {msg_type}")

    return ParsedMessage(kind=ClientMessageType(msg_type), model=model)


__all__ = [
    "ClientMessage",
    "ClientMessageType",
    "ServerMessage",
    "ServerMessageType",
    "InterimStateValue",
    "StartSession",
    "StopSession",
    "AbortSession",
    "SessionStarted",
    "FinalResult",
    "ErrorMessage",
    "StatusMessage",
    "InterimStateMessage",
    "InterimTextMessage",
    "AudioLevelMessage",
    "SessionEndedMessage",
    "ParsedMessage",
    "parse_client_message",
]
