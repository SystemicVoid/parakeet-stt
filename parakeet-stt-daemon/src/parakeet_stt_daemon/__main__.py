"""CLI entrypoint for running the Parakeet STT daemon server."""

from __future__ import annotations

import argparse
from collections.abc import Sequence
from typing import Literal, TypedDict, cast

import uvicorn
from loguru import logger

from .config import ServerSettings
from .server import create_app


class SettingsKwargs(TypedDict, total=False):
    host: str
    port: int
    language: str | None
    device: Literal["cuda", "cpu"]
    mic_device: int | str | None
    shared_secret: str | None
    status_enabled: bool


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
