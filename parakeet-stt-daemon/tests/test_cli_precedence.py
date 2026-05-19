"""Tests for CLI settings and startup checks."""

from __future__ import annotations

from typing import Any

import pytest

from parakeet_stt_daemon import __main__ as daemon_main
from parakeet_stt_daemon.config import DEFAULT_DAEMON_HOST, DEFAULT_DAEMON_PORT


class _FakeAudio:
    def __init__(self, start_error: Exception | None = None) -> None:
        self._start_error = start_error
        self.start_calls = 0
        self.stop_calls = 0

    def start(self) -> None:
        self.start_calls += 1
        if self._start_error is not None:
            raise self._start_error

    def stop(self) -> None:
        self.stop_calls += 1


class _FakeTranscriber:
    def __init__(self, warmup_error: Exception | None = None) -> None:
        self._warmup_error = warmup_error
        self.warmup_calls = 0

    def warmup(self) -> None:
        self.warmup_calls += 1
        if self._warmup_error is not None:
            raise self._warmup_error


class _FakeRuntimeTruth:
    stream_helper_active = True
    stream_helper_class_name = "FakeStreamingHelper"
    stream_helper_scope = "live_session_only"
    stream_fallback_reason = None
    stream_path_executed = False
    stream_chunks_processed = 0
    finalization_mode = "offline_seal"
    final_audio_source = "canonical_session_audio"
    tail_trim_mode = "rms"
    vad_active = True
    vad_fallback_reason = None

    def to_log_record(self) -> dict[str, str]:
        return {"runtime": "test"}


class _FakeOrchestrator:
    def __init__(
        self,
        audio: _FakeAudio,
        transcriber: _FakeTranscriber,
    ) -> None:
        self.audio = audio
        self.transcriber = transcriber

    def prepare_vad(self) -> None:
        return None

    def runtime_truth(self, *, overlay_events_enabled: bool) -> _FakeRuntimeTruth:
        del overlay_events_enabled
        return _FakeRuntimeTruth()


def _install_fake_daemon_server(
    monkeypatch: pytest.MonkeyPatch,
    *,
    audio_start_error: Exception | None = None,
    warmup_error: Exception | None = None,
) -> _FakeOrchestrator:
    orchestrator = _FakeOrchestrator(
        _FakeAudio(audio_start_error),
        _FakeTranscriber(warmup_error),
    )

    class FakeDaemonServer:
        def __init__(self, settings: Any) -> None:
            del settings
            self.orchestrator = orchestrator

    monkeypatch.setattr(daemon_main, "DaemonServer", FakeDaemonServer)
    monkeypatch.setattr(
        daemon_main.sd,
        "query_devices",
        lambda: [{"name": "Fake microphone", "max_input_channels": 1}],
    )
    return orchestrator


def test_parse_args_boolean_flags_default_to_none() -> None:
    args = daemon_main._parse_args([])

    assert args.status_enabled is None
    assert args.streaming_enabled is None


def test_env_values_apply_when_cli_flags_absent(monkeypatch) -> None:
    monkeypatch.setenv("PARAKEET_HOST", "0.0.0.0")
    monkeypatch.setenv("PARAKEET_PORT", "9876")
    monkeypatch.setenv("PARAKEET_STATUS_ENABLED", "false")
    monkeypatch.setenv("PARAKEET_STREAMING_ENABLED", "true")
    monkeypatch.setenv("PARAKEET_OVERLAY_EVENTS_ENABLED", "true")

    settings = daemon_main._build_settings(daemon_main._parse_args([]))

    assert settings.host == "0.0.0.0"
    assert settings.port == 9876
    assert settings.status_enabled is False
    assert settings.streaming_enabled is True
    assert settings.overlay_events_enabled is True


def test_cli_explicit_disable_overrides_env_true(monkeypatch) -> None:
    monkeypatch.setenv("PARAKEET_STATUS_ENABLED", "true")
    monkeypatch.setenv("PARAKEET_STREAMING_ENABLED", "true")

    settings = daemon_main._build_settings(
        daemon_main._parse_args(["--no-status", "--no-streaming"])
    )

    assert settings.status_enabled is False
    assert settings.streaming_enabled is False


def test_unrelated_cli_args_do_not_override_env_booleans(monkeypatch) -> None:
    monkeypatch.setenv("PARAKEET_STATUS_ENABLED", "false")
    monkeypatch.setenv("PARAKEET_STREAMING_ENABLED", "false")

    settings = daemon_main._build_settings(
        daemon_main._parse_args(["--host", "0.0.0.0", "--port", "9000"])
    )

    assert settings.host == "0.0.0.0"
    assert settings.port == 9000
    assert settings.status_enabled is False
    assert settings.streaming_enabled is False


def test_cli_host_port_override_env(monkeypatch) -> None:
    monkeypatch.setenv("PARAKEET_HOST", "0.0.0.0")
    monkeypatch.setenv("PARAKEET_PORT", "7000")

    settings = daemon_main._build_settings(
        daemon_main._parse_args(["--host", "127.0.0.2", "--port", "9001"])
    )

    assert settings.host == "127.0.0.2"
    assert settings.port == 9001


def test_defaults_apply_without_env_or_cli(monkeypatch) -> None:
    monkeypatch.delenv("PARAKEET_HOST", raising=False)
    monkeypatch.delenv("PARAKEET_PORT", raising=False)
    monkeypatch.delenv("PARAKEET_STATUS_ENABLED", raising=False)
    monkeypatch.delenv("PARAKEET_STREAMING_ENABLED", raising=False)
    monkeypatch.delenv("PARAKEET_OVERLAY_EVENTS_ENABLED", raising=False)

    settings = daemon_main._build_settings(daemon_main._parse_args([]))

    assert settings.host == DEFAULT_DAEMON_HOST
    assert settings.port == DEFAULT_DAEMON_PORT
    assert settings.status_enabled is True
    assert settings.streaming_enabled is False
    assert settings.overlay_events_enabled is False
    assert settings.max_session_seconds == 600.0
    assert settings.max_session_samples is None


def test_env_session_limits_apply_when_cli_absent(monkeypatch) -> None:
    monkeypatch.setenv("PARAKEET_MAX_SESSION_SECONDS", "45")
    monkeypatch.setenv("PARAKEET_MAX_SESSION_SAMPLES", "12345")

    settings = daemon_main._build_settings(daemon_main._parse_args([]))

    assert settings.max_session_seconds == 45.0
    assert settings.max_session_samples == 12_345


def test_cli_session_limits_override_env(monkeypatch) -> None:
    monkeypatch.setenv("PARAKEET_MAX_SESSION_SECONDS", "30")
    monkeypatch.setenv("PARAKEET_MAX_SESSION_SAMPLES", "1000")

    settings = daemon_main._build_settings(
        daemon_main._parse_args(["--max-session-seconds", "12", "--max-session-samples", "2048"])
    )

    assert settings.max_session_seconds == 12.0
    assert settings.max_session_samples == 2_048


def test_run_checks_fails_when_audio_startup_fails(monkeypatch: pytest.MonkeyPatch) -> None:
    orchestrator = _install_fake_daemon_server(
        monkeypatch,
        audio_start_error=RuntimeError("audio device unavailable"),
    )

    with pytest.raises(RuntimeError, match="audio stream check failed: audio device unavailable"):
        daemon_main.run_checks(
            daemon_main._build_settings(daemon_main._parse_args(["--device", "cpu"]))
        )

    assert orchestrator.audio.start_calls == 1
    assert orchestrator.audio.stop_calls == 0


def test_run_checks_fails_when_model_warmup_fails(monkeypatch: pytest.MonkeyPatch) -> None:
    orchestrator = _install_fake_daemon_server(
        monkeypatch,
        warmup_error=RuntimeError("decoder unavailable"),
    )

    with pytest.raises(RuntimeError, match="model warmup check failed: decoder unavailable"):
        daemon_main.run_checks(
            daemon_main._build_settings(daemon_main._parse_args(["--device", "cpu"]))
        )

    assert orchestrator.transcriber.warmup_calls == 1


def test_main_check_exits_nonzero_when_startup_checks_fail(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    def fail_checks(settings: object) -> None:
        del settings
        raise RuntimeError("startup gate failed")

    monkeypatch.setattr(daemon_main, "run_checks", fail_checks)

    with pytest.raises(SystemExit) as exc_info:
        daemon_main.main(["--check", "--device", "cpu"])

    assert exc_info.value.code == 1
