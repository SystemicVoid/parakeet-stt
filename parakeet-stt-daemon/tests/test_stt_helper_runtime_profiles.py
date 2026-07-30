"""Helper runtime profile resolution for daemon launch settings."""

from __future__ import annotations

import json
import os
import shlex
import subprocess
import textwrap
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path

import pytest

from parakeet_stt_daemon.config import (
    DEFAULT_DAEMON_HOST,
    DEFAULT_DAEMON_PORT,
    daemon_health_url,
    daemon_status_url,
    daemon_websocket_endpoint,
)
from parakeet_stt_daemon.messages import (
    STATUS_RUNTIME_TRUTH_FIELD_GROUPS,
    STATUS_RUNTIME_TRUTH_FIELDS,
)
from parakeet_stt_daemon.runtime_interface import (
    managed_llm_api_base_url,
    managed_llm_health_url,
)

REPO_ROOT = Path(__file__).resolve().parents[2]
HELPER_PATH = REPO_ROOT / "scripts" / "stt-helper.sh"
README_PATH = REPO_ROOT / "README.md"
TROUBLESHOOTING_DOC_PATH = REPO_ROOT / "docs" / "stt-troubleshooting.md"
FIXTURE_DIR = REPO_ROOT / "docs" / "protocol" / "fixtures"
START_REFERENCE_START_MARKER = "<!-- stt-helper:start-reference:start -->"
START_REFERENCE_END_MARKER = "<!-- stt-helper:start-reference:end -->"
START_PROFILE_ROW_FIELDS = (
    "profile_id",
    "mode_aliases",
    "command_aliases",
    "start_cli_arg",
    "daemon_streaming_enabled",
    "daemon_device_override",
    "overlay_enabled_default",
    "overlay_adaptive_width_default",
    "help_description",
)


@contextmanager
def _preserve_paths(*paths: Path) -> Iterator[None]:
    snapshots = {path: path.read_bytes() if path.exists() else None for path in paths}
    try:
        yield
    finally:
        for path, snapshot in snapshots.items():
            if snapshot is None:
                path.unlink(missing_ok=True)
            else:
                path.write_bytes(snapshot)


def _write_fake_command(bin_dir: Path, name: str, script: str) -> None:
    command_path = bin_dir / name
    command_path.write_text(script, encoding="utf-8")
    command_path.chmod(0o755)


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


def _run_start_profile_rows() -> list[dict[str, str]]:
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
        f"source {shlex.quote(str(HELPER_PATH))} && stt __start-profile-rows",
    ]
    completed = subprocess.run(
        command,
        check=True,
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
    )
    rows: list[dict[str, str]] = []
    for line in completed.stdout.splitlines():
        values = line.split("|")
        assert len(values) == len(START_PROFILE_ROW_FIELDS)
        rows.append(dict(zip(START_PROFILE_ROW_FIELDS, values, strict=True)))
    return rows


def _run_helper_help(topic: str) -> str:
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
        f"source {shlex.quote(str(HELPER_PATH))} && stt help {shlex.quote(topic)}",
    ]
    completed = subprocess.run(
        command,
        check=True,
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
    )
    return completed.stdout


def _run_helper_main_help() -> str:
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
        f"source {shlex.quote(str(HELPER_PATH))} && stt help",
    ]
    completed = subprocess.run(
        command,
        check=True,
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
    )
    return completed.stdout


def _run_stt_helper_command(
    *args: str,
    extra_env: dict[str, str] | None = None,
) -> subprocess.CompletedProcess[str]:
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
        f"source {shlex.quote(str(HELPER_PATH))} && stt {command_line}",
    ]
    return subprocess.run(
        command,
        check=False,
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
    )


def _run_helper_operator_docs_start(*, skip_local_overrides: bool = True) -> str:
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("PARAKEET_") and not key.startswith("_STT_")
    }
    env["PARAKEET_ROOT"] = str(REPO_ROOT)
    if skip_local_overrides:
        env["_STT_SKIP_LOCAL_OVERRIDES"] = "1"

    command = [
        "bash",
        "-lc",
        f"source {shlex.quote(str(HELPER_PATH))} && stt __operator-docs-start",
    ]
    completed = subprocess.run(
        command,
        check=True,
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
    )
    return completed.stdout.strip()


def _extract_start_reference_block(document: str) -> str:
    start = document.index(START_REFERENCE_START_MARKER) + len(START_REFERENCE_START_MARKER)
    end = document.index(START_REFERENCE_END_MARKER, start)
    return textwrap.dedent(document[start:end]).strip()


def _run_helper_variable_scope_probe() -> dict[str, str]:
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
            "row=outer-row && "
            "command_aliases=outer-command-aliases && "
            "start_cli_arg=outer-start-cli-arg && "
            "export command_aliases && "
            "stt help >/dev/null && "
            "printf 'row=%s\\n' \"$row\" && "
            "printf 'command_aliases=%s\\n' \"$command_aliases\" && "
            "printf 'start_cli_arg=%s\\n' \"$start_cli_arg\" && "
            "if export -p | "
            "grep -F 'declare -x command_aliases=\"outer-command-aliases\"' >/dev/null; "
            "then printf 'command_aliases_exported=true\\n'; "
            "else printf 'command_aliases_exported=false\\n'; fi"
        ),
    ]
    completed = subprocess.run(
        command,
        check=True,
        cwd=REPO_ROOT,
        env=env,
        text=True,
        capture_output=True,
    )

    values: dict[str, str] = {}
    for line in completed.stdout.splitlines():
        key, value = line.split("=", 1)
        values[key] = value
    return values


def _expected_profile_help_label(profile: dict[str, str]) -> str:
    aliases = [profile["start_cli_arg"]]
    aliases.extend(
        alias for alias in profile["mode_aliases"].split(",") if alias != profile["start_cli_arg"]
    )
    label = ",".join(aliases)
    if profile["profile_id"] == "stream-seal":
        return f"(default) {label}"
    return label


def _assert_profile_help_lines(help_text: str, profiles: list[dict[str, str]]) -> None:
    help_lines = [line.strip() for line in help_text.splitlines()]
    for profile in profiles:
        matches = [line for line in help_lines if line.endswith(profile["help_description"])]
        assert len(matches) == 1
        rendered_label = matches[0][: -len(profile["help_description"])].rstrip()
        assert rendered_label == _expected_profile_help_label(profile)


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


def _runtime_truth_contract_fields() -> list[str]:
    return [field for fields in STATUS_RUNTIME_TRUTH_FIELD_GROUPS.values() for field in fields]


def _status_payload(**overrides: object) -> dict[str, object]:
    payload: dict[str, object] = {
        "type": "status",
        "state": "idle",
        "sessions_active": 0,
        "gpu_mem_mb": 1024,
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
        "interim_transcript_enabled": True,
        "interim_transcript_last_source": "live",
        "interim_transcript_live_chunks_processed": 2,
        "interim_transcript_stop_replay_chunks_processed": 1,
        "interim_transcript_updates_emitted": 3,
        "interim_transcript_live_updates_emitted": 2,
        "interim_transcript_stop_replay_updates_emitted": 1,
        "interim_transcript_live_failed": False,
        "interim_transcript_stop_replay_failed": False,
        "interim_transcript_source_fallback_reason": None,
        "overlay_events_enabled": True,
        "overlay_events_emitted": 9,
        "overlay_events_dropped": 1,
        "active_session_age_ms": 321,
        "audio_stop_ms": 0,
        "finalize_ms": 4,
        "infer_ms": 5,
        "send_ms": 6,
        "last_audio_ms": 2400,
        "last_infer_ms": 7,
        "last_send_ms": 8,
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


def test_start_profile_metadata_drives_runtime_defaults() -> None:
    profiles = _run_start_profile_rows()

    assert [profile["profile_id"] for profile in profiles] == ["stream-seal", "offline", "cpu"]
    for profile in profiles:
        args = () if profile["profile_id"] == "stream-seal" else (profile["start_cli_arg"],)
        config = _run_runtime_config(*args)
        expected_device = profile["daemon_device_override"] or "cuda"

        assert config["launch_profile"] == profile["profile_id"]
        assert config["daemon_device"] == expected_device
        assert config["daemon_streaming_enabled"] == profile["daemon_streaming_enabled"]
        assert config["overlay_enabled"] == profile["overlay_enabled_default"]
        assert config["overlay_adaptive_width"] == profile["overlay_adaptive_width_default"]
        assert config["daemon_overlay_events_enabled"] == profile["overlay_enabled_default"]


def test_start_profile_metadata_drives_mode_aliases_and_generated_args() -> None:
    for profile in _run_start_profile_rows():
        for alias in profile["mode_aliases"].split(","):
            config = _run_runtime_config(alias)
            assert config["launch_profile"] == profile["profile_id"]

        args = _run_get_stt_start_args(profile["start_cli_arg"])
        assert args[0] == profile["start_cli_arg"]


def test_start_profile_alias_scan_does_not_leak_shell_variables() -> None:
    values = _run_helper_variable_scope_probe()

    assert values == {
        "row": "outer-row",
        "command_aliases": "outer-command-aliases",
        "start_cli_arg": "outer-start-cli-arg",
        "command_aliases_exported": "true",
    }


def test_start_help_modes_are_generated_from_profile_metadata() -> None:
    help_text = _run_helper_help("start")

    _assert_profile_help_lines(help_text, _run_start_profile_rows())


def test_llm_help_modes_are_generated_from_profile_metadata() -> None:
    profiles = _run_start_profile_rows()
    help_text = _run_helper_help("llm")
    profile_choices = "|".join(profile["start_cli_arg"] for profile in profiles)

    assert f"stt llm [{profile_choices}]" in help_text
    assert f"stt llm start [{profile_choices}]" in help_text
    assert f"stt llm restart [{profile_choices}]" in help_text
    _assert_profile_help_lines(help_text, profiles)


def test_main_help_describes_tmux_as_attach_kill_only() -> None:
    help_text = _run_helper_main_help()

    assert (
        "tmux [attach|kill]     Attach/kill existing helper tmux session; launch with 'stt start'."
        in help_text
    )


@pytest.mark.parametrize("doc_path", [README_PATH, TROUBLESHOOTING_DOC_PATH])
def test_operator_docs_start_reference_matches_helper_metadata(doc_path: Path) -> None:
    generated = _run_helper_operator_docs_start()

    assert _extract_start_reference_block(doc_path.read_text(encoding="utf-8")) == generated


def test_operator_docs_start_reference_uses_matching_end_marker() -> None:
    document = "\n".join(
        [
            START_REFERENCE_END_MARKER,
            START_REFERENCE_START_MARKER,
            "expected",
            START_REFERENCE_END_MARKER,
        ]
    )

    assert _extract_start_reference_block(document) == "expected"


def test_operator_docs_start_reference_ignores_local_overrides() -> None:
    expected = _run_helper_operator_docs_start()
    local_env_path = REPO_ROOT / ".parakeet-stt.local.env"
    local_shell_path = REPO_ROOT / ".parakeet-stt.local.sh"

    with _preserve_paths(local_env_path, local_shell_path):
        local_env_path.write_text(
            "\n".join(
                [
                    "PARAKEET_DEVICE=machine-local-device",
                    "PARAKEET_LLM_SERVER_HOST=llm.machine.local",
                    "PARAKEET_LLM_SERVER_PORT=9999",
                    "PARAKEET_LLM_SYSTEM_PROMPT=machine-local-prompt",
                ]
            )
            + "\n",
            encoding="utf-8",
        )
        local_shell_path.write_text(
            "\n".join(
                [
                    "export PARAKEET_OVERLAY_ENABLED=false",
                    "export PARAKEET_OVERLAY_ADAPTIVE_WIDTH=true",
                ]
            )
            + "\n",
            encoding="utf-8",
        )

        assert _run_helper_operator_docs_start(skip_local_overrides=False) == expected


@pytest.mark.parametrize("tmux_args", [(), ("attach",), ("kill",)])
def test_tmux_surface_does_not_launch_runtime_when_session_is_absent(
    tmp_path: Path,
    tmux_args: tuple[str, ...],
) -> None:
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    tmux_log = tmp_path / "tmux.log"
    _write_fake_command(
        fake_bin,
        "tmux",
        """#!/usr/bin/env bash
printf '%s\\n' "$*" >> "$STT_TEST_TMUX_LOG"
case "${1:-}" in
    has-session)
        exit 1
        ;;
    attach|kill-session)
        exit 0
        ;;
esac
exit 42
""",
    )
    for command_name in ("lsof", "pgrep", "ss"):
        _write_fake_command(fake_bin, command_name, "#!/usr/bin/env bash\nexit 1\n")

    env = {
        "PATH": f"{fake_bin}:{os.environ['PATH']}",
        "STT_TEST_TMUX_LOG": str(tmux_log),
    }
    daemon_port_file = Path("/tmp/parakeet-daemon.port")
    daemon_log = Path("/tmp/parakeet-daemon.log")
    client_log = Path("/tmp/parakeet-ptt.log")
    with _preserve_paths(daemon_port_file, daemon_log, client_log):
        completed = _run_stt_helper_command("tmux", *tmux_args, extra_env=env)

    tmux_calls = tmux_log.read_text(encoding="utf-8").splitlines()
    launch_calls = [
        call
        for call in tmux_calls
        if call.startswith(("new-session", "new-window", "split-window", "select-window"))
    ]

    assert completed.returncode == 0
    assert "No tmux session 'parakeet-stt' found." in completed.stdout
    assert "Start with 'stt start'." in completed.stdout
    assert "Creating tmux session" not in completed.stdout
    assert launch_calls == []


def test_tmux_case_is_attach_kill_only_without_start_runtime_wiring() -> None:
    helper = HELPER_PATH.read_text(encoding="utf-8")
    start = helper.index("        tmux)\n")
    end = helper.index("        check)\n", start)
    tmux_case = helper[start:end]

    forbidden_fragments = [
        "_resolve_port",
        "_load_start_vars_from_defaults",
        "_build_ptt_args",
        "_build_client_cmd",
        "_select_client_runner_mode",
        "PARAKEET_STREAMING_ENABLED",
        "uv run parakeet-stt-daemon",
        "tmux new-session",
        "tmux new-window",
    ]

    for fragment in forbidden_fragments:
        assert fragment not in tmux_case


def test_status_describes_absent_tmux_as_attach_kill_only(tmp_path: Path) -> None:
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    for command_name in ("curl", "lsof", "nc", "pgrep", "ss"):
        _write_fake_command(fake_bin, command_name, "#!/usr/bin/env bash\nexit 1\n")
    _write_fake_command(
        fake_bin,
        "tmux",
        """#!/usr/bin/env bash
if [ "${1:-}" = "has-session" ]; then
    exit 1
fi
exit 42
""",
    )

    env = {"PATH": f"{fake_bin}:{os.environ['PATH']}"}
    client_pid_file = Path("/tmp/parakeet-ptt.pid")
    daemon_pid_file = Path("/tmp/parakeet-daemon.pid")
    daemon_port_file = Path("/tmp/parakeet-daemon.port")
    with _preserve_paths(client_pid_file, daemon_pid_file, daemon_port_file):
        client_pid_file.unlink(missing_ok=True)
        daemon_pid_file.unlink(missing_ok=True)
        daemon_port_file.unlink(missing_ok=True)

        completed = _run_stt_helper_command("status", extra_env=env)

    assert completed.returncode == 0
    assert "tmux session: none (attach/kill only; launch with 'stt start')" in completed.stdout


def test_llm_direct_profile_forwards_remaining_start_args_once(tmp_path: Path) -> None:
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    for command_name in ("curl", "llama-server", "tmux"):
        _write_fake_command(fake_bin, command_name, "#!/usr/bin/env bash\nexit 0\n")
    for command_name in ("lsof", "ss"):
        _write_fake_command(fake_bin, command_name, "#!/usr/bin/env bash\nexit 1\n")

    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("PARAKEET_") and not key.startswith("_STT_")
    }
    env.update(
        {
            "PARAKEET_ROOT": str(REPO_ROOT),
            "PARAKEET_LLM_SERVER_EXTRA_ARGS": "--test-no-model",
            "PATH": f"{fake_bin}:{env['PATH']}",
            "_STT_SKIP_LOCAL_OVERRIDES": "1",
        }
    )

    llm_pid_file = Path("/tmp/parakeet-llama-server.pid")
    llm_port_file = Path("/tmp/parakeet-llama-server.port")
    with _preserve_paths(llm_pid_file, llm_port_file):
        completed = subprocess.run(
            [
                "bash",
                "-lc",
                f"source {shlex.quote(str(HELPER_PATH))} && stt llm cpu --stt-test-sentinel",
            ],
            check=False,
            cwd=REPO_ROOT,
            env=env,
            text=True,
            capture_output=True,
        )

    assert "Unknown option for 'stt start': --stt-test-sentinel" in completed.stdout
    assert "Unknown option for 'stt start': cpu" not in completed.stdout


def test_profile_overlay_env_overrides_metadata_defaults() -> None:
    config = _run_runtime_config(
        "offline",
        extra_env={
            "PARAKEET_OVERLAY_ENABLED": "true",
            "PARAKEET_OVERLAY_ADAPTIVE_WIDTH": "true",
        },
    )

    assert config["overlay_enabled"] == "true"
    assert config["overlay_adaptive_width"] == "true"
    assert config["daemon_overlay_events_enabled"] == "true"


def test_helper_endpoint_defaults_match_daemon_settings() -> None:
    config = _run_runtime_config()

    assert config["daemon_host"] == DEFAULT_DAEMON_HOST
    assert config["daemon_port"] == str(DEFAULT_DAEMON_PORT)
    assert config["daemon_websocket_endpoint"] == daemon_websocket_endpoint()
    assert config["daemon_status_url"] == daemon_status_url()
    assert config["daemon_health_url"] == daemon_health_url()
    assert config["llm_base_url"] == managed_llm_api_base_url()
    assert config["managed_llm_api_base_url"] == managed_llm_api_base_url()
    assert config["llm_health_url"] == managed_llm_health_url()


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
    assert config["daemon_health_url"] == daemon_health_url("0.0.0.0", 9001)
    assert config["llm_base_url"] == managed_llm_api_base_url("llm.local", 8181)
    assert config["managed_llm_api_base_url"] == managed_llm_api_base_url("llm.local", 8181)
    assert config["llm_health_url"] == managed_llm_health_url("llm.local", 8181)


def test_helper_start_cli_llm_base_url_overrides_env() -> None:
    config = _run_runtime_config(
        "--llm-base-url",
        "http://cli.local:8182/custom",
        extra_env={"PARAKEET_LLM_BASE_URL": "http://env.local:8181/v1"},
    )

    assert config["llm_base_url"] == "http://cli.local:8182/custom"
    assert config["managed_llm_api_base_url"] == "http://127.0.0.1:8081/v1"


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


def test_runtime_match_accepts_missing_optional_overlay_truth() -> None:
    assert _run_runtime_match("cuda", "true", "true", "cuda", "true", "") is True


def test_runtime_match_rejects_present_overlay_mismatch() -> None:
    assert _run_runtime_match("cuda", "true", "true", "cuda", "true", "false") is False


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
        json.dumps(_status_payload()),
        encoding="utf-8",
    )

    truth = _run_status_runtime_truth(payload_path)

    assert set(truth) == STATUS_RUNTIME_TRUTH_FIELDS
    assert list(truth) == _runtime_truth_contract_fields()
    assert truth == {
        "type": "status",
        "state": "idle",
        "sessions_active": "0",
        "gpu_mem_mb": "1024",
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
        "interim_transcript_enabled": "true",
        "interim_transcript_last_source": "live",
        "interim_transcript_live_chunks_processed": "2",
        "interim_transcript_stop_replay_chunks_processed": "1",
        "interim_transcript_updates_emitted": "3",
        "interim_transcript_live_updates_emitted": "2",
        "interim_transcript_stop_replay_updates_emitted": "1",
        "interim_transcript_live_failed": "false",
        "interim_transcript_stop_replay_failed": "false",
        "interim_transcript_source_fallback_reason": "",
        "overlay_events_enabled": "true",
        "overlay_events_emitted": "9",
        "overlay_events_dropped": "1",
        "active_session_age_ms": "321",
        "audio_stop_ms": "0",
        "finalize_ms": "4",
        "infer_ms": "5",
        "send_ms": "6",
        "last_audio_ms": "2400",
        "last_infer_ms": "7",
        "last_send_ms": "8",
    }


def test_daemon_status_runtime_truth_accepts_minimal_protocol_status(
    tmp_path: Path,
) -> None:
    payload_path = tmp_path / "status.json"
    payload_path.write_text(
        json.dumps({"type": "status", "state": "idle", "sessions_active": 0}),
        encoding="utf-8",
    )

    truth = _run_status_runtime_truth(payload_path)

    assert set(truth) == STATUS_RUNTIME_TRUTH_FIELDS
    assert truth["type"] == "status"
    assert truth["state"] == "idle"
    assert truth["sessions_active"] == "0"
    for field in STATUS_RUNTIME_TRUTH_FIELDS - {"type", "state", "sessions_active"}:
        assert truth[field] == ""


def test_daemon_status_runtime_truth_accepts_protocol_status_fixtures() -> None:
    for payload_path in sorted(FIXTURE_DIR.glob("status_*.json")):
        truth = _run_status_runtime_truth(payload_path)

        assert set(truth) == STATUS_RUNTIME_TRUTH_FIELDS, payload_path.name
        assert list(truth) == _runtime_truth_contract_fields(), payload_path.name


def test_daemon_status_runtime_truth_normalizes_numeric_string(tmp_path: Path) -> None:
    payload_path = tmp_path / "status.json"
    payload_path.write_text(
        json.dumps(_status_payload(chunk_secs="2.4000")),
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
        json.dumps(_status_payload(chunk_secs="inf")),
        encoding="utf-8",
    )

    with pytest.raises(subprocess.CalledProcessError):
        _run_status_runtime_truth(payload_path)


def test_status_runtime_truth_rejects_non_finite_numeric_tokens(tmp_path: Path) -> None:
    for index, chunk_secs in enumerate((float("inf"), float("-inf"), float("nan"))):
        payload_path = tmp_path / f"status-{index}.json"
        payload_path.write_text(
            json.dumps(_status_payload(chunk_secs=chunk_secs)),
            encoding="utf-8",
        )

        with pytest.raises(subprocess.CalledProcessError):
            _run_status_runtime_truth(payload_path)


def test_status_uses_persisted_daemon_authority_for_liveness(
    tmp_path: Path,
) -> None:
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    listener_port = "9876"
    listener_pid = "424242"
    expected_status_url = f"http://127.0.0.1:{listener_port}/status"
    curl_url_file = tmp_path / "curl-url.txt"

    _write_fake_command(
        fake_bin,
        "lsof",
        """#!/usr/bin/env bash
if [ "${1:-}" = "-tiTCP:${STT_TEST_LISTENER_PORT}" ]; then
    printf '%s\\n' "$STT_TEST_LISTENER_PID"
    exit 0
fi
exit 1
""",
    )
    _write_fake_command(
        fake_bin,
        "ps",
        """#!/usr/bin/env bash
if [ "${1:-}" = "-p" ] && [ "${2:-}" = "$STT_TEST_LISTENER_PID" ] && [ "${3:-}" = "-o" ]; then
    printf '%s\\n' "python -m parakeet-stt-daemon"
    exit 0
fi
if [ "${1:-}" = "-p" ] && [ "${2:-}" = "$STT_TEST_LISTENER_PID" ]; then
    exit 0
fi
exit 1
""",
    )
    _write_fake_command(
        fake_bin,
        "curl",
        """#!/usr/bin/env bash
url="${@: -1}"
printf '%s\\n' "$url" > "$STT_TEST_CURL_URL_FILE"
if [ "$url" = "$STT_TEST_EXPECTED_STATUS_URL" ]; then
    printf '%s' "$STT_TEST_STATUS_PAYLOAD"
    exit 0
fi
exit 22
""",
    )
    for command_name in ("nc", "pgrep", "tmux"):
        _write_fake_command(
            fake_bin,
            command_name,
            "#!/usr/bin/env bash\nexit 1\n",
        )

    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("PARAKEET_") and not key.startswith("_STT_")
    }
    env.update(
        {
            "PARAKEET_ROOT": str(REPO_ROOT),
            "PARAKEET_PORT": "8765",
            "PATH": f"{fake_bin}:{env['PATH']}",
            "_STT_SKIP_LOCAL_OVERRIDES": "1",
            "STT_TEST_CURL_URL_FILE": str(curl_url_file),
            "STT_TEST_EXPECTED_STATUS_URL": expected_status_url,
            "STT_TEST_LISTENER_PID": listener_pid,
            "STT_TEST_LISTENER_PORT": listener_port,
            "STT_TEST_STATUS_PAYLOAD": json.dumps(
                {"type": "status", "state": "idle", "sessions_active": 0}
            ),
        }
    )

    port_file = Path("/tmp/parakeet-daemon.port")
    pid_file = Path("/tmp/parakeet-daemon.pid")
    with _preserve_paths(port_file, pid_file):
        port_file.write_text(f"{listener_port}\n", encoding="utf-8")
        pid_file.unlink(missing_ok=True)

        completed = subprocess.run(
            [
                "bash",
                "-lc",
                f"source {shlex.quote(str(HELPER_PATH))} && stt status",
            ],
            check=True,
            cwd=REPO_ROOT,
            env=env,
            text=True,
            capture_output=True,
        )

        assert f"Daemon running (pid {listener_pid})" in completed.stdout
        assert f"Endpoint: ws://127.0.0.1:{listener_port}/ws" in completed.stdout
        assert "Daemon runtime truth:" in completed.stdout
        assert "sessions_active=0" in completed.stdout
        assert curl_url_file.read_text(encoding="utf-8").strip() == expected_status_url
        assert pid_file.read_text(encoding="utf-8").strip() == listener_pid


def test_stop_refreshes_daemon_identity_from_bound_listener_before_shutdown(
    tmp_path: Path,
) -> None:
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    listener_port = "9877"
    listener_pid = "424243"
    launcher_pid = "111111"
    expected_status_url = f"http://127.0.0.1:{listener_port}/status"
    kill_log = tmp_path / "kill.log"
    killed_marker = tmp_path / "listener-killed"

    _write_fake_command(
        fake_bin,
        "lsof",
        """#!/usr/bin/env bash
if [ "${1:-}" = "-tiTCP:${STT_TEST_LISTENER_PORT}" ]; then
    printf '%s\\n' "$STT_TEST_LISTENER_PID"
    exit 0
fi
exit 1
""",
    )
    _write_fake_command(
        fake_bin,
        "ps",
        """#!/usr/bin/env bash
if [ "${1:-}" = "-p" ]; then
    if [ "${2:-}" = "$STT_TEST_LISTENER_PID" ]; then
        if [ "${3:-}" = "-o" ]; then
            printf '%s\\n' "python -m parakeet-stt-daemon"
            exit 0
        fi
        [ ! -f "$STT_TEST_KILLED_MARKER" ]
        exit $?
    fi
    if [ "${2:-}" = "$STT_TEST_LAUNCHER_PID" ]; then
        exit 0
    fi
fi
exit 1
""",
    )
    _write_fake_command(
        fake_bin,
        "nc",
        """#!/usr/bin/env bash
if [ "${1:-}" = "-z" ] && [ "${3:-}" = "$STT_TEST_LISTENER_PORT" ]; then
    [ ! -f "$STT_TEST_KILLED_MARKER" ]
    exit $?
fi
exit 1
""",
    )
    _write_fake_command(
        fake_bin,
        "curl",
        """#!/usr/bin/env bash
url="${@: -1}"
if [ "$url" = "$STT_TEST_EXPECTED_STATUS_URL" ]; then
    printf '%s' "$STT_TEST_STATUS_PAYLOAD"
    exit 0
fi
exit 22
""",
    )
    for command_name in ("pgrep", "pkill", "tmux"):
        _write_fake_command(
            fake_bin,
            command_name,
            "#!/usr/bin/env bash\nexit 1\n",
        )

    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("PARAKEET_") and not key.startswith("_STT_")
    }
    env.update(
        {
            "PARAKEET_ROOT": str(REPO_ROOT),
            "PARAKEET_PORT": "8765",
            "PATH": f"{fake_bin}:{env['PATH']}",
            "_STT_SKIP_LOCAL_OVERRIDES": "1",
            "STT_TEST_KILL_LOG": str(kill_log),
            "STT_TEST_KILLED_MARKER": str(killed_marker),
            "STT_TEST_EXPECTED_STATUS_URL": expected_status_url,
            "STT_TEST_LAUNCHER_PID": launcher_pid,
            "STT_TEST_LISTENER_PID": listener_pid,
            "STT_TEST_LISTENER_PORT": listener_port,
            "STT_TEST_STATUS_PAYLOAD": json.dumps(
                {"type": "status", "state": "idle", "sessions_active": 0}
            ),
        }
    )

    client_pid_file = Path("/tmp/parakeet-ptt.pid")
    daemon_pid_file = Path("/tmp/parakeet-daemon.pid")
    daemon_port_file = Path("/tmp/parakeet-daemon.port")
    llm_pid_file = Path("/tmp/parakeet-llama-server.pid")
    llm_port_file = Path("/tmp/parakeet-llama-server.port")
    with _preserve_paths(
        client_pid_file,
        daemon_pid_file,
        daemon_port_file,
        llm_pid_file,
        llm_port_file,
    ):
        client_pid_file.unlink(missing_ok=True)
        llm_pid_file.unlink(missing_ok=True)
        llm_port_file.unlink(missing_ok=True)
        daemon_pid_file.write_text(f"{launcher_pid}\n", encoding="utf-8")
        daemon_port_file.write_text(f"127.0.0.1:{listener_port}\n", encoding="utf-8")

        completed = subprocess.run(
            [
                "bash",
                "-lc",
                f"""
set -e
source {shlex.quote(str(HELPER_PATH))}
kill() {{
    local signal=""
    local pid=""
    for arg in "$@"; do
        case "$arg" in
            -*) signal="$arg" ;;
            *) pid="$arg" ;;
        esac
    done
    printf '%s %s\\n' "$signal" "$pid" >> "$STT_TEST_KILL_LOG"
    if [ "$pid" = "$STT_TEST_LISTENER_PID" ]; then
        : > "$STT_TEST_KILLED_MARKER"
    fi
    return 0
}}
stt stop
""",
            ],
            check=True,
            cwd=REPO_ROOT,
            env=env,
            text=True,
            capture_output=True,
        )

        killed_pids = [
            line.rsplit(maxsplit=1)[-1]
            for line in kill_log.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
        assert "Daemon stopped" in completed.stdout
        assert listener_pid in killed_pids
        assert launcher_pid not in killed_pids
        assert not daemon_pid_file.exists()
        assert not daemon_port_file.exists()


def test_stop_does_not_kill_listener_from_stale_daemon_port_file(
    tmp_path: Path,
) -> None:
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    listener_port = "9878"
    unrelated_listener_pid = "525252"
    stale_pid = "626262"
    expected_health_url = f"http://127.0.0.1:{listener_port}/healthz"
    kill_log = tmp_path / "kill.log"

    _write_fake_command(
        fake_bin,
        "lsof",
        """#!/usr/bin/env bash
if [ "${1:-}" = "-tiTCP:${STT_TEST_LISTENER_PORT}" ]; then
    printf '%s\\n' "$STT_TEST_UNRELATED_LISTENER_PID"
    exit 0
fi
exit 1
""",
    )
    _write_fake_command(
        fake_bin,
        "ps",
        """#!/usr/bin/env bash
if [ "${1:-}" = "-p" ]; then
    if [ "${2:-}" = "$STT_TEST_UNRELATED_LISTENER_PID" ]; then
        exit 0
    fi
    if [ "${2:-}" = "$STT_TEST_STALE_PID" ]; then
        exit 0
    fi
fi
exit 1
""",
    )
    _write_fake_command(
        fake_bin,
        "nc",
        """#!/usr/bin/env bash
if [ "${1:-}" = "-z" ] && [ "${3:-}" = "$STT_TEST_LISTENER_PORT" ]; then
    exit 0
fi
exit 1
""",
    )
    _write_fake_command(
        fake_bin,
        "curl",
        """#!/usr/bin/env bash
url="${@: -1}"
if [ "$url" = "$STT_TEST_EXPECTED_HEALTH_URL" ]; then
    exit 0
fi
exit 22
""",
    )
    for command_name in ("pgrep", "pkill", "tmux"):
        _write_fake_command(
            fake_bin,
            command_name,
            "#!/usr/bin/env bash\nexit 1\n",
        )

    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("PARAKEET_") and not key.startswith("_STT_")
    }
    env.update(
        {
            "PARAKEET_ROOT": str(REPO_ROOT),
            "PARAKEET_PORT": "8765",
            "PATH": f"{fake_bin}:{env['PATH']}",
            "_STT_SKIP_LOCAL_OVERRIDES": "1",
            "STT_TEST_EXPECTED_HEALTH_URL": expected_health_url,
            "STT_TEST_KILL_LOG": str(kill_log),
            "STT_TEST_LISTENER_PORT": listener_port,
            "STT_TEST_STALE_PID": stale_pid,
            "STT_TEST_UNRELATED_LISTENER_PID": unrelated_listener_pid,
        }
    )

    client_pid_file = Path("/tmp/parakeet-ptt.pid")
    daemon_pid_file = Path("/tmp/parakeet-daemon.pid")
    daemon_port_file = Path("/tmp/parakeet-daemon.port")
    llm_pid_file = Path("/tmp/parakeet-llama-server.pid")
    llm_port_file = Path("/tmp/parakeet-llama-server.port")
    with _preserve_paths(
        client_pid_file,
        daemon_pid_file,
        daemon_port_file,
        llm_pid_file,
        llm_port_file,
    ):
        client_pid_file.unlink(missing_ok=True)
        daemon_pid_file.write_text(f"{stale_pid}\n", encoding="utf-8")
        llm_pid_file.unlink(missing_ok=True)
        llm_port_file.unlink(missing_ok=True)
        daemon_port_file.write_text(f"127.0.0.1:{listener_port}\n", encoding="utf-8")

        completed = subprocess.run(
            [
                "bash",
                "-lc",
                f"""
set -e
source {shlex.quote(str(HELPER_PATH))}
kill() {{
    printf '%s\\n' "$*" >> "$STT_TEST_KILL_LOG"
    return 0
}}
stt stop
""",
            ],
            check=True,
            cwd=REPO_ROOT,
            env=env,
            text=True,
            capture_output=True,
        )

        assert "Daemon stopped" not in completed.stdout
        assert not kill_log.exists()
        assert not daemon_pid_file.exists()
        assert not daemon_port_file.exists()


def test_stop_falls_back_to_current_daemon_authority_when_port_file_is_stale(
    tmp_path: Path,
) -> None:
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    current_port = "8765"
    stale_port = "9878"
    listener_pid = "424245"
    unrelated_listener_pid = "525253"
    launcher_pid = "111113"
    expected_status_url = f"http://127.0.0.1:{current_port}/status"
    kill_log = tmp_path / "kill.log"
    killed_marker = tmp_path / "listener-killed"

    _write_fake_command(
        fake_bin,
        "lsof",
        """#!/usr/bin/env bash
if [ "${1:-}" = "-tiTCP:${STT_TEST_CURRENT_PORT}" ]; then
    printf '%s\\n' "$STT_TEST_LISTENER_PID"
    exit 0
fi
if [ "${1:-}" = "-tiTCP:${STT_TEST_STALE_PORT}" ]; then
    printf '%s\\n' "$STT_TEST_UNRELATED_LISTENER_PID"
    exit 0
fi
exit 1
""",
    )
    _write_fake_command(
        fake_bin,
        "ps",
        """#!/usr/bin/env bash
if [ "${1:-}" = "-p" ]; then
    if [ "${2:-}" = "$STT_TEST_LISTENER_PID" ]; then
        if [ "${3:-}" = "-o" ]; then
            printf '%s\\n' "python -m parakeet-stt-daemon"
            exit 0
        fi
        [ ! -f "$STT_TEST_KILLED_MARKER" ]
        exit $?
    fi
    if [ "${2:-}" = "$STT_TEST_UNRELATED_LISTENER_PID" ]; then
        exit 0
    fi
fi
exit 1
""",
    )
    _write_fake_command(
        fake_bin,
        "nc",
        """#!/usr/bin/env bash
if [ "${1:-}" = "-z" ] && [ "${3:-}" = "$STT_TEST_CURRENT_PORT" ]; then
    [ ! -f "$STT_TEST_KILLED_MARKER" ]
    exit $?
fi
if [ "${1:-}" = "-z" ] && [ "${3:-}" = "$STT_TEST_STALE_PORT" ]; then
    exit 0
fi
exit 1
""",
    )
    _write_fake_command(
        fake_bin,
        "curl",
        """#!/usr/bin/env bash
url="${@: -1}"
if [ "$url" = "$STT_TEST_EXPECTED_STATUS_URL" ]; then
    printf '%s' "$STT_TEST_STATUS_PAYLOAD"
    exit 0
fi
exit 22
""",
    )
    for command_name in ("pgrep", "pkill", "tmux"):
        _write_fake_command(
            fake_bin,
            command_name,
            "#!/usr/bin/env bash\nexit 1\n",
        )

    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("PARAKEET_") and not key.startswith("_STT_")
    }
    env.update(
        {
            "PARAKEET_ROOT": str(REPO_ROOT),
            "PARAKEET_PORT": current_port,
            "PATH": f"{fake_bin}:{env['PATH']}",
            "_STT_SKIP_LOCAL_OVERRIDES": "1",
            "STT_TEST_CURRENT_PORT": current_port,
            "STT_TEST_EXPECTED_STATUS_URL": expected_status_url,
            "STT_TEST_KILL_LOG": str(kill_log),
            "STT_TEST_KILLED_MARKER": str(killed_marker),
            "STT_TEST_LISTENER_PID": listener_pid,
            "STT_TEST_STALE_PORT": stale_port,
            "STT_TEST_STATUS_PAYLOAD": json.dumps(
                {"type": "status", "state": "idle", "sessions_active": 0}
            ),
            "STT_TEST_UNRELATED_LISTENER_PID": unrelated_listener_pid,
        }
    )

    client_pid_file = Path("/tmp/parakeet-ptt.pid")
    daemon_pid_file = Path("/tmp/parakeet-daemon.pid")
    daemon_port_file = Path("/tmp/parakeet-daemon.port")
    llm_pid_file = Path("/tmp/parakeet-llama-server.pid")
    llm_port_file = Path("/tmp/parakeet-llama-server.port")
    with _preserve_paths(
        client_pid_file,
        daemon_pid_file,
        daemon_port_file,
        llm_pid_file,
        llm_port_file,
    ):
        client_pid_file.unlink(missing_ok=True)
        llm_pid_file.unlink(missing_ok=True)
        llm_port_file.unlink(missing_ok=True)
        daemon_pid_file.write_text(f"{launcher_pid}\n", encoding="utf-8")
        daemon_port_file.write_text(f"127.0.0.1:{stale_port}\n", encoding="utf-8")

        completed = subprocess.run(
            [
                "bash",
                "-lc",
                f"""
set -e
source {shlex.quote(str(HELPER_PATH))}
kill() {{
    local signal=""
    local pid=""
    for arg in "$@"; do
        case "$arg" in
            -*) signal="$arg" ;;
            *) pid="$arg" ;;
        esac
    done
    printf '%s %s\\n' "$signal" "$pid" >> "$STT_TEST_KILL_LOG"
    if [ "$pid" = "$STT_TEST_LISTENER_PID" ]; then
        : > "$STT_TEST_KILLED_MARKER"
    fi
    return 0
}}
stt stop
""",
            ],
            check=True,
            cwd=REPO_ROOT,
            env=env,
            text=True,
            capture_output=True,
        )

        killed_pids = [
            line.rsplit(maxsplit=1)[-1]
            for line in kill_log.read_text(encoding="utf-8").splitlines()
            if line.strip()
        ]
        assert "Daemon stopped" in completed.stdout
        assert listener_pid in killed_pids
        assert unrelated_listener_pid not in killed_pids
        assert not daemon_pid_file.exists()
        assert not daemon_port_file.exists()


def test_stop_preserves_daemon_identity_when_shutdown_fails(
    tmp_path: Path,
) -> None:
    fake_bin = tmp_path / "bin"
    fake_bin.mkdir()
    listener_port = "9879"
    listener_pid = "424244"
    launcher_pid = "111112"
    expected_status_url = f"http://127.0.0.1:{listener_port}/status"

    _write_fake_command(
        fake_bin,
        "lsof",
        """#!/usr/bin/env bash
if [ "${1:-}" = "-tiTCP:${STT_TEST_LISTENER_PORT}" ]; then
    printf '%s\\n' "$STT_TEST_LISTENER_PID"
    exit 0
fi
exit 1
""",
    )
    _write_fake_command(
        fake_bin,
        "ps",
        """#!/usr/bin/env bash
if [ "${1:-}" = "-p" ] && [ "${2:-}" = "$STT_TEST_LISTENER_PID" ] && [ "${3:-}" = "-o" ]; then
    printf '%s\\n' "python -m parakeet-stt-daemon"
    exit 0
fi
if [ "${1:-}" = "-p" ] && [ "${2:-}" = "$STT_TEST_LISTENER_PID" ]; then
    exit 0
fi
exit 1
""",
    )
    _write_fake_command(
        fake_bin,
        "nc",
        """#!/usr/bin/env bash
if [ "${1:-}" = "-z" ] && [ "${3:-}" = "$STT_TEST_LISTENER_PORT" ]; then
    exit 0
fi
exit 1
""",
    )
    _write_fake_command(
        fake_bin,
        "curl",
        """#!/usr/bin/env bash
url="${@: -1}"
if [ "$url" = "$STT_TEST_EXPECTED_STATUS_URL" ]; then
    printf '%s' "$STT_TEST_STATUS_PAYLOAD"
    exit 0
fi
exit 22
""",
    )
    for command_name in ("pgrep", "pkill", "tmux"):
        _write_fake_command(
            fake_bin,
            command_name,
            "#!/usr/bin/env bash\nexit 1\n",
        )

    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("PARAKEET_") and not key.startswith("_STT_")
    }
    env.update(
        {
            "PARAKEET_ROOT": str(REPO_ROOT),
            "PARAKEET_PORT": "8765",
            "PATH": f"{fake_bin}:{env['PATH']}",
            "_STT_SKIP_LOCAL_OVERRIDES": "1",
            "STT_TEST_EXPECTED_STATUS_URL": expected_status_url,
            "STT_TEST_LISTENER_PID": listener_pid,
            "STT_TEST_LISTENER_PORT": listener_port,
            "STT_TEST_STATUS_PAYLOAD": json.dumps(
                {"type": "status", "state": "idle", "sessions_active": 0}
            ),
        }
    )

    client_pid_file = Path("/tmp/parakeet-ptt.pid")
    daemon_pid_file = Path("/tmp/parakeet-daemon.pid")
    daemon_port_file = Path("/tmp/parakeet-daemon.port")
    llm_pid_file = Path("/tmp/parakeet-llama-server.pid")
    llm_port_file = Path("/tmp/parakeet-llama-server.port")
    with _preserve_paths(
        client_pid_file,
        daemon_pid_file,
        daemon_port_file,
        llm_pid_file,
        llm_port_file,
    ):
        client_pid_file.unlink(missing_ok=True)
        llm_pid_file.unlink(missing_ok=True)
        llm_port_file.unlink(missing_ok=True)
        daemon_pid_file.write_text(f"{launcher_pid}\n", encoding="utf-8")
        daemon_port_file.write_text(f"127.0.0.1:{listener_port}\n", encoding="utf-8")

        completed = subprocess.run(
            [
                "bash",
                "-lc",
                f"""
source {shlex.quote(str(HELPER_PATH))}
kill() {{
    return 0
}}
sleep() {{
    return 0
}}
stt stop
""",
            ],
            check=False,
            cwd=REPO_ROOT,
            env=env,
            text=True,
            capture_output=True,
        )

        assert completed.returncode == 1
        assert "Failed to stop the running daemon" in completed.stdout
        assert daemon_pid_file.read_text(encoding="utf-8").strip() == listener_pid
        assert daemon_port_file.read_text(encoding="utf-8").strip() == (
            f"127.0.0.1:{listener_port}"
        )
