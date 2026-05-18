"""Regression tests for offline in-memory transcription path."""

from __future__ import annotations

import asyncio
import time
from collections.abc import Callable
from pathlib import Path
from types import SimpleNamespace
from typing import Any, cast

import numpy as np

from parakeet_stt_daemon import session_orchestrator as orchestrator_module
from parakeet_stt_daemon.config import ServerSettings
from parakeet_stt_daemon.model import ParakeetTranscriber
from parakeet_stt_daemon.session_orchestrator import SessionOrchestrator
from parakeet_stt_daemon.tail_trim import SealPathTailTrimmer, TailTrimOutcome


class _ArrayModel:
    def __init__(self) -> None:
        self.calls: list[object] = []

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


class _ExplodingStreamSession:
    def __init__(self) -> None:
        self.feed_calls: list[np.ndarray] = []
        self.finalize_called = False

    def feed(self, chunk: np.ndarray) -> None:
        self.feed_calls.append(chunk.copy())

    def finalize(self) -> str:
        self.finalize_called = True
        raise AssertionError("final transcript should not read from streaming mirror")


def _tail_trimmer_with_trim(
    trim: Callable[[np.ndarray, int], TailTrimOutcome],
) -> SealPathTailTrimmer:
    tail_trimmer = SealPathTailTrimmer(vad_enabled=False, silence_floor_db=-40.0)
    cast(Any, tail_trimmer).trim = trim
    return tail_trimmer


def _identity_tail_trim(samples: np.ndarray, _sample_rate: int) -> TailTrimOutcome:
    return TailTrimOutcome(samples, "rms", False, None)


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


def test_server_offline_finalize_uses_in_memory_transcriber() -> None:
    async def scenario() -> None:
        server = cast(Any, SessionOrchestrator.__new__(SessionOrchestrator))
        server.settings = ServerSettings(device="cpu", streaming_enabled=False)
        server.audio = SimpleNamespace(sample_rate=16_000)
        server.transcriber = _RecordingTranscriber()
        server.streaming_transcriber = None
        server._active_stream = None

        samples = np.array([0.2, 0.1, 0.05], dtype=np.float32)
        typed_server = cast(SessionOrchestrator, server)
        text, infer_ms = await typed_server._finalise_transcription(samples)

        assert text == "offline text"
        assert infer_ms >= 0
        recording = server.transcriber
        assert len(recording.calls) == 1
        forwarded_samples, forwarded_rate = recording.calls[0]
        assert forwarded_rate == 16_000
        assert forwarded_samples.size > 0

    asyncio.run(scenario())


def test_server_offline_finalize_skips_model_call_when_trimmed_audio_empty(monkeypatch) -> None:
    async def scenario() -> None:
        server = cast(Any, SessionOrchestrator.__new__(SessionOrchestrator))
        server.settings = ServerSettings(device="cuda", streaming_enabled=False)
        server.audio = SimpleNamespace(sample_rate=16_000)
        server.transcriber = _RecordingTranscriber()
        server.streaming_transcriber = None
        server._active_stream = None
        server._vad_enabled = False
        server._effective_device = "cuda"

        def trim_empty(_samples: np.ndarray, _sample_rate: int) -> TailTrimOutcome:
            return TailTrimOutcome(
                np.zeros((0,), dtype=np.float32),
                "rms",
                False,
                None,
            )

        server.tail_trimmer = _tail_trimmer_with_trim(trim_empty)
        released_devices: list[str] = []

        monkeypatch.setattr(
            orchestrator_module,
            "_release_cuda_cache",
            lambda device: released_devices.append(device),
        )

        samples = np.array([0.2, 0.1, 0.05], dtype=np.float32)
        typed_server = cast(SessionOrchestrator, server)
        text, infer_ms = await typed_server._finalise_transcription(samples)

        assert text == ""
        assert infer_ms == 0
        assert server.transcriber.calls == []
        assert released_devices == ["cuda"]

    asyncio.run(scenario())


def test_server_finalize_uses_canonical_audio_even_when_stream_session_exists() -> None:
    async def scenario() -> None:
        server = cast(Any, SessionOrchestrator.__new__(SessionOrchestrator))
        server.settings = ServerSettings(device="cpu", streaming_enabled=True)
        server.audio = SimpleNamespace(sample_rate=16_000)
        server.transcriber = _RecordingTranscriber()
        server.streaming_transcriber = object()
        server._active_stream = _ExplodingStreamSession()
        server.tail_trimmer = _tail_trimmer_with_trim(_identity_tail_trim)

        samples = np.array([0.2, 0.1, 0.05, 0.4], dtype=np.float32)
        typed_server = cast(SessionOrchestrator, server)
        text, infer_ms = await typed_server._finalise_transcription(samples)

        assert text == "offline text"
        assert infer_ms >= 0
        recording = server.transcriber
        assert len(recording.calls) == 1
        forwarded_samples, forwarded_rate = recording.calls[0]
        assert forwarded_rate == 16_000
        np.testing.assert_array_equal(forwarded_samples, samples)
        stream = server._active_stream
        assert stream.feed_calls == []
        assert stream.finalize_called is False

    asyncio.run(scenario())


def test_server_finalize_offloads_tail_trim_off_event_loop() -> None:
    async def scenario() -> None:
        server = cast(Any, SessionOrchestrator.__new__(SessionOrchestrator))
        server.settings = ServerSettings(device="cpu", streaming_enabled=False)
        server.audio = SimpleNamespace(sample_rate=16_000)
        server.transcriber = _RecordingTranscriber()
        server.streaming_transcriber = None
        server._active_stream = None

        def slow_trim(samples: np.ndarray, _sample_rate: int) -> TailTrimOutcome:
            time.sleep(0.12)
            return TailTrimOutcome(samples, "rms", False, None)

        server.tail_trimmer = _tail_trimmer_with_trim(slow_trim)

        progress = asyncio.Event()

        async def ticker() -> None:
            await asyncio.sleep(0.02)
            progress.set()

        finalize_task = asyncio.create_task(
            cast(SessionOrchestrator, server)._finalise_transcription(
                np.array([0.2, 0.1, 0.05], dtype=np.float32)
            )
        )
        ticker_task = asyncio.create_task(ticker())

        await asyncio.wait_for(progress.wait(), timeout=0.05)
        text, infer_ms = await finalize_task
        await ticker_task

        assert text == "offline text"
        assert infer_ms >= 0

    asyncio.run(scenario())


def test_server_finalize_releases_cuda_cache_after_model_call(monkeypatch) -> None:
    async def scenario() -> None:
        server = cast(Any, SessionOrchestrator.__new__(SessionOrchestrator))
        server.settings = ServerSettings(device="cuda", streaming_enabled=False)
        server.audio = SimpleNamespace(sample_rate=16_000)
        server.transcriber = _RecordingTranscriber()
        server.streaming_transcriber = None
        server._active_stream = None
        server._effective_device = "cuda"
        server.tail_trimmer = _tail_trimmer_with_trim(_identity_tail_trim)
        released_devices: list[str] = []

        monkeypatch.setattr(
            orchestrator_module,
            "_release_cuda_cache",
            lambda device: released_devices.append(device),
        )

        text, infer_ms = await cast(SessionOrchestrator, server)._finalise_transcription(
            np.array([0.2, 0.1, 0.05], dtype=np.float32)
        )

        assert text == "offline text"
        assert infer_ms >= 0
        assert released_devices == ["cuda"]

    asyncio.run(scenario())
