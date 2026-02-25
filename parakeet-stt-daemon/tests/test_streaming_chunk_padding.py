"""Regression tests for NeMo chunked iterator setup."""

from __future__ import annotations

from types import SimpleNamespace
from typing import Any, cast

import numpy as np
from parakeet_stt_daemon.model import (
    ParakeetStreamingSession,
    _coerce_rnnt_texts,
    _compute_eou_drain_samples,
)


class _FakeIterator:
    def __init__(
        self,
        samples: np.ndarray,
        frame_len: float,
        raw_preprocessor: object,
        device: str,
        *,
        pad_to_frame_len: bool = True,
    ) -> None:
        self.samples = samples
        self.frame_len = frame_len
        self.raw_preprocessor = raw_preprocessor
        self.device = device
        self.pad_to_frame_len = pad_to_frame_len


class _FakeChunkHelper:
    def __init__(self) -> None:
        self.frame_len = 2.0
        self.raw_preprocessor = object()
        self.asr_model = SimpleNamespace(device="cpu")
        self.frame_reader: _FakeIterator | None = None
        self.call_count = 0
        self.calls: list[tuple[int, int]] = []

    def set_frame_reader(self, frame_reader: _FakeIterator) -> None:
        self.frame_reader = frame_reader

    def transcribe(self, *args: int) -> str:
        self.call_count += 1
        if len(args) == 2:
            self.calls.append((args[0], args[1]))
        return "tail text" if self.call_count == 1 else "drained text"


class _FakeParent:
    def __init__(self) -> None:
        self.chunk_helper = _FakeChunkHelper()
        self._audio_feature_iter_cls = _FakeIterator
        self._helper_tokens_per_chunk: int | None = None
        self._helper_delay: int | None = None
        self._helper_model_stride_secs: float | None = None
        self.model = SimpleNamespace(
            _cfg=SimpleNamespace(preprocessor=SimpleNamespace(window_stride=0.01)),
            encoder=SimpleNamespace(streaming_cfg=SimpleNamespace(shift_size=16)),
        )
        self.offline_calls = 0

    def _transcribe_offline(self, _samples: np.ndarray, _sample_rate: int) -> str:
        self.offline_calls += 1
        return "offline text"


def test_finalize_disables_padding_for_chunked_iterator(monkeypatch) -> None:
    monkeypatch.setenv("PARAKEET_STREAM_THEN_SEAL", "0")
    parent = _FakeParent()
    session = ParakeetStreamingSession(cast(Any, parent), sample_rate=16_000)
    session.feed(np.array([0.1, 0.2, 0.3], dtype=np.float32))

    result = session.finalize()

    assert result == "tail text"
    assert parent.offline_calls == 0
    assert parent.chunk_helper.frame_reader is not None
    assert parent.chunk_helper.frame_reader.pad_to_frame_len is False


def test_finalize_runs_explicit_drain_pass_for_tdt_helper(monkeypatch) -> None:
    # Keep seal disabled to assert helper drain behavior directly.
    monkeypatch.setenv("PARAKEET_STREAM_THEN_SEAL", "0")
    parent = _FakeParent()
    parent._helper_tokens_per_chunk = 10
    parent._helper_delay = 3
    parent._helper_model_stride_secs = 0.04
    session = ParakeetStreamingSession(cast(Any, parent), sample_rate=16_000)
    session.feed(np.array([0.1, 0.2, 0.3], dtype=np.float32))

    result = session.finalize()

    assert result == "drained text"
    assert parent.offline_calls == 0
    assert parent.chunk_helper.call_count == 2
    assert parent.chunk_helper.calls == [(10, 3), (10, 3)]
    assert parent.chunk_helper.frame_reader is not None
    # Second pass should be drain-only silence from config-derived frame count.
    assert parent.chunk_helper.frame_reader.samples.size == 5120
    assert parent.chunk_helper.frame_reader.pad_to_frame_len is True


def test_finalize_uses_offline_seal_by_default() -> None:
    parent = _FakeParent()
    parent._helper_tokens_per_chunk = 10
    parent._helper_delay = 3
    parent._helper_model_stride_secs = 0.04
    session = ParakeetStreamingSession(cast(Any, parent), sample_rate=16_000)
    session.feed(np.array([0.1, 0.2, 0.3], dtype=np.float32))

    result = session.finalize()

    assert result == "offline text"
    assert parent.offline_calls == 1
    assert parent.chunk_helper.call_count == 0


def test_compute_eou_drain_samples_falls_back_to_delay_stride() -> None:
    model = SimpleNamespace(
        _cfg=SimpleNamespace(preprocessor=SimpleNamespace(window_stride=0.01)),
        encoder=SimpleNamespace(streaming_cfg=SimpleNamespace()),
    )

    samples = _compute_eou_drain_samples(
        cast(Any, model),
        sample_rate=16_000,
        delay=4,
        model_stride_secs=0.04,
    )

    assert samples == 2560


def test_coerce_rnnt_texts_handles_hypothesis_list() -> None:
    result = _coerce_rnnt_texts([SimpleNamespace(text="hello"), SimpleNamespace(text="world")])

    assert result == ["hello", "world"]


def test_coerce_rnnt_texts_handles_legacy_tuple() -> None:
    result = _coerce_rnnt_texts((["best"], ["alt"]))

    assert result == ["best"]


def test_coerce_rnnt_texts_handles_nbest_lists() -> None:
    result = _coerce_rnnt_texts(
        [
            [SimpleNamespace(text="best"), SimpleNamespace(text="alt")],
            [SimpleNamespace(text="next")],
        ]
    )

    assert result == ["best", "next"]
