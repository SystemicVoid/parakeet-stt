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

    transcriber = ParakeetStreamingTranscriber(cast(Any, model), batch_size=32)

    assert transcriber.helper_active is True
    assert transcriber._helper_class_name == "BatchedFrameASRTDT"
    assert model.change_decoding_flags == [(False, False)]
    assert rebuilt_decoder.disable_calls == 1


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
