"""CLI entrypoint for running the Parakeet STT daemon server."""
from __future__ import annotations

import argparse
from typing import Sequence

import uvicorn
from loguru import logger

from .config import ServerSettings
from .server import create_app


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
    return parser.parse_args(argv)


def _build_settings(args: argparse.Namespace) -> ServerSettings:
    kwargs: dict[str, object] = {}
    for field in ("host", "port", "language", "device", "mic_device", "shared_secret"):
        value = getattr(args, field, None)
        if value is not None:
            kwargs[field] = value
    if hasattr(args, "status_enabled"):
        kwargs["status_enabled"] = bool(args.status_enabled)
    return ServerSettings(**kwargs)


def main(argv: Sequence[str] | None = None) -> None:
    args = _parse_args(argv)
    settings = _build_settings(args)
    logger.info(
        "Starting parakeet-stt-daemon on {}:{} (device: {}, mic: {})",
        settings.host,
        settings.port,
        settings.device,
        settings.mic_device,
    )
    app = create_app(settings)
    uvicorn.run(app, host=settings.host, port=settings.port, log_level="info")


if __name__ == "__main__":
    main()
