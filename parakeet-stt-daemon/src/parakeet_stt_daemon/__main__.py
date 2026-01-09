"""CLI entrypoint for running the Parakeet STT daemon server."""

from __future__ import annotations

import argparse
from collections.abc import Sequence
from typing import Any, Literal, TypedDict, cast

import sounddevice as sd
import uvicorn
from loguru import logger

from .config import ServerSettings
from .server import DaemonServer, create_app


class SettingsKwargs(TypedDict, total=False):
    host: str
    port: int
    language: str | None
    device: Literal["cuda", "cpu"]
    mic_device: int | str | None
    shared_secret: str | None
    status_enabled: bool
    streaming_enabled: bool
    chunk_secs: float
    right_context_secs: float
    left_context_secs: float
    batch_size: int


def _parse_args(argv: Sequence[str] | None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Parakeet STT daemon")
    parser.add_argument("--host", help="Bind host", default=None)
    parser.add_argument("--port", type=int, help="Bind port", default=None)
    parser.add_argument("--language", help="Language hint", default=None)
    parser.add_argument("--device", choices=["cuda", "cpu"], default=None)
    parser.add_argument("--mic-device", dest="mic_device", default=None)
    parser.add_argument(
        "--shared-secret",
        dest="shared_secret",
        help="Optional shared secret required from clients",
        default=None,
    )
    parser.add_argument(
        "--no-status",
        dest="status_enabled",
        action="store_false",
        help="Disable the /status endpoint",
    )
    parser.add_argument(
        "--no-streaming",
        dest="streaming_enabled",
        action="store_false",
        help="Disable streaming path and fall back to offline transcription",
    )
    parser.add_argument("--chunk-secs", type=float, help="Chunk size (seconds) for streaming")
    parser.add_argument(
        "--right-context-secs", type=float, help="Right context (seconds) for streaming"
    )
    parser.add_argument(
        "--left-context-secs", type=float, help="Left context (seconds) for streaming"
    )
    parser.add_argument(
        "--batch-size", type=int, help="Batch size for streaming chunked inference helper"
    )
    parser.add_argument(
        "--check",
        action="store_true",
        help="Run startup checks (audio/model/streaming config) then exit",
    )
    return parser.parse_args(argv)


def _build_settings(args: argparse.Namespace) -> ServerSettings:
    kwargs: SettingsKwargs = {}
    if args.host is not None:
        kwargs["host"] = str(args.host)
    if args.port is not None:
        kwargs["port"] = int(args.port)
    if args.language is not None:
        kwargs["language"] = str(args.language)
    if args.device is not None:
        kwargs["device"] = cast(Literal["cuda", "cpu"], args.device)
    if args.mic_device is not None:
        kwargs["mic_device"] = args.mic_device
    if args.shared_secret is not None:
        kwargs["shared_secret"] = str(args.shared_secret)
    kwargs["status_enabled"] = bool(args.status_enabled)
    kwargs["streaming_enabled"] = bool(args.streaming_enabled)
    if args.chunk_secs is not None:
        kwargs["chunk_secs"] = float(args.chunk_secs)
    if args.right_context_secs is not None:
        kwargs["right_context_secs"] = float(args.right_context_secs)
    if args.left_context_secs is not None:
        kwargs["left_context_secs"] = float(args.left_context_secs)
    if args.batch_size is not None:
        kwargs["batch_size"] = int(args.batch_size)
    return ServerSettings(**kwargs)


def main(argv: Sequence[str] | None = None) -> None:
    args = _parse_args(argv)
    settings = _build_settings(args)
    logger.info(
        "Starting parakeet-stt-daemon on {}:{} (device: {}, mic: {}, streaming: {})",
        settings.host,
        settings.port,
        settings.device,
        settings.mic_device,
        settings.streaming_enabled,
    )
    if settings.streaming_enabled:
        logger.info(
            (
                "Streaming config: chunk_secs={}, right_context_secs={}, "
                "left_context_secs={}, batch_size={}"
            ),
            settings.chunk_secs,
            settings.right_context_secs,
            settings.left_context_secs,
            settings.batch_size,
        )
    if args.check:
        run_checks(settings)
        return

    app = create_app(settings)
    uvicorn.run(app, host=settings.host, port=settings.port, log_level="info")


def run_checks(settings: ServerSettings) -> None:
    logger.info("Running startup checks with settings: {}", settings)
    try:
        server = DaemonServer(settings)
    except Exception as exc:  # noqa: BLE001
        logger.error("Failed to initialise server components: {}", exc)
        raise

    try:
        server.audio.start()
        logger.info("Audio stream opened successfully (device={})", settings.mic_device)
        server.audio.stop()
    except Exception as exc:  # noqa: BLE001
        logger.error("Audio stream check failed: {}", exc)

    try:
        server.transcriber.warmup()
        logger.info("Model warmup completed")
    except Exception as exc:  # noqa: BLE001
        logger.warning("Model warmup skipped/failed: {}", exc)

    try:
        # sounddevice returns DeviceList which pyright doesn't recognize as dict-like
        devices = cast(Sequence[dict[str, Any]], sd.query_devices())
        inputs = [
            (idx, dev["name"])
            for idx, dev in enumerate(devices)
            if dev.get("max_input_channels", 0) > 0
        ]
        if inputs:
            pretty = [f"{idx}: {name}" for idx, name in inputs]
            logger.info("Input devices (index: name): {}", pretty)
        else:
            logger.warning("No audio input devices detected")
    except Exception as exc:  # noqa: BLE001
        logger.warning("Failed to list audio devices: {}", exc)

    logger.info(
        "Streaming enabled: {}, chunk_secs={}, right_context_secs={}, left_context_secs={}, batch_size={}",  # noqa: E501
        settings.streaming_enabled,
        settings.chunk_secs,
        settings.right_context_secs,
        settings.left_context_secs,
        settings.batch_size,
    )


if __name__ == "__main__":
    main()
