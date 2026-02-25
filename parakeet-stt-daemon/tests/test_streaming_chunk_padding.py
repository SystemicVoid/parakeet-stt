"""Regression tests for NeMo chunked iterator setup."""

from __future__ import annotations

from types import SimpleNamespace
from typing import Any, cast

import numpy as np

from parakeet_stt_daemon.model import ParakeetStreamingSession, _coerce_rnnt_texts


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

    def set_frame_reader(self, frame_reader: _FakeIterator) -> None:
        self.frame_reader = frame_reader

    def transcribe(self) -> str:
        return "tail text"


class _FakeParent:
    def __init__(self) -> None:
        self.chunk_helper = _FakeChunkHelper()
        self._audio_feature_iter_cls = _FakeIterator
        self._helper_tokens_per_chunk = None
        self._helper_delay = None
        self.offline_calls = 0

    def _transcribe_offline(self, _samples: np.ndarray, _sample_rate: int) -> str:
        self.offline_calls += 1
        return "offline text"


def test_finalize_disables_padding_for_chunked_iterator() -> None:
    parent = _FakeParent()
    session = ParakeetStreamingSession(cast(Any, parent), sample_rate=16_000)
    session.feed(np.array([0.1, 0.2, 0.3], dtype=np.float32))

    result = session.finalize()

    assert result == "tail text"
    assert parent.offline_calls == 0
    assert parent.chunk_helper.frame_reader is not None
    assert parent.chunk_helper.frame_reader.pad_to_frame_len is False


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
