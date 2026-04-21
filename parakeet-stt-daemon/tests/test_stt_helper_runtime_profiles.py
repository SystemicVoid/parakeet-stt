"""Helper runtime profile resolution for daemon launch settings."""

from __future__ import annotations

import os
import shlex
import subprocess
from pathlib import Path

REPO_ROOT = Path(__file__).resolve().parents[2]
HELPER_PATH = REPO_ROOT / "scripts" / "stt-helper.sh"


def _run_runtime_config(*args: str, extra_env: dict[str, str] | None = None) -> dict[str, str]:
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("PARAKEET_") and not key.startswith("_STT_")
    }
    env["PARAKEET_ROOT"] = str(REPO_ROOT)
    env["_STT_SKIP_LOCAL_OVERRIDES"] = "1"
    if extra_env:
        env.update(extra_env)

    command_line = " ".join(shlex.quote(arg) for arg in args)
    if command_line:
        command_line = f" {command_line}"
    command = [
        "bash",
        "-lc",
        f"source {shlex.quote(str(HELPER_PATH))} && stt __start-runtime-config{command_line}",
    ]
    completed = subprocess.run(
        command,
        check=True,
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
    )

    config: dict[str, str] = {}
    for line in completed.stdout.splitlines():
        key, value = line.split("=", 1)
        config[key] = value
    return config


def _run_runtime_match(*args: str) -> bool:
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("PARAKEET_") and not key.startswith("_STT_")
    }
    env["PARAKEET_ROOT"] = str(REPO_ROOT)
    env["_STT_SKIP_LOCAL_OVERRIDES"] = "1"

    command_line = " ".join(shlex.quote(arg) for arg in args)
    command = [
        "bash",
        "-lc",
        f"source {shlex.quote(str(HELPER_PATH))} && stt __daemon-runtime-matches {command_line}",
    ]
    completed = subprocess.run(
        command,
        check=True,
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
    )
    return completed.stdout.strip() == "match=true"


def test_cpu_profile_resolves_offline_cpu_runtime() -> None:
    config = _run_runtime_config("cpu")

    assert config["launch_profile"] == "cpu"
    assert config["daemon_device"] == "cpu"
    assert config["daemon_streaming_enabled"] == "false"
    assert config["daemon_overlay_events_enabled"] == "false"


def test_cpu_profile_overrides_ambient_parakeet_device() -> None:
    config = _run_runtime_config("cpu", extra_env={"PARAKEET_DEVICE": "cuda"})

    assert config["daemon_device"] == "cpu"
    assert config["daemon_streaming_enabled"] == "false"
    assert config["daemon_overlay_events_enabled"] == "false"


def test_default_profile_resolves_streaming_cuda_runtime() -> None:
    config = _run_runtime_config()

    assert config["launch_profile"] == "stream-seal"
    assert config["daemon_device"] == "cuda"
    assert config["daemon_streaming_enabled"] == "true"
    assert config["daemon_overlay_events_enabled"] == "true"


def test_offline_profile_resolves_cuda_without_streaming() -> None:
    config = _run_runtime_config("offline")

    assert config["launch_profile"] == "offline"
    assert config["daemon_device"] == "cuda"
    assert config["daemon_streaming_enabled"] == "false"
    assert config["daemon_overlay_events_enabled"] == "false"


def test_runtime_match_accepts_cpu_fallback_for_accelerator_request() -> None:
    assert _run_runtime_match("cuda", "false", "false", "cpu", "false", "false") is True


def test_runtime_match_rejects_accelerator_when_cpu_requested() -> None:
    assert _run_runtime_match("cpu", "false", "false", "cuda", "false", "false") is False
