"""Regression tests for offline in-memory transcription path."""

from __future__ import annotations

import asyncio
import time
from collections.abc import Callable
from pathlib import Path
from types import SimpleNamespace
from typing import Any, cast

import numpy as np
import pytest

from parakeet_stt_daemon.model import ParakeetTranscriber
from parakeet_stt_daemon.session import (
    SealPathFinalizationFailure,
    SealPathFinalizationResult,
    SealPathRuntime,
)
from parakeet_stt_daemon.tail_trim import SealPathTailTrimmer, TailTrimOutcome


class _ArrayModel:
    def __init__(self, sample_rate: int = 16_000) -> None:
        self.calls: list[object] = []
        self.cfg = SimpleNamespace(sample_rate=sample_rate)

    def transcribe(self, audio, **_kwargs):  # noqa: ANN001, ANN003
        self.calls.append(audio)
        if isinstance(audio, list) and audio and isinstance(audio[0], np.ndarray):
            return ["in memory text"]
        raise AssertionError("unexpected input type")


class _FallbackModel:
    def __init__(self) -> None:
        self.calls: list[object] = []

    def transcribe(self, audio, **_kwargs):  # noqa: ANN001, ANN003
        self.calls.append(audio)
        if isinstance(audio, list) and audio and isinstance(audio[0], np.ndarray):
            raise RuntimeError("array decode unsupported")
        return ["file fallback text"]


class _RecordingTranscriber:
    def __init__(self) -> None:
        self.calls: list[tuple[np.ndarray, int]] = []

    def transcribe_samples(self, samples: np.ndarray, *, sample_rate: int = 16_000) -> str:
        self.calls.append((samples.copy(), sample_rate))
        return "offline text"


def _tail_trimmer_with_trim(
    trim: Callable[[np.ndarray, int], TailTrimOutcome],
) -> SealPathTailTrimmer:
    tail_trimmer = SealPathTailTrimmer(vad_enabled=False, silence_floor_db=-40.0)
    cast(Any, tail_trimmer).trim = trim
    return tail_trimmer


def _identity_tail_trim(samples: np.ndarray, _sample_rate: int) -> TailTrimOutcome:
    return TailTrimOutcome(samples, "rms", False, None)


def _runtime(
    trim: Callable[[np.ndarray, int], TailTrimOutcome] = _identity_tail_trim,
    *,
    released_devices: list[str] | None = None,
) -> SealPathRuntime:
    return SealPathRuntime(
        sample_rate=16_000,
        tail_trimmer=_tail_trimmer_with_trim(trim),
        release_device_cache=(
            released_devices.append if released_devices is not None else lambda _device: None
        ),
    )


def _async_transcriber(transcriber: _RecordingTranscriber):
    async def transcribe(samples: np.ndarray) -> str:
        return transcriber.transcribe_samples(samples, sample_rate=16_000)

    return transcribe


def test_transcribe_samples_uses_array_path_when_supported() -> None:
    model = _ArrayModel()
    transcriber = ParakeetTranscriber(model=cast(Any, model))
    samples = np.array([0.1, -0.2, 0.3], dtype=np.float32)

    result = transcriber.transcribe_samples(samples, sample_rate=16_000)

    assert result == "in memory text"
    assert len(model.calls) == 1
    first_call = model.calls[0]
    assert isinstance(first_call, list)
    assert isinstance(first_call[0], np.ndarray)


def test_transcribe_samples_rejects_sample_rate_mismatch_without_model_call() -> None:
    model = _ArrayModel()
    transcriber = ParakeetTranscriber(model=cast(Any, model))
    samples = np.array([0.1, -0.2, 0.3], dtype=np.float32)

    with pytest.raises(ValueError, match=r"sample rate.*8000.*16000"):
        transcriber.transcribe_samples(samples, sample_rate=8_000)

    assert model.calls == []


def test_transcribe_samples_accepts_configured_model_sample_rate() -> None:
    model = _ArrayModel(sample_rate=8_000)
    transcriber = ParakeetTranscriber(model=cast(Any, model))
    samples = np.array([0.1, -0.2, 0.3], dtype=np.float32)

    result = transcriber.transcribe_samples(samples, sample_rate=8_000)

    assert result == "in memory text"
    assert len(model.calls) == 1


def test_transcribe_samples_falls_back_to_file_when_array_path_fails(monkeypatch) -> None:
    model = _FallbackModel()
    transcriber = ParakeetTranscriber(model=cast(Any, model))
    samples = np.array([0.1, 0.2, 0.3], dtype=np.float32)
    fallback_writer_calls: list[tuple[Path, int]] = []

    def _fake_write_audio_file(path: Path, _samples: np.ndarray, sample_rate: int) -> None:
        fallback_writer_calls.append((path, sample_rate))

    monkeypatch.setattr("parakeet_stt_daemon.model._write_audio_file", _fake_write_audio_file)
    result = transcriber.transcribe_samples(samples, sample_rate=16_000)

    assert result == "file fallback text"
    assert len(model.calls) == 2
    assert len(fallback_writer_calls) == 1
    assert fallback_writer_calls[0][1] == 16_000


def test_transcribe_samples_empty_audio_returns_empty_string_without_model_call() -> None:
    model = _ArrayModel()
    transcriber = ParakeetTranscriber(model=cast(Any, model))

    result = transcriber.transcribe_samples(np.zeros((0,), dtype=np.float32), sample_rate=16_000)

    assert result == ""
    assert model.calls == []


def test_seal_path_finalize_uses_in_memory_transcriber_and_records_timing() -> None:
    async def scenario() -> None:
        runtime = _runtime()
        transcriber = _RecordingTranscriber()
        samples = np.array([0.2, 0.1, 0.05], dtype=np.float32)
        outcome = await runtime.finalize(
            samples,
            _async_transcriber(transcriber),
            effective_device="cpu",
        )

        assert isinstance(outcome, SealPathFinalizationResult)
        assert outcome.text == "offline text"
        assert outcome.infer_ms >= 0
        assert outcome.finalize_ms >= outcome.infer_ms
        runtime.record_success(outcome, audio_stop_ms=9, send_ms=4)
        metrics = runtime.metrics()
        assert metrics.audio_stop_ms == 9
        assert metrics.finalize_ms == outcome.finalize_ms
        assert metrics.infer_ms == outcome.infer_ms
        assert metrics.send_ms == 4
        assert metrics.last_audio_ms == outcome.audio_ms
        assert metrics.last_infer_ms == outcome.infer_ms
        assert metrics.last_send_ms == 4
        assert len(transcriber.calls) == 1
        forwarded_samples, forwarded_rate = transcriber.calls[0]
        assert forwarded_rate == 16_000
        assert forwarded_samples.size > 0

    asyncio.run(scenario())


def test_seal_path_finalize_skips_model_call_when_trimmed_audio_empty() -> None:
    async def scenario() -> None:
        def trim_empty(_samples: np.ndarray, _sample_rate: int) -> TailTrimOutcome:
            return TailTrimOutcome(
                np.zeros((0,), dtype=np.float32),
                "rms",
                False,
                None,
            )

        released_devices: list[str] = []
        runtime = _runtime(trim_empty, released_devices=released_devices)
        transcriber = _RecordingTranscriber()
        samples = np.array([0.2, 0.1, 0.05], dtype=np.float32)
        outcome = await runtime.finalize(
            samples,
            _async_transcriber(transcriber),
            effective_device="cuda",
        )

        assert isinstance(outcome, SealPathFinalizationResult)
        assert outcome.text == ""
        assert outcome.infer_ms == 0
        assert transcriber.calls == []
        assert released_devices == ["cuda"]

    asyncio.run(scenario())


def test_seal_path_finalize_forwards_canonical_audio_samples() -> None:
    async def scenario() -> None:
        runtime = _runtime()
        transcriber = _RecordingTranscriber()
        samples = np.array([0.2, 0.1, 0.05, 0.4], dtype=np.float32)
        outcome = await runtime.finalize(
            samples,
            _async_transcriber(transcriber),
            effective_device="cpu",
        )

        assert isinstance(outcome, SealPathFinalizationResult)
        assert outcome.text == "offline text"
        assert outcome.infer_ms >= 0
        assert len(transcriber.calls) == 1
        forwarded_samples, forwarded_rate = transcriber.calls[0]
        assert forwarded_rate == 16_000
        np.testing.assert_array_equal(forwarded_samples, samples)

    asyncio.run(scenario())


def test_seal_path_finalize_offloads_tail_trim_off_event_loop() -> None:
    async def scenario() -> None:
        def slow_trim(samples: np.ndarray, _sample_rate: int) -> TailTrimOutcome:
            time.sleep(0.12)
            return TailTrimOutcome(samples, "rms", False, None)

        runtime = _runtime(slow_trim)
        transcriber = _RecordingTranscriber()
        progress = asyncio.Event()

        async def ticker() -> None:
            await asyncio.sleep(0.02)
            progress.set()

        finalize_task = asyncio.create_task(
            runtime.finalize(
                np.array([0.2, 0.1, 0.05], dtype=np.float32),
                _async_transcriber(transcriber),
                effective_device="cpu",
            )
        )
        ticker_task = asyncio.create_task(ticker())

        await asyncio.wait_for(progress.wait(), timeout=0.05)
        outcome = await finalize_task
        await ticker_task

        assert isinstance(outcome, SealPathFinalizationResult)
        assert outcome.text == "offline text"
        assert outcome.infer_ms >= 0

    asyncio.run(scenario())


def test_seal_path_finalize_releases_cuda_cache_after_model_call() -> None:
    async def scenario() -> None:
        released_devices: list[str] = []
        runtime = _runtime(released_devices=released_devices)
        transcriber = _RecordingTranscriber()

        outcome = await runtime.finalize(
            np.array([0.2, 0.1, 0.05], dtype=np.float32),
            _async_transcriber(transcriber),
            effective_device="cuda",
        )

        assert isinstance(outcome, SealPathFinalizationResult)
        assert outcome.text == "offline text"
        assert outcome.infer_ms >= 0
        assert released_devices == ["cuda"]

    asyncio.run(scenario())


def test_seal_path_finalize_releases_cuda_cache_when_tail_trim_fails() -> None:
    async def scenario() -> None:
        def fail_trim(_samples: np.ndarray, _sample_rate: int) -> TailTrimOutcome:
            raise RuntimeError("trim failed")

        released_devices: list[str] = []
        runtime = _runtime(fail_trim, released_devices=released_devices)
        transcriber = _RecordingTranscriber()

        outcome = await runtime.finalize(
            np.array([0.2, 0.1, 0.05], dtype=np.float32),
            _async_transcriber(transcriber),
            effective_device="cuda",
        )

        assert isinstance(outcome, SealPathFinalizationFailure)
        assert outcome.code == "MODEL"
        assert transcriber.calls == []
        assert released_devices == ["cuda"]

    asyncio.run(scenario())


def test_seal_path_finalize_preserves_result_when_cache_release_fails() -> None:
    async def scenario() -> None:
        def fail_release(_device: str) -> None:
            raise RuntimeError("cache release failed")

        runtime = SealPathRuntime(
            sample_rate=16_000,
            tail_trimmer=_tail_trimmer_with_trim(_identity_tail_trim),
            release_device_cache=fail_release,
        )
        transcriber = _RecordingTranscriber()

        outcome = await runtime.finalize(
            np.array([0.2, 0.1, 0.05], dtype=np.float32),
            _async_transcriber(transcriber),
            effective_device="cuda",
        )

        assert isinstance(outcome, SealPathFinalizationResult)
        assert outcome.text == "offline text"
        assert runtime.last_failure is None

    asyncio.run(scenario())
