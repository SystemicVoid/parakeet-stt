"""Audio I/O, chunking, and streaming transcription runtime for the eval harness."""

from __future__ import annotations

import time
import wave
from pathlib import Path
from typing import Any

import numpy as np

from check_model_lib.constants import SAMPLE_RATE
from parakeet_stt_daemon.model import ParakeetStreamingTranscriber


def _read_wav_samples(path: Path) -> tuple[np.ndarray, int]:
    try:
        import soundfile as sf

        samples, sample_rate = sf.read(path, dtype="float32", always_2d=False)
        sample_array = np.asarray(samples, dtype=np.float32)
    except Exception as err:  # pragma: no cover - minimal fallback
        with wave.open(str(path), "rb") as wf:
            sample_rate = wf.getframerate()
            channels = wf.getnchannels()
            sample_width = wf.getsampwidth()
            raw = wf.readframes(wf.getnframes())
        dtype_map: dict[int, tuple[str, float]] = {
            1: ("<u1", 128.0),
            2: ("<i2", 32768.0),
            4: ("<i4", 2147483648.0),
        }
        if sample_width not in dtype_map:
            raise ValueError(
                f"Unsupported wav sample width ({sample_width} bytes) in {path}"
            ) from err
        dtype, scale = dtype_map[sample_width]
        sample_array = np.frombuffer(raw, dtype=np.dtype(dtype)).astype(np.float32)
        if sample_width == 1:
            sample_array = (sample_array - 128.0) / scale
        else:
            sample_array = sample_array / scale
        if channels > 1:
            sample_array = sample_array.reshape(-1, channels).mean(axis=1)
        return sample_array.reshape(-1), sample_rate

    if sample_array.ndim > 1:
        sample_array = sample_array.mean(axis=1)
    return sample_array.reshape(-1), int(sample_rate)


def _trim_tail_with_rms(
    samples: np.ndarray,
    *,
    sample_rate: int,
    silence_floor_db: float,
    window_ms: int = 50,
) -> np.ndarray:
    if samples.size == 0:
        return samples
    window = max(1, int(sample_rate * window_ms / 1000))
    audio = samples.astype(np.float32, copy=False)
    idx = audio.size
    while idx > 0:
        start = max(0, idx - window)
        window_slice = audio[start:idx]
        rms = np.sqrt(np.mean(window_slice**2))
        db = 20 * np.log10(max(rms, 1e-6))
        if db > silence_floor_db:
            break
        idx = start
    return audio[:idx]


def _split_stream_ready_and_tail(
    samples: np.ndarray, *, sample_rate: int, chunk_secs: float
) -> tuple[list[np.ndarray], np.ndarray]:
    if samples.size == 0:
        return [], np.zeros((0,), dtype=np.float32)
    chunk_size = max(1, int(sample_rate * chunk_secs))
    ready_limit = (samples.size // chunk_size) * chunk_size
    ready_chunks = [
        np.asarray(samples[idx : idx + chunk_size], dtype=np.float32)
        for idx in range(0, ready_limit, chunk_size)
    ]
    tail = np.asarray(samples[ready_limit:], dtype=np.float32)
    return ready_chunks, tail


def _transcribe_stream_seal(
    streamer: ParakeetStreamingTranscriber,
    samples: np.ndarray,
    *,
    sample_rate: int,
    silence_floor_db: float,
    max_tail_trim_secs: float,
) -> tuple[str, float]:
    ready_chunks, tail = _split_stream_ready_and_tail(
        samples, sample_rate=sample_rate, chunk_secs=streamer.chunk_secs
    )
    trimmed_tail = tail
    if tail.size:
        candidate_tail = _trim_tail_with_rms(
            tail,
            sample_rate=sample_rate,
            silence_floor_db=silence_floor_db,
        )
        max_trim_samples = max(0, int(sample_rate * max_tail_trim_secs))
        minimum_keep = max(0, tail.size - max_trim_samples)
        if candidate_tail.size < minimum_keep:
            candidate_tail = tail[:minimum_keep]
        trimmed_tail = candidate_tail

    def _finalize_with_tail(active_tail: np.ndarray) -> str:
        session = streamer.start_session(sample_rate)
        for chunk in ready_chunks:
            session.feed(chunk)
        if active_tail.size:
            session.feed(active_tail)
        return session.finalize()

    infer_start = time.perf_counter()
    hypothesis = _finalize_with_tail(trimmed_tail)
    # Avoid catastrophic empty outputs when tail trimming removed endpoint cues.
    if not hypothesis.strip() and tail.size and trimmed_tail.size < tail.size:
        hypothesis = _finalize_with_tail(tail)
    infer_ms = (time.perf_counter() - infer_start) * 1000.0
    return hypothesis, infer_ms


def generate_sine(duration_secs: float, freq_hz: float, amplitude: float) -> np.ndarray:
    t = np.linspace(0, duration_secs, int(SAMPLE_RATE * duration_secs), endpoint=False)
    return (amplitude * np.sin(2 * np.pi * freq_hz * t)).astype(np.float32)


def write_wav(path: Path, samples: np.ndarray) -> None:
    sf: Any | None
    try:
        import soundfile as sf_mod
    except ImportError:
        sf = None  # pragma: no cover - minimal fallback
    else:
        sf = sf_mod

    if sf is not None:
        try:
            sf.write(path, samples, SAMPLE_RATE)
            return
        except (OSError, RuntimeError, TypeError, ValueError):
            pass  # pragma: no cover - minimal fallback

    # Minimal fallback path for environments without soundfile or where writes fail.
    pcm = (np.clip(samples, -1.0, 1.0) * 32767).astype("<i2")
    with wave.open(str(path), "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(SAMPLE_RATE)
        wf.writeframes(pcm.tobytes())


def split_chunks(samples: np.ndarray, chunk_size: int) -> list[np.ndarray]:
    return [samples[idx : idx + chunk_size] for idx in range(0, samples.size, chunk_size)]
