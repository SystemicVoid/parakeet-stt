"""Pure Pre-roll buffer and Session audio buffer invariants."""

from __future__ import annotations

from typing import Any, cast

import numpy as np
import pytest

from parakeet_stt_daemon.audio import AudioInput
from parakeet_stt_daemon.audio_buffer import CaptureSessionResult, SessionAudioBuffer


def test_pre_roll_is_clipped_to_session_sample_limit() -> None:
    buffer = SessionAudioBuffer(
        sample_rate=16_000,
        dtype="float32",
        max_session_samples=3,
    )

    buffer.ingest_chunk(np.array([0.1, 0.2], dtype=np.float32))
    buffer.ingest_chunk(np.array([0.3, 0.4], dtype=np.float32))

    buffer.start_session()

    assert buffer.session_limit_exceeded() is False
    assert np.allclose(buffer.stop_session(), np.array([0.2, 0.3, 0.4], dtype=np.float32))


def test_post_start_audio_is_clipped_to_session_sample_limit() -> None:
    buffer = SessionAudioBuffer(
        sample_rate=16_000,
        dtype="float32",
        max_session_samples=5,
    )
    buffer.start_session()

    buffer.ingest_chunk(np.array([0.1, 0.2, 0.3, 0.4], dtype=np.float32))
    buffer.ingest_chunk(np.array([0.5, 0.6, 0.7, 0.8], dtype=np.float32))

    assert buffer.session_limit_exceeded() is True
    assert np.allclose(
        buffer.stop_session(),
        np.array([0.1, 0.2, 0.3, 0.4, 0.5], dtype=np.float32),
    )


def test_stream_ready_chunks_and_tail_facts_include_seeded_pre_roll() -> None:
    buffer = SessionAudioBuffer(sample_rate=10, dtype="float32", pre_roll_seconds=1.0)
    buffer.configure_stream_chunk_size(4)
    buffer.ingest_chunk(np.array([0.1, 0.2, 0.3], dtype=np.float32))

    buffer.start_session()
    buffer.ingest_chunk(np.array([0.4, 0.5], dtype=np.float32))

    result = buffer.stop_session_with_streaming()

    assert isinstance(result, CaptureSessionResult)
    assert np.allclose(
        result.audio_samples,
        np.array([0.1, 0.2, 0.3, 0.4, 0.5], dtype=np.float32),
    )
    assert result.captured_samples == 5
    assert result.pre_roll_samples == 3
    assert result.post_start_samples == 2
    assert result.ready_chunk_count == 1
    assert np.allclose(
        result.ready_chunks[0],
        np.array([0.1, 0.2, 0.3, 0.4], dtype=np.float32),
    )
    assert np.allclose(result.tail_buffer, np.array([0.5], dtype=np.float32))
    assert result.tail_samples == 1


def test_empty_session_result_has_empty_audio_and_facts() -> None:
    buffer = SessionAudioBuffer(sample_rate=16_000, dtype="float32")

    buffer.start_session()
    result = buffer.stop_session_with_streaming()

    assert isinstance(result, CaptureSessionResult)
    assert result.audio_samples.size == 0
    assert result.ready_chunks == []
    assert result.tail_buffer.size == 0
    assert result.pre_roll_samples == 0
    assert result.post_start_samples == 0
    assert result.captured_samples == 0
    assert result.ready_chunk_count == 0
    assert result.tail_samples == 0


def test_audio_levels_are_collected_without_sounddevice() -> None:
    buffer = SessionAudioBuffer(sample_rate=16_000, dtype="float32")
    buffer.start_session()

    buffer.ingest_chunk(np.array([3.0, 4.0], dtype=np.float32))
    buffer.ingest_chunk(np.array([np.nan, 1.0], dtype=np.float32))
    buffer.ingest_chunk(np.array([np.nan], dtype=np.float32))

    assert buffer.take_audio_levels() == pytest.approx([np.sqrt(12.5), 1.0])
    assert buffer.take_audio_levels() == []


def test_audio_input_releases_lock_before_building_stopped_audio(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    audio = AudioInput(sample_rate=16_000, channels=1)
    audio.start_session()
    audio._callback(
        np.array([[0.1], [0.2]], dtype=np.float32),
        frames=2,
        time=None,
        status=cast(Any, 0),
    )

    def build_audio(chunks: list[np.ndarray]) -> np.ndarray:
        _assert_audio_lock_is_unheld(audio)
        return np.concatenate(chunks).astype("float32", copy=False)

    monkeypatch.setattr(audio._buffer, "audio_samples_from_chunks", build_audio)

    assert np.allclose(audio.stop_session(), np.array([0.1, 0.2], dtype=np.float32))


def test_audio_input_releases_lock_before_building_streaming_result(
    monkeypatch: pytest.MonkeyPatch,
) -> None:
    audio = AudioInput(sample_rate=16_000, channels=1)
    audio.configure_stream_chunk_size(4)
    audio.start_session()
    audio._callback(
        np.array([[0.1], [0.2]], dtype=np.float32),
        frames=2,
        time=None,
        status=cast(Any, 0),
    )
    original_build = audio._buffer.capture_result_from_snapshot

    def build_result(snapshot: Any) -> CaptureSessionResult:
        _assert_audio_lock_is_unheld(audio)
        return original_build(snapshot)

    monkeypatch.setattr(audio._buffer, "capture_result_from_snapshot", build_result)

    result = audio.stop_session_with_streaming()

    assert result.post_start_samples == 2
    assert np.allclose(result.audio_samples, np.array([0.1, 0.2], dtype=np.float32))


def _assert_audio_lock_is_unheld(audio: AudioInput) -> None:
    acquired = audio._lock.acquire(blocking=False)
    assert acquired is True
    audio._lock.release()
