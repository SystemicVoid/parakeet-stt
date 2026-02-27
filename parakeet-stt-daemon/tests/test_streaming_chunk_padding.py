"""Regression tests for streaming session finalization behavior."""

from __future__ import annotations

from typing import Any, cast

import numpy as np


class _FakeParent:
    def __init__(self) -> None:
        self.offline_calls = 0
        self.last_samples: np.ndarray | None = None
        self.last_sample_rate: int | None = None

    def _transcribe_offline(self, samples: np.ndarray, sample_rate: int) -> str:
        self.offline_calls += 1
        self.last_samples = samples
        self.last_sample_rate = sample_rate
        return "offline text"


def test_finalize_returns_empty_when_no_audio() -> None:
    from parakeet_stt_daemon.model import ParakeetStreamingSession

    parent = _FakeParent()
    session = ParakeetStreamingSession(cast(Any, parent), sample_rate=16_000)

    result = session.finalize()

    assert result == ""
    assert parent.offline_calls == 0


def test_finalize_uses_offline_seal_even_with_retired_streaming_envs(monkeypatch) -> None:
    from parakeet_stt_daemon.model import ParakeetStreamingSession

    monkeypatch.setenv("PARAKEET_STREAM_THEN_SEAL", "0")
    monkeypatch.setenv("PARAKEET_STREAMING_TAIL_PAD_SECS", "0.6")
    monkeypatch.setenv("PARAKEET_STREAMING_DEBUG", "1")
    monkeypatch.setenv("PARAKEET_EXPERIMENTAL_CONFORMER_PARTIALS", "1")

    parent = _FakeParent()
    session = ParakeetStreamingSession(cast(Any, parent), sample_rate=16_000)
    session.feed(np.array([0.1, 0.2, 0.3], dtype=np.float32))

    result = session.finalize()

    assert result == "offline text"
    assert parent.offline_calls == 1
    assert parent.last_samples is not None
    np.testing.assert_allclose(parent.last_samples, np.array([0.1, 0.2, 0.3], dtype=np.float32))
    assert parent.last_sample_rate == 16_000


def test_finalize_concatenates_stream_chunks_before_offline_seal() -> None:
    from parakeet_stt_daemon.model import ParakeetStreamingSession

    parent = _FakeParent()
    session = ParakeetStreamingSession(cast(Any, parent), sample_rate=16_000)
    session.feed(np.array([0.1, 0.2], dtype=np.float32))
    session.feed(np.array([0.3, 0.4], dtype=np.float32))

    result = session.finalize()

    assert result == "offline text"
    assert parent.offline_calls == 1
    assert parent.last_samples is not None
    np.testing.assert_allclose(
        parent.last_samples, np.array([0.1, 0.2, 0.3, 0.4], dtype=np.float32)
    )
    assert parent.last_sample_rate == 16_000
