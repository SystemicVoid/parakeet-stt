"""Configuration management for the Parakeet STT daemon."""

from __future__ import annotations

from pathlib import Path
from typing import Literal

from pydantic import Field
from pydantic_settings import BaseSettings, SettingsConfigDict


class ServerSettings(BaseSettings):
    """Runtime settings loaded from env vars or CLI overrides."""

    host: str = Field(default="127.0.0.1", description="Host the WebSocket/HTTP server binds to")
    port: int = Field(default=8765, ge=1, le=65535)
    shared_secret: str | None = Field(
        default=None,
        description="Optional shared secret required on the WebSocket connection.",
    )
    mic_device: int | str | None = Field(
        default=None, description="Preferred microphone device identifier."
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

    model_config = SettingsConfigDict(
        env_prefix="PARAKEET_",
        env_file=".env",
        env_file_encoding="utf-8",
        extra="ignore",
    )


__all__ = ["ServerSettings"]
