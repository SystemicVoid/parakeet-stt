"""Drift checks for the shared runtime endpoint/path interface."""

from __future__ import annotations

import json
import re
from pathlib import Path
from typing import Any, cast

from parakeet_stt_daemon import runtime_interface

REPO_ROOT = Path(__file__).resolve().parents[2]
CONTRACT_PATH = REPO_ROOT / "docs" / "protocol" / "runtime-interface.json"
SHELL_PROJECTION_PATH = REPO_ROOT / "scripts" / "stt-runtime-interface.sh"
RUST_PROJECTION_PATH = REPO_ROOT / "parakeet-ptt" / "src" / "runtime_interface.rs"
TROUBLESHOOTING_DOC_PATH = REPO_ROOT / "docs" / "stt-troubleshooting.md"
PROTOCOL_DOC_PATH = REPO_ROOT / "docs" / "protocol" / "README.md"
JsonObject = dict[str, Any]


def _contract() -> JsonObject:
    return json.loads(CONTRACT_PATH.read_text(encoding="utf-8"))


def _as_object(value: object) -> JsonObject:
    assert isinstance(value, dict)
    return cast(JsonObject, value)


def _as_str(value: object) -> str:
    assert isinstance(value, str)
    return value


def _as_port(value: object) -> str:
    assert isinstance(value, int)
    return str(value)


def _shell_projection() -> dict[str, str]:
    values: dict[str, str] = {}
    for line in SHELL_PROJECTION_PATH.read_text(encoding="utf-8").splitlines():
        match = re.fullmatch(r'(STT_[A-Z0-9_]+)="([^"]*)"', line)
        if match:
            values[match.group(1)] = match.group(2)
    return values


def _rust_projection() -> dict[str, str]:
    values: dict[str, str] = {}
    text = RUST_PROJECTION_PATH.read_text(encoding="utf-8")
    for name, value in re.findall(r'pub\(crate\) const ([A-Z0-9_]+): &str = "([^"]*)";', text):
        values[name] = value
    for name, value in re.findall(r"pub\(crate\) const ([A-Z0-9_]+): u16 = ([0-9]+);", text):
        values[name] = value
    return values


def test_runtime_interface_projections_match_documented_contract() -> None:
    contract = _contract()
    daemon = _as_object(contract["daemon"])
    managed_llm = _as_object(contract["managed_llm"])
    daemon_paths = _as_object(daemon["paths"])
    managed_llm_paths = _as_object(managed_llm["paths"])

    expected = {
        "DEFAULT_DAEMON_HOST": _as_str(daemon["default_host"]),
        "DEFAULT_DAEMON_PORT": _as_port(daemon["default_port"]),
        "DAEMON_WS_PATH": _as_str(daemon_paths["websocket"]),
        "DAEMON_STATUS_PATH": _as_str(daemon_paths["status"]),
        "DAEMON_HEALTH_PATH": _as_str(daemon_paths["health"]),
        "DEFAULT_LLM_HOST": _as_str(managed_llm["default_host"]),
        "DEFAULT_LLM_PORT": _as_port(managed_llm["default_port"]),
        "LLM_API_PATH": _as_str(managed_llm_paths["api"]),
        "LLM_HEALTH_PATH": _as_str(managed_llm_paths["health"]),
    }

    assert runtime_interface.DEFAULT_DAEMON_HOST == expected["DEFAULT_DAEMON_HOST"]
    assert str(runtime_interface.DEFAULT_DAEMON_PORT) == expected["DEFAULT_DAEMON_PORT"]
    assert runtime_interface.DAEMON_WS_PATH == expected["DAEMON_WS_PATH"]
    assert runtime_interface.DAEMON_STATUS_PATH == expected["DAEMON_STATUS_PATH"]
    assert runtime_interface.DAEMON_HEALTH_PATH == expected["DAEMON_HEALTH_PATH"]
    assert runtime_interface.DEFAULT_LLM_SERVER_HOST == expected["DEFAULT_LLM_HOST"]
    assert str(runtime_interface.DEFAULT_LLM_SERVER_PORT) == expected["DEFAULT_LLM_PORT"]
    assert runtime_interface.LLM_API_PATH == expected["LLM_API_PATH"]
    assert runtime_interface.LLM_HEALTH_PATH == expected["LLM_HEALTH_PATH"]

    shell = _shell_projection()
    assert shell["STT_DEFAULT_DAEMON_HOST"] == expected["DEFAULT_DAEMON_HOST"]
    assert shell["STT_DEFAULT_DAEMON_PORT"] == expected["DEFAULT_DAEMON_PORT"]
    assert shell["STT_DAEMON_WS_PATH"] == expected["DAEMON_WS_PATH"]
    assert shell["STT_DAEMON_STATUS_PATH"] == expected["DAEMON_STATUS_PATH"]
    assert shell["STT_DAEMON_HEALTH_PATH"] == expected["DAEMON_HEALTH_PATH"]
    assert shell["STT_DEFAULT_LLM_SERVER_HOST"] == expected["DEFAULT_LLM_HOST"]
    assert shell["STT_DEFAULT_LLM_SERVER_PORT"] == expected["DEFAULT_LLM_PORT"]
    assert shell["STT_LLM_API_PATH"] == expected["LLM_API_PATH"]
    assert shell["STT_LLM_HEALTH_PATH"] == expected["LLM_HEALTH_PATH"]

    rust = _rust_projection()
    assert rust["DEFAULT_DAEMON_HOST"] == expected["DEFAULT_DAEMON_HOST"]
    assert rust["DEFAULT_DAEMON_PORT"] == expected["DEFAULT_DAEMON_PORT"]
    assert rust["DAEMON_WS_PATH"] == expected["DAEMON_WS_PATH"]
    assert rust["DAEMON_STATUS_PATH"] == expected["DAEMON_STATUS_PATH"]
    assert rust["DAEMON_HEALTH_PATH"] == expected["DAEMON_HEALTH_PATH"]
    assert rust["DEFAULT_LLM_HOST"] == expected["DEFAULT_LLM_HOST"]
    assert rust["DEFAULT_LLM_PORT"] == expected["DEFAULT_LLM_PORT"]
    assert rust["LLM_API_PATH"] == expected["LLM_API_PATH"]
    assert rust["LLM_HEALTH_PATH"] == expected["LLM_HEALTH_PATH"]


def test_runtime_interface_urls_are_built_from_named_parts() -> None:
    assert runtime_interface.daemon_websocket_endpoint() == "ws://127.0.0.1:8765/ws"
    assert runtime_interface.daemon_status_url() == "http://127.0.0.1:8765/status"
    assert runtime_interface.daemon_health_url() == "http://127.0.0.1:8765/healthz"
    assert runtime_interface.managed_llm_api_base_url() == "http://127.0.0.1:8080/v1"
    assert runtime_interface.managed_llm_health_url() == "http://127.0.0.1:8080/health"


def test_operator_docs_reference_runtime_interface_defaults() -> None:
    default_authority = (
        f"{runtime_interface.DEFAULT_DAEMON_HOST}:{runtime_interface.DEFAULT_DAEMON_PORT}"
    )
    troubleshooting = TROUBLESHOOTING_DOC_PATH.read_text(encoding="utf-8")
    protocol_readme = PROTOCOL_DOC_PATH.read_text(encoding="utf-8")

    assert "docs/protocol/runtime-interface.json" in troubleshooting
    assert "docs/protocol/runtime-interface.json" in protocol_readme
    assert default_authority in troubleshooting
