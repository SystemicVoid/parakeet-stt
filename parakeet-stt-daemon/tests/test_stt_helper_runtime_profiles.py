"""Helper runtime profile resolution for daemon launch settings."""

from __future__ import annotations

import json
import os
import shlex
import subprocess
from pathlib import Path

import pytest

from parakeet_stt_daemon.config import (
    DEFAULT_DAEMON_HOST,
    DEFAULT_DAEMON_PORT,
    daemon_status_url,
    daemon_websocket_endpoint,
)

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


def _run_get_stt_start_args(
    *args: str,
    extra_env: dict[str, str] | None = None,
) -> list[str]:
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
    command = [
        "bash",
        "-lc",
        (
            f"source {shlex.quote(str(HELPER_PATH))} && "
            f"get_stt_start_args start_args {command_line} && "
            "printf '%s\\0' \"${start_args[@]}\""
        ),
    ]
    completed = subprocess.run(
        command,
        check=True,
        cwd=REPO_ROOT,
        env=env,
        capture_output=True,
    )
    return [item.decode() for item in completed.stdout.split(b"\0") if item]


def _run_get_stt_start_args_with_malformed_export() -> subprocess.CompletedProcess[str]:
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("PARAKEET_") and not key.startswith("_STT_")
    }
    env["PARAKEET_ROOT"] = str(REPO_ROOT)
    env["_STT_SKIP_LOCAL_OVERRIDES"] = "1"

    command = [
        "bash",
        "-lc",
        (
            f"source {shlex.quote(str(HELPER_PATH))} && "
            'stt() { printf "%s\\n" "unterminated\'"; } && '
            "get_stt_start_args start_args"
        ),
    ]
    return subprocess.run(
        command,
        check=False,
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
    )


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


def _run_status_runtime_truth(payload_path: Path) -> dict[str, str]:
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("PARAKEET_") and not key.startswith("_STT_")
    }
    env["PARAKEET_ROOT"] = str(REPO_ROOT)
    env["_STT_SKIP_LOCAL_OVERRIDES"] = "1"
    command = [
        "bash",
        "-lc",
        f"source {shlex.quote(str(HELPER_PATH))} && "
        f"stt __daemon-status-runtime-truth {shlex.quote(payload_path.as_uri())}",
    ]
    completed = subprocess.run(
        command,
        check=True,
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
    )
    truth: dict[str, str] = {}
    for line in completed.stdout.splitlines():
        key, value = line.split("=", 1)
        truth[key] = value
    return truth


def _status_payload(**overrides: object) -> dict[str, object]:
    payload: dict[str, object] = {
        "device": "cuda",
        "effective_device": "cpu",
        "streaming_enabled": True,
        "stream_helper_active": False,
        "stream_helper_scope": "live_session_only",
        "stream_fallback_reason": None,
        "stream_path_executed": False,
        "stream_chunks_processed": 0,
        "chunk_secs": 2.4,
        "finalization_mode": "offline_seal",
        "final_audio_source": "canonical_session_audio",
        "tail_trim_mode": "rms",
        "vad_enabled": True,
        "vad_active": False,
        "vad_fallback_reason": "load_failed:missing_dependency:onnxruntime",
        "overlay_events_enabled": True,
    }
    payload.update(overrides)
    return payload


def _run_client_ready_once(
    log_path: Path,
    pid_file: Path,
    marker: str,
    pid: int,
) -> dict[str, str]:
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("PARAKEET_") and not key.startswith("_STT_")
    }
    env["PARAKEET_ROOT"] = str(REPO_ROOT)
    env["_STT_SKIP_LOCAL_OVERRIDES"] = "1"
    command = [
        "bash",
        "-lc",
        f"source {shlex.quote(str(HELPER_PATH))} && "
        "stt __client-ready-once "
        f"{shlex.quote(str(log_path))} "
        f"{shlex.quote(str(pid_file))} "
        f"{shlex.quote(marker)} "
        f"{pid}",
    ]
    completed = subprocess.run(
        command,
        check=True,
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
    )

    result: dict[str, str] = {}
    for line in completed.stdout.splitlines():
        key, value = line.split("=", 1)
        result[key] = value
    return result


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


def test_helper_endpoint_defaults_match_daemon_settings() -> None:
    config = _run_runtime_config()

    assert config["daemon_host"] == DEFAULT_DAEMON_HOST
    assert config["daemon_port"] == str(DEFAULT_DAEMON_PORT)
    assert config["daemon_websocket_endpoint"] == daemon_websocket_endpoint()
    assert config["daemon_status_url"] == daemon_status_url()
    assert config["daemon_health_url"] == "http://127.0.0.1:8765/healthz"
    assert config["llm_base_url"] == "http://127.0.0.1:8080/v1"
    assert config["managed_llm_api_base_url"] == "http://127.0.0.1:8080/v1"
    assert config["llm_health_url"] == "http://127.0.0.1:8080/health"


def test_helper_endpoint_env_overrides_resolve_consistently() -> None:
    config = _run_runtime_config(
        extra_env={
            "PARAKEET_HOST": "0.0.0.0",
            "PARAKEET_PORT": "9001",
            "PARAKEET_LLM_SERVER_HOST": "llm.local",
            "PARAKEET_LLM_SERVER_PORT": "8181",
        }
    )

    assert config["daemon_host"] == "0.0.0.0"
    assert config["daemon_port"] == "9001"
    assert config["daemon_websocket_endpoint"] == daemon_websocket_endpoint("0.0.0.0", 9001)
    assert config["daemon_status_url"] == daemon_status_url("0.0.0.0", 9001)
    assert config["daemon_health_url"] == "http://0.0.0.0:9001/healthz"
    assert config["llm_base_url"] == "http://llm.local:8181/v1"
    assert config["managed_llm_api_base_url"] == "http://llm.local:8181/v1"
    assert config["llm_health_url"] == "http://llm.local:8181/health"


def test_helper_start_cli_llm_base_url_overrides_env() -> None:
    config = _run_runtime_config(
        "--llm-base-url",
        "http://cli.local:8182/custom",
        extra_env={"PARAKEET_LLM_BASE_URL": "http://env.local:8181/v1"},
    )

    assert config["llm_base_url"] == "http://cli.local:8182/custom"
    assert config["managed_llm_api_base_url"] == "http://127.0.0.1:8080/v1"


def test_get_stt_start_args_preserves_multiline_text_with_scalar_option() -> None:
    prompt = "\n".join(
        [
            "Return only final text.",
            "--unknown-option should remain prompt text.",
            "Keep shell-sensitive content: $HOME \"quoted\" 'single'.",
        ]
    )

    args = _run_get_stt_start_args(
        "streaming",
        "--llm-system-prompt",
        prompt,
        "--uinput-dwell-ms",
        "42",
    )

    prompt_index = args.index("--llm-system-prompt")
    dwell_index = args.index("--uinput-dwell-ms")
    assert args[prompt_index + 1] == prompt
    assert args[dwell_index + 1] == "42"
    assert "--unknown-option should remain prompt text." not in args


def test_get_stt_start_args_fails_on_malformed_shell_export() -> None:
    completed = _run_get_stt_start_args_with_malformed_export()

    assert completed.returncode != 0
    assert "get_stt_start_args: failed to parse __start-cli-args-shell output" in completed.stderr


def test_runtime_match_accepts_cpu_fallback_for_accelerator_request() -> None:
    assert _run_runtime_match("cuda", "false", "false", "cpu", "false", "false") is True


def test_runtime_match_rejects_accelerator_when_cpu_requested() -> None:
    assert _run_runtime_match("cpu", "false", "false", "cuda", "false", "false") is False


def test_client_ready_once_rejects_pid_without_current_readiness_logs(tmp_path: Path) -> None:
    marker = "current-client-start"
    log_path = tmp_path / "client.log"
    pid_file = tmp_path / "client.pid"
    log_path.write_text(f"{marker}\n", encoding="utf-8")

    result = _run_client_ready_once(log_path, pid_file, marker, os.getpid())

    assert result == {"ready": "false"}
    assert not pid_file.exists()


def test_client_ready_once_requires_current_hotkey_and_connection_logs(tmp_path: Path) -> None:
    marker = "current-client-start"
    log_path = tmp_path / "client.log"
    pid_file = tmp_path / "client.pid"
    log_path.write_text(
        "\n".join(
            [
                "Hotkey listeners started",
                "Connected to daemon",
                marker,
                "Hotkey listeners started",
            ]
        ),
        encoding="utf-8",
    )

    stale_result = _run_client_ready_once(log_path, pid_file, marker, os.getpid())
    assert stale_result == {"ready": "false"}

    log_path.write_text(
        "\n".join(
            [
                marker,
                "Hotkey listeners started",
                "Connected to daemon",
            ]
        ),
        encoding="utf-8",
    )

    ready_result = _run_client_ready_once(log_path, pid_file, marker, os.getpid())

    assert ready_result == {"ready": "true"}
    assert pid_file.read_text(encoding="utf-8").strip() == str(os.getpid())


def test_daemon_status_runtime_truth_reads_status_fields_unchanged(tmp_path: Path) -> None:
    payload_path = tmp_path / "status.json"
    payload_path.write_text(
        json.dumps(
            {
                "device": "cuda",
                "effective_device": "cpu",
                "streaming_enabled": True,
                "stream_helper_active": False,
                "stream_helper_scope": "live_session_only",
                "stream_fallback_reason": None,
                "stream_path_executed": False,
                "stream_chunks_processed": 0,
                "chunk_secs": 2.4,
                "finalization_mode": "offline_seal",
                "final_audio_source": "canonical_session_audio",
                "tail_trim_mode": "rms",
                "vad_enabled": True,
                "vad_active": False,
                "vad_fallback_reason": "load_failed:missing_dependency:onnxruntime",
                "overlay_events_enabled": True,
            }
        ),
        encoding="utf-8",
    )

    truth = _run_status_runtime_truth(payload_path)

    assert truth == {
        "device": "cuda",
        "effective_device": "cpu",
        "streaming_enabled": "true",
        "stream_helper_active": "false",
        "stream_helper_scope": "live_session_only",
        "stream_fallback_reason": "",
        "stream_path_executed": "false",
        "stream_chunks_processed": "0",
        "chunk_secs": "2.4",
        "finalization_mode": "offline_seal",
        "final_audio_source": "canonical_session_audio",
        "tail_trim_mode": "rms",
        "vad_enabled": "true",
        "vad_active": "false",
        "vad_fallback_reason": "load_failed:missing_dependency:onnxruntime",
        "overlay_events_enabled": "true",
    }


def test_daemon_status_runtime_truth_normalizes_numeric_string(tmp_path: Path) -> None:
    payload_path = tmp_path / "status.json"
    payload_path.write_text(
        json.dumps(
            {
                "device": "cuda",
                "effective_device": "cpu",
                "streaming_enabled": True,
                "stream_helper_active": False,
                "stream_helper_scope": "live_session_only",
                "stream_fallback_reason": None,
                "stream_path_executed": False,
                "stream_chunks_processed": 0,
                "chunk_secs": "2.4000",
                "finalization_mode": "offline_seal",
                "final_audio_source": "canonical_session_audio",
                "tail_trim_mode": "rms",
                "vad_enabled": True,
                "vad_active": False,
                "vad_fallback_reason": "load_failed:missing_dependency:onnxruntime",
                "overlay_events_enabled": True,
            }
        ),
        encoding="utf-8",
    )

    truth = _run_status_runtime_truth(payload_path)

    assert truth["chunk_secs"] == "2.4"


def test_daemon_status_runtime_truth_accepts_stream_chunk_count_string(
    tmp_path: Path,
) -> None:
    payload_path = tmp_path / "status.json"
    payload_path.write_text(
        json.dumps(_status_payload(stream_chunks_processed="2")),
        encoding="utf-8",
    )

    truth = _run_status_runtime_truth(payload_path)

    assert truth["stream_chunks_processed"] == "2"


@pytest.mark.parametrize("value", [-1, -1.0, 1.5, "1.5", "-1", True, "nan"])
def test_daemon_status_runtime_truth_rejects_invalid_stream_chunk_count(
    tmp_path: Path,
    value: object,
) -> None:
    payload_path = tmp_path / "status.json"
    payload_path.write_text(
        json.dumps(_status_payload(stream_chunks_processed=value)),
        encoding="utf-8",
    )

    with pytest.raises(subprocess.CalledProcessError):
        _run_status_runtime_truth(payload_path)


def test_daemon_status_runtime_truth_rejects_non_finite_numeric_string(tmp_path: Path) -> None:
    payload_path = tmp_path / "status.json"
    payload_path.write_text(
        json.dumps(
            {
                "device": "cuda",
                "effective_device": "cpu",
                "streaming_enabled": True,
                "stream_helper_active": False,
                "stream_helper_scope": "live_session_only",
                "stream_fallback_reason": None,
                "stream_path_executed": False,
                "stream_chunks_processed": 0,
                "chunk_secs": "inf",
                "finalization_mode": "offline_seal",
                "final_audio_source": "canonical_session_audio",
                "tail_trim_mode": "rms",
                "vad_enabled": True,
                "vad_active": False,
                "vad_fallback_reason": "load_failed:missing_dependency:onnxruntime",
                "overlay_events_enabled": True,
            }
        ),
        encoding="utf-8",
    )

    with pytest.raises(subprocess.CalledProcessError):
        _run_status_runtime_truth(payload_path)


def test_status_runtime_truth_rejects_non_finite_numeric_tokens(tmp_path: Path) -> None:
    for index, chunk_secs in enumerate((float("inf"), float("-inf"), float("nan"))):
        payload_path = tmp_path / f"status-{index}.json"
        payload_path.write_text(
            json.dumps(
                {
                    "device": "cuda",
                    "effective_device": "cpu",
                    "streaming_enabled": True,
                    "stream_helper_active": False,
                    "stream_helper_scope": "live_session_only",
                    "stream_fallback_reason": None,
                    "stream_path_executed": False,
                    "stream_chunks_processed": 0,
                    "chunk_secs": chunk_secs,
                    "finalization_mode": "offline_seal",
                    "final_audio_source": "canonical_session_audio",
                    "tail_trim_mode": "rms",
                    "vad_enabled": True,
                    "vad_active": False,
                    "vad_fallback_reason": "load_failed:missing_dependency:onnxruntime",
                    "overlay_events_enabled": True,
                }
            ),
            encoding="utf-8",
        )

        with pytest.raises(subprocess.CalledProcessError):
            _run_status_runtime_truth(payload_path)
