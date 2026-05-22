"""Shared endpoint and path defaults for Parakeet runtime processes.

These constants are the Python projection of
``docs/protocol/runtime-interface.json``. Keep the projection synchronized with
that contract; tests fail when the documented interface and projections drift.
"""

from __future__ import annotations

DEFAULT_DAEMON_HOST = "127.0.0.1"
DEFAULT_DAEMON_PORT = 8765
DAEMON_WS_PATH = "/ws"
DAEMON_STATUS_PATH = "/status"
DAEMON_HEALTH_PATH = "/healthz"

DEFAULT_LLM_SERVER_HOST = "127.0.0.1"
DEFAULT_LLM_SERVER_PORT = 8080
LLM_API_PATH = "/v1"
LLM_HEALTH_PATH = "/health"


def endpoint_url(scheme: str, host: str, port: int | str, path: str) -> str:
    """Return a URL from named endpoint parts."""

    return f"{scheme}://{host}:{port}{path}"


def daemon_websocket_endpoint(
    host: str = DEFAULT_DAEMON_HOST, port: int = DEFAULT_DAEMON_PORT
) -> str:
    """Return the Client WebSocket endpoint for a Daemon host/port pair."""

    return endpoint_url("ws", host, port, DAEMON_WS_PATH)


def daemon_status_url(host: str = DEFAULT_DAEMON_HOST, port: int = DEFAULT_DAEMON_PORT) -> str:
    """Return the Daemon status endpoint for a host/port pair."""

    return endpoint_url("http", host, port, DAEMON_STATUS_PATH)


def daemon_health_url(host: str = DEFAULT_DAEMON_HOST, port: int = DEFAULT_DAEMON_PORT) -> str:
    """Return the Daemon health endpoint for a host/port pair."""

    return endpoint_url("http", host, port, DAEMON_HEALTH_PATH)


def managed_llm_api_base_url(
    host: str = DEFAULT_LLM_SERVER_HOST, port: int = DEFAULT_LLM_SERVER_PORT
) -> str:
    """Return the managed local LLM API base URL."""

    return endpoint_url("http", host, port, LLM_API_PATH)


def managed_llm_health_url(
    host: str = DEFAULT_LLM_SERVER_HOST, port: int = DEFAULT_LLM_SERVER_PORT
) -> str:
    """Return the managed local LLM health URL."""

    return endpoint_url("http", host, port, LLM_HEALTH_PATH)


__all__ = [
    "DAEMON_HEALTH_PATH",
    "DAEMON_STATUS_PATH",
    "DAEMON_WS_PATH",
    "DEFAULT_DAEMON_HOST",
    "DEFAULT_DAEMON_PORT",
    "DEFAULT_LLM_SERVER_HOST",
    "DEFAULT_LLM_SERVER_PORT",
    "LLM_API_PATH",
    "LLM_HEALTH_PATH",
    "daemon_health_url",
    "daemon_status_url",
    "daemon_websocket_endpoint",
    "endpoint_url",
    "managed_llm_api_base_url",
    "managed_llm_health_url",
]
