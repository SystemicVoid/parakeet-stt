"""Tests for CLI/environment precedence when building daemon settings."""

from __future__ import annotations

from parakeet_stt_daemon.__main__ import _build_settings, _parse_args


def test_parse_args_boolean_flags_default_to_none() -> None:
    args = _parse_args([])

    assert args.status_enabled is None
    assert args.streaming_enabled is None


def test_env_values_apply_when_cli_flags_absent(monkeypatch) -> None:
    monkeypatch.setenv("PARAKEET_STATUS_ENABLED", "false")
    monkeypatch.setenv("PARAKEET_STREAMING_ENABLED", "true")
    monkeypatch.setenv("PARAKEET_OVERLAY_EVENTS_ENABLED", "true")

    settings = _build_settings(_parse_args([]))

    assert settings.status_enabled is False
    assert settings.streaming_enabled is True
    assert settings.overlay_events_enabled is True


def test_cli_explicit_disable_overrides_env_true(monkeypatch) -> None:
    monkeypatch.setenv("PARAKEET_STATUS_ENABLED", "true")
    monkeypatch.setenv("PARAKEET_STREAMING_ENABLED", "true")

    settings = _build_settings(_parse_args(["--no-status", "--no-streaming"]))

    assert settings.status_enabled is False
    assert settings.streaming_enabled is False


def test_unrelated_cli_args_do_not_override_env_booleans(monkeypatch) -> None:
    monkeypatch.setenv("PARAKEET_STATUS_ENABLED", "false")
    monkeypatch.setenv("PARAKEET_STREAMING_ENABLED", "false")

    settings = _build_settings(_parse_args(["--host", "0.0.0.0", "--port", "9000"]))

    assert settings.host == "0.0.0.0"
    assert settings.port == 9000
    assert settings.status_enabled is False
    assert settings.streaming_enabled is False


def test_defaults_apply_without_env_or_cli(monkeypatch) -> None:
    monkeypatch.delenv("PARAKEET_STATUS_ENABLED", raising=False)
    monkeypatch.delenv("PARAKEET_STREAMING_ENABLED", raising=False)
    monkeypatch.delenv("PARAKEET_OVERLAY_EVENTS_ENABLED", raising=False)

    settings = _build_settings(_parse_args([]))

    assert settings.status_enabled is True
    assert settings.streaming_enabled is False
    assert settings.overlay_events_enabled is False
