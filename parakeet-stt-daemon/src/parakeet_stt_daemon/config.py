"""Configuration management for the Parakeet STT daemon."""

from __future__ import annotations

from pathlib import Path
from typing import Literal

from pydantic import Field
from pydantic_settings import BaseSettings, SettingsConfigDict

DEFAULT_DAEMON_HOST = "127.0.0.1"
DEFAULT_DAEMON_PORT = 8765
DAEMON_WS_PATH = "/ws"
DAEMON_STATUS_PATH = "/status"


def daemon_websocket_endpoint(
    host: str = DEFAULT_DAEMON_HOST, port: int = DEFAULT_DAEMON_PORT
) -> str:
    """Return the Client WebSocket endpoint for a Daemon host/port pair."""

    return f"ws://{host}:{port}{DAEMON_WS_PATH}"


def daemon_status_url(host: str = DEFAULT_DAEMON_HOST, port: int = DEFAULT_DAEMON_PORT) -> str:
    """Return the Daemon status endpoint for a host/port pair."""

    return f"http://{host}:{port}{DAEMON_STATUS_PATH}"


class ServerSettings(BaseSettings):
    """Runtime settings loaded from env vars or CLI overrides."""

    host: str = Field(
        default=DEFAULT_DAEMON_HOST, description="Host the WebSocket/HTTP server binds to"
    )
    port: int = Field(default=DEFAULT_DAEMON_PORT, ge=1, le=65535)
    shared_secret: str | None = Field(
        default=None,
        description="Optional shared secret required on the WebSocket connection.",
    )
    mic_device: int | str | None = Field(
        default=None,
        description="Microphone device (index or substring). None = system default input.",
    )
    language: str | None = Field(
        default="auto", description="Language hint passed to Parakeet where supported."
    )
    device: Literal["cuda", "cpu"] = Field(
        default="cuda", description="Inference device to target."
    )
    status_enabled: bool = Field(
        default=True, description="Expose /status HTTP endpoint when true."
    )
    config_path: Path | None = Field(
        default=None, description="Optional explicit config file path (reserved)."
    )
    streaming_enabled: bool = Field(
        default=False, description="Enable streaming inference path when true."
    )
    overlay_events_enabled: bool = Field(
        default=False,
        description="Emit overlay interim/session events over websocket when true.",
    )
    chunk_secs: float = Field(
        default=2.4, ge=0.1, le=10.0, description="Chunk size (seconds) for streaming inference."
    )
    right_context_secs: float = Field(
        default=1.6,
        ge=0.0,
        le=20.0,
        description="Right context (seconds) appended for streaming inference.",
    )
    left_context_secs: float = Field(
        default=10.0,
        ge=0.0,
        le=60.0,
        description="Left context (seconds) retained for streaming inference.",
    )
    batch_size: int = Field(
        default=32, ge=1, le=128, description="Batch size used by streaming inference helper."
    )
    max_session_seconds: float = Field(
        default=600.0,
        ge=1.0,
        le=1800.0,
        description=(
            "Hard session duration limit (seconds). Sessions reaching this limit stop "
            "accepting new audio and finalize on key release."
        ),
    )
    max_session_samples: int | None = Field(
        default=None,
        ge=1,
        le=28_800_000,
        description=(
            "Optional hard cap on in-memory buffered session samples. "
            "When unset, duration-based limit applies."
        ),
    )
    silence_floor_db: float = Field(
        default=-40.0,
        ge=-120.0,
        le=0.0,
        description="Stop trimming tail silence when RMS exceeds this floor (dB).",
    )
    vad_enabled: bool = Field(
        default=False,
        description="Enable Silero VAD-based tail trimming (opt-in, defaults off).",
    )

    model_config = SettingsConfigDict(
        env_prefix="PARAKEET_",
        env_file=".env",
        env_file_encoding="utf-8",
        extra="ignore",
    )


__all__ = [
    "DAEMON_STATUS_PATH",
    "DAEMON_WS_PATH",
    "DEFAULT_DAEMON_HOST",
    "DEFAULT_DAEMON_PORT",
    "ServerSettings",
    "daemon_status_url",
    "daemon_websocket_endpoint",
]
