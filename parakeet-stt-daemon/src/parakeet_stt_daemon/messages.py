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
    code: Literal["SESSION_BUSY", "AUDIO_DEVICE", "MODEL", "UNEXPECTED"]
    message: str


class StatusMessage(BaseModel):
    model_config = ConfigDict(extra="forbid")

    type: Literal[ServerMessageType.STATUS] = Field(default=ServerMessageType.STATUS)
    state: Literal["idle", "listening", "processing"]
    sessions_active: int
    gpu_mem_mb: int | None = None
    device: str | None = None
    streaming_enabled: bool | None = None
    chunk_secs: float | None = None


ServerMessage = SessionStarted | FinalResult | ErrorMessage | StatusMessage


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
    "StartSession",
    "StopSession",
    "AbortSession",
    "SessionStarted",
    "FinalResult",
    "ErrorMessage",
    "StatusMessage",
    "ParsedMessage",
    "parse_client_message",
]
