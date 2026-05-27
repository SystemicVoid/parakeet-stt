"""Regression tests for streaming session finalization behavior."""

from __future__ import annotations

import sys
from types import ModuleType, SimpleNamespace
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


class _FakeCudaGraphDecoder:
    def __init__(self) -> None:
        self.disable_calls = 0

    def disable_cuda_graphs(self) -> bool:
        self.disable_calls += 1
        return True


class _FakeTDTLoss:
    pass


class _FailingStreamParent(_FakeParent):
    def __init__(self) -> None:
        super().__init__()
        self.marked_fallback_reason: str | None = None

    def process_stream_chunk(self, _samples: np.ndarray, _sample_rate: int) -> object:
        raise RuntimeError("stream helper failed")

    def mark_stream_fallback(self, reason: str) -> None:
        self.marked_fallback_reason = reason


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


def test_streaming_transcriber_can_disable_helper_without_importing_nemo() -> None:
    from parakeet_stt_daemon.model import ParakeetStreamingTranscriber

    transcriber = ParakeetStreamingTranscriber(
        cast(Any, object()),
        enable_helper=False,
        helper_disabled_reason="streaming_helper_disabled:test",
    )

    session = transcriber.start_session(16_000)
    processed = session.feed(np.array([0.1, 0.2], dtype=np.float32))

    assert transcriber.helper_active is False
    assert transcriber.fallback_reason == "streaming_helper_disabled:test"
    assert transcriber._helper_class_name is None
    assert processed is False
    assert session.stream_path_executed is False
    assert session.stream_chunks_processed == 0
    assert session.stream_fallback_reason == "streaming_helper_disabled:test"


def test_cuda_graph_decoder_config_is_disabled_before_decoder_rebuild() -> None:
    from parakeet_stt_daemon.model import _disable_cuda_graph_decoder_config

    decoding_cfg = {
        "greedy": {"use_cuda_graph_decoder": True},
        "beam": {"allow_cuda_graphs": True},
    }

    changed = _disable_cuda_graph_decoder_config(decoding_cfg)

    assert changed is True
    assert decoding_cfg["greedy"]["use_cuda_graph_decoder"] is False
    assert decoding_cfg["beam"]["allow_cuda_graphs"] is False


def test_cuda_graph_decoder_disable_reaches_nested_nemo_decoder() -> None:
    from parakeet_stt_daemon.model import _disable_cuda_graph_decoder

    decoder = _FakeCudaGraphDecoder()
    model = SimpleNamespace(
        _cfg=SimpleNamespace(
            decoding={
                "greedy": {"use_cuda_graph_decoder": True},
                "beam": {"allow_cuda_graphs": True},
            }
        ),
        decoding=SimpleNamespace(decoding=decoder),
    )

    _disable_cuda_graph_decoder(cast(Any, model))

    assert decoder.disable_calls == 1
    assert model._cfg.decoding["greedy"]["use_cuda_graph_decoder"] is False
    assert model._cfg.decoding["beam"]["allow_cuda_graphs"] is False


def test_cuda_graph_decoder_disable_reaches_decoding_computer() -> None:
    from parakeet_stt_daemon.model import _disable_cuda_graph_decoder

    decoder = _FakeCudaGraphDecoder()
    model = SimpleNamespace(
        _cfg=SimpleNamespace(decoding={}),
        decoding=SimpleNamespace(decoding=SimpleNamespace(decoding_computer=decoder)),
    )

    _disable_cuda_graph_decoder(cast(Any, model))

    assert decoder.disable_calls == 1


def test_tdt_streaming_helper_rebuilds_decoder_with_cuda_graphs_disabled(monkeypatch) -> None:
    from parakeet_stt_daemon.model import ParakeetStreamingTranscriber

    streaming_utils = ModuleType("nemo.collections.asr.parts.utils.streaming_utils")

    class _FakeFrameBatchChunkedRNNT:
        def __init__(self, **_kwargs: Any) -> None:
            raise AssertionError("TDT model should use BatchedFrameASRTDT")

    class _FakeBatchedFrameASRTDT:
        def __init__(self, **kwargs: Any) -> None:
            self.kwargs = kwargs

    streaming_utils_any = cast(Any, streaming_utils)
    streaming_utils_any.FrameBatchChunkedRNNT = _FakeFrameBatchChunkedRNNT
    streaming_utils_any.BatchedFrameASRTDT = _FakeBatchedFrameASRTDT
    for module_name in (
        "nemo",
        "nemo.collections",
        "nemo.collections.asr",
        "nemo.collections.asr.parts",
        "nemo.collections.asr.parts.utils",
    ):
        monkeypatch.setitem(sys.modules, module_name, ModuleType(module_name))
    monkeypatch.setitem(
        sys.modules,
        "nemo.collections.asr.parts.utils.streaming_utils",
        streaming_utils,
    )
    monkeypatch.setitem(sys.modules, "omegaconf", None)

    rebuilt_decoder = _FakeCudaGraphDecoder()

    class _FakeTDTModel:
        def __init__(self) -> None:
            self._cfg = SimpleNamespace(
                decoding={
                    "greedy": {"use_cuda_graph_decoder": True},
                    "beam": {"allow_cuda_graphs": True},
                }
            )
            self.loss = SimpleNamespace(_loss=_FakeTDTLoss())
            self.decoding = SimpleNamespace(decoding=SimpleNamespace())
            self.change_decoding_flags: list[tuple[bool, bool]] = []

        def change_decoding_strategy(self, decoding_cfg: dict[str, Any]) -> None:
            self.change_decoding_flags.append(
                (
                    decoding_cfg["greedy"]["use_cuda_graph_decoder"],
                    decoding_cfg["beam"]["allow_cuda_graphs"],
                )
            )
            self.decoding = SimpleNamespace(decoding=rebuilt_decoder)

    model = _FakeTDTModel()
    original_decoding = model.decoding

    transcriber = ParakeetStreamingTranscriber(
        cast(Any, model),
        batch_size=32,
        enable_helper=True,
    )

    assert transcriber.helper_active is True
    assert transcriber._helper_class_name == "BatchedFrameASRTDT"
    assert model.change_decoding_flags == [(False, False)]
    assert model.decoding is not original_decoding
    assert rebuilt_decoder.disable_calls == 1


def test_tdt_stream_chunk_pads_terminal_frame_and_records_execution(monkeypatch) -> None:
    from parakeet_stt_daemon.model import ParakeetStreamingTranscriber

    streaming_utils = ModuleType("nemo.collections.asr.parts.utils.streaming_utils")
    captured: dict[str, Any] = {}

    class _FakeAudioFeatureIterator:
        def __init__(
            self,
            samples: np.ndarray,
            frame_len: float,
            preprocessor: object,
            device: str,
            *,
            pad_to_frame_len: bool = True,
        ) -> None:
            self.samples = np.asarray(samples)
            self.frame_len = frame_len
            self.preprocessor = preprocessor
            self.device = device
            self.pad_to_frame_len = pad_to_frame_len
            captured["frame_reader"] = self

    class _FakeFrameBatchChunkedRNNT:
        def __init__(self, **_kwargs: Any) -> None:
            raise AssertionError("TDT model should use BatchedFrameASRTDT")

    class _FakeBatchedFrameASRTDT:
        def __init__(self, **_kwargs: Any) -> None:
            self.raw_preprocessor = SimpleNamespace(_cfg={"window_stride": 0.01})
            self.reader: _FakeAudioFeatureIterator | None = None
            self.reset_calls = 0

        def reset(self) -> None:
            self.reset_calls += 1

        def set_frame_reader(self, frame_reader: _FakeAudioFeatureIterator, idx: int) -> None:
            self.reader = frame_reader
            captured["set_frame_reader_idx"] = idx

        def transcribe(self, tokens_per_chunk: int, delay: int) -> list[str]:
            captured["tokens_per_chunk"] = tokens_per_chunk
            captured["delay"] = delay
            if self.reader is None:
                raise AssertionError("missing frame reader")
            if not self.reader.pad_to_frame_len:
                raise ValueError("short terminal frame broadcast failure")
            if self.reader.samples.shape[0] <= 1_600:
                raise ValueError("missing TDT delay pad")
            return ["partial text"]

    streaming_utils_any = cast(Any, streaming_utils)
    streaming_utils_any.AudioFeatureIterator = _FakeAudioFeatureIterator
    streaming_utils_any.FrameBatchChunkedRNNT = _FakeFrameBatchChunkedRNNT
    streaming_utils_any.BatchedFrameASRTDT = _FakeBatchedFrameASRTDT
    for module_name in (
        "nemo",
        "nemo.collections",
        "nemo.collections.asr",
        "nemo.collections.asr.parts",
        "nemo.collections.asr.parts.utils",
    ):
        monkeypatch.setitem(sys.modules, module_name, ModuleType(module_name))
    monkeypatch.setitem(
        sys.modules,
        "nemo.collections.asr.parts.utils.streaming_utils",
        streaming_utils,
    )
    monkeypatch.setitem(sys.modules, "omegaconf", None)

    model = SimpleNamespace(
        _cfg=SimpleNamespace(
            sample_rate=16_001,
            preprocessor=SimpleNamespace(window_stride=0.01),
            decoding={"greedy": {"max_symbols_per_step": 5}},
        ),
        device="cpu",
        encoder=SimpleNamespace(subsampling_factor=8),
        loss=SimpleNamespace(_loss=_FakeTDTLoss()),
        decoding=SimpleNamespace(decoding=SimpleNamespace()),
    )
    model.change_decoding_strategy = lambda _decoding_cfg: setattr(
        model,
        "decoding",
        SimpleNamespace(decoding=SimpleNamespace()),
    )

    transcriber = ParakeetStreamingTranscriber(
        cast(Any, model),
        chunk_secs=0.2,
        right_context_secs=0.1,
        batch_size=32,
        enable_helper=True,
    )
    session = transcriber.start_session(16_001)

    processed = session.feed(np.ones((1_600,), dtype=np.float32))

    frame_reader = captured["frame_reader"]
    assert processed is True
    assert session.stream_path_executed is True
    assert session.stream_chunks_processed == 1
    assert session.stream_fallback_reason is None
    assert captured["set_frame_reader_idx"] == 0
    assert captured["tokens_per_chunk"] == 3
    assert captured["delay"] == 4
    assert frame_reader.pad_to_frame_len is True
    assert frame_reader.samples.dtype == np.float32
    assert frame_reader.samples.shape == (6_721,)


def test_streaming_session_falls_back_to_seal_path_after_helper_failure() -> None:
    from parakeet_stt_daemon.model import ParakeetStreamingSession

    parent = _FailingStreamParent()
    session = ParakeetStreamingSession(cast(Any, parent), sample_rate=16_000)

    processed = session.feed(np.array([0.1, 0.2], dtype=np.float32))
    result = session.finalize()

    assert processed is False
    assert session.stream_path_executed is False
    assert session.stream_chunks_processed == 0
    assert session.stream_fallback_reason == "stream_chunk_failed:RuntimeError"
    assert parent.marked_fallback_reason == "stream_chunk_failed:RuntimeError"
    assert result == "offline text"
    assert parent.offline_calls == 1
    assert parent.last_samples is not None
    np.testing.assert_allclose(parent.last_samples, np.array([0.1, 0.2], dtype=np.float32))


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
