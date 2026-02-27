"""Streaming helper truth signals: active/fallback state transitions."""

from __future__ import annotations

import asyncio
from typing import Any, cast
from uuid import uuid4

from parakeet_stt_daemon.config import ServerSettings
from parakeet_stt_daemon.server import DaemonServer
from parakeet_stt_daemon.session import SessionManager


class FakeAudio:
    sample_rate = 16_000

    def __init__(self) -> None:
        self.abort_calls = 0
        self.start_calls = 0

    def start_session(self) -> None:
        self.start_calls += 1

    def abort_session(self) -> None:
        self.abort_calls += 1

    def take_stream_chunks(self) -> list[object]:
        return []


class FakeStreamingTranscriber:
    def __init__(
        self,
        *,
        helper_active: bool = True,
        fallback_reason: str | None = None,
        helper_class_name: str | None = None,
    ) -> None:
        self.helper_active = helper_active
        self.fallback_reason = fallback_reason
        self._helper_class_name = helper_class_name

    def start_session(self, _sample_rate: int) -> object:
        return object()


def _build_server(
    *,
    streaming_enabled: bool = True,
    streaming_transcriber: Any = None,
    vad_enabled: bool = False,
) -> DaemonServer:
    server = cast(Any, DaemonServer.__new__(DaemonServer))
    server.settings = ServerSettings(
        device="cpu",
        status_enabled=True,
        streaming_enabled=streaming_enabled,
        vad_enabled=vad_enabled,
    )
    server.sessions = SessionManager()
    server.audio = FakeAudio()
    server.model = object()
    server.transcriber = object()
    server._transcribe_lock = asyncio.Lock()
    server.streaming_transcriber = streaming_transcriber
    server._active_stream = None
    server._stream_drain_task = None
    server._stream_drain_running = False
    server._requested_device = "cpu"
    server._effective_device = "cpu"
    server._last_audio_ms = None
    server._last_audio_stop_ms = None
    server._last_finalize_ms = None
    server._last_infer_ms = None
    server._last_send_ms = None
    server._vad_model = None
    server._vad_import_error = None
    server._vad_enabled = vad_enabled
    return cast(DaemonServer, server)


def test_status_streaming_disabled_by_config() -> None:
    """When streaming is disabled by config, helper fields reflect that."""
    server = _build_server(streaming_enabled=False)

    status = server.status()

    assert status.streaming_enabled is False
    assert status.stream_helper_active is False
    assert status.stream_fallback_reason is None
    assert status.chunk_secs is None


def test_status_streaming_enabled_helper_active() -> None:
    """When streaming is enabled and helper initialized, truth is active."""
    transcriber = FakeStreamingTranscriber(
        helper_active=True,
        helper_class_name="FrameBatchChunkedRNNT",
    )
    server = _build_server(streaming_transcriber=transcriber)

    status = server.status()

    assert status.streaming_enabled is True
    assert status.stream_helper_active is True
    assert status.stream_fallback_reason is None


def test_status_streaming_enabled_helper_inactive() -> None:
    """When streaming is enabled but helper failed, truth shows fallback."""
    transcriber = FakeStreamingTranscriber(
        helper_active=False,
        fallback_reason="import_failed:ImportError",
    )
    server = _build_server(streaming_transcriber=transcriber)

    status = server.status()

    assert status.streaming_enabled is True
    assert status.stream_helper_active is False
    assert status.stream_fallback_reason == "import_failed:ImportError"


def test_status_streaming_enabled_transcriber_none() -> None:
    """When streaming_transcriber is None despite enabled config."""
    server = _build_server(streaming_transcriber=None)

    status = server.status()

    assert status.streaming_enabled is True
    assert status.stream_helper_active is False
    assert status.stream_fallback_reason == "streaming_transcriber_unavailable"


def test_stream_helper_active_reflects_transcriber_state() -> None:
    """_stream_helper_active() delegates to transcriber.helper_active."""
    transcriber = FakeStreamingTranscriber(helper_active=True)
    server = _build_server(streaming_transcriber=transcriber)
    assert server._stream_helper_active() is True

    transcriber.helper_active = False
    assert server._stream_helper_active() is False


def test_stream_fallback_reason_init_failed() -> None:
    """Fallback reason captures the init failure class."""
    transcriber = FakeStreamingTranscriber(
        helper_active=False,
        fallback_reason="init_failed:RuntimeError",
    )
    server = _build_server(streaming_transcriber=transcriber)

    assert server._stream_fallback_reason() == "init_failed:RuntimeError"


def test_stream_fallback_reason_none_when_active() -> None:
    """No fallback reason when helper is active."""
    transcriber = FakeStreamingTranscriber(helper_active=True)
    server = _build_server(streaming_transcriber=transcriber)

    assert server._stream_fallback_reason() is None


def test_status_includes_active_session_age_when_session_active() -> None:
    """active_session_age_ms is populated when a session exists."""

    async def scenario() -> None:
        server = _build_server(streaming_enabled=False)
        session_id = uuid4()
        await server.sessions.start_session(session_id)

        status = server.status()
        assert status.active_session_age_ms is not None
        assert status.active_session_age_ms >= 0

    asyncio.run(scenario())


def test_status_no_active_session_age_when_idle() -> None:
    """active_session_age_ms is None when no session is active."""
    server = _build_server(streaming_enabled=False)

    status = server.status()
    assert status.active_session_age_ms is None


def test_status_last_timings_none_before_first_session() -> None:
    """Timing fields are None before any session completes."""
    server = _build_server(streaming_enabled=False)

    status = server.status()
    assert status.audio_stop_ms is None
    assert status.finalize_ms is None
    assert status.infer_ms is None
    assert status.send_ms is None
    assert status.last_audio_ms is None
    assert status.last_infer_ms is None
    assert status.last_send_ms is None


def test_status_last_timings_populated_after_session() -> None:
    """Timing fields reflect last session values."""
    server = _build_server(streaming_enabled=False)
    server._last_audio_ms = 2500
    server._last_audio_stop_ms = 12
    server._last_finalize_ms = 180
    server._last_infer_ms = 120
    server._last_send_ms = 3

    status = server.status()
    assert status.audio_stop_ms == 12
    assert status.finalize_ms == 180
    assert status.infer_ms == 120
    assert status.send_ms == 3
    assert status.last_audio_ms == 2500
    assert status.last_infer_ms == 120
    assert status.last_send_ms == 3


def test_trim_tail_silence_default_path_uses_rms() -> None:
    import numpy as np

    server = _build_server(streaming_enabled=False, vad_enabled=False)

    def fake_rms(_samples: Any, _sample_rate: int, _window_ms: int = 50) -> Any:
        return "rms"

    server._trim_tail_with_rms = fake_rms  # type: ignore[method-assign]

    result = server._trim_tail_silence(np.zeros((16,), dtype=np.float32), sample_rate=16_000)
    assert result == "rms"


def test_trim_tail_silence_vad_opt_in_uses_vad_when_available() -> None:
    import numpy as np

    server = _build_server(streaming_enabled=False, vad_enabled=True)

    def fake_vad(_samples: Any, _sample_rate: int) -> Any:
        return "vad"

    def fake_rms(_samples: Any, _sample_rate: int, _window_ms: int = 50) -> Any:
        return "rms"

    server._trim_tail_with_vad = fake_vad  # type: ignore[method-assign]
    server._trim_tail_with_rms = fake_rms  # type: ignore[method-assign]

    result = server._trim_tail_silence(np.zeros((16,), dtype=np.float32), sample_rate=16_000)
    assert result == "vad"


def test_trim_tail_silence_vad_opt_in_falls_back_to_rms() -> None:
    import numpy as np

    server = _build_server(streaming_enabled=False, vad_enabled=True)

    def fake_vad(_samples: Any, _sample_rate: int) -> Any:
        return None

    def fake_rms(_samples: Any, _sample_rate: int, _window_ms: int = 50) -> Any:
        return "rms"

    server._trim_tail_with_vad = fake_vad  # type: ignore[method-assign]
    server._trim_tail_with_rms = fake_rms  # type: ignore[method-assign]

    result = server._trim_tail_silence(np.zeros((16,), dtype=np.float32), sample_rate=16_000)
    assert result == "rms"
