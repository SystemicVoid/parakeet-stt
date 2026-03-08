"""Non-blocking audio capture with a rolling pre-roll buffer."""

from __future__ import annotations

import threading
from collections import deque
from typing import Any

import numpy as np
import sounddevice as sd
from loguru import logger


class AudioInput:
    """Stream microphone samples into a ring buffer and session accumulator."""

    def __init__(
        self,
        *,
        sample_rate: int = 16_000,
        channels: int = 1,
        dtype: str = "float32",
        pre_roll_seconds: float = 2.5,
        device: int | str | None = None,
        blocksize: int | None = None,
        max_session_samples: int | None = None,
    ) -> None:
        self.sample_rate = sample_rate
        self.channels = channels
        self.dtype = dtype
        self.device = device
        self.blocksize = blocksize
        self._pre_roll_capacity = int(pre_roll_seconds * sample_rate)

        self._pre_roll: deque[np.ndarray] = deque()
        self._pre_roll_frames = 0
        self._session_chunks: list[np.ndarray] = []
        self._session_active = False
        self._max_session_samples = (
            max(1, int(max_session_samples)) if max_session_samples is not None else None
        )
        self._session_samples = 0
        self._session_limit_exceeded = False
        self._stream_chunk_size: int | None = None
        self._stream_ready: list[np.ndarray] = []
        self._stream_buffer: np.ndarray = np.zeros((0,), dtype=np.float32)
        self._level_ready: list[float] = []
        self._lock = threading.Lock()
        self._stream: sd.InputStream | None = None

    def start(self) -> None:
        """Open the input stream if it is not already running."""
        if self._stream is not None:
            return

        self._stream = sd.InputStream(
            samplerate=self.sample_rate,
            channels=self.channels,
            dtype=self.dtype,
            device=self.device,
            blocksize=self.blocksize,
            callback=self._callback,
        )
        self._stream.start()
        logger.info(
            "Audio input stream started (device={}, rate={} Hz)", self.device, self.sample_rate
        )

    def stop(self) -> None:
        if self._stream is None:
            return
        self._stream.stop()
        self._stream.close()
        self._stream = None
        logger.info("Audio input stream stopped")

    def start_session(self) -> None:
        """Begin accumulating audio for a new session (includes pre-roll)."""
        with self._lock:
            pre_roll_chunks = self._bounded_pre_roll_chunks()
            self._session_chunks = pre_roll_chunks
            # Pre-roll is clipped up front so low session caps still start cleanly.
            # The live session budget applies to post-start capture.
            self._session_samples = 0
            self._session_limit_exceeded = False
            self._session_active = True
            self._stream_ready = []
            self._level_ready = []
            if self._stream_chunk_size:
                # Seed the streaming buffer with the pre-roll so the streaming path
                # sees the leading audio instead of starting from the first post-start chunk.
                self._stream_buffer = (
                    np.concatenate(pre_roll_chunks, dtype=np.float32)
                    if pre_roll_chunks
                    else np.zeros((0,), dtype=np.float32)
                )
            else:
                self._stream_buffer = np.zeros((0,), dtype=np.float32)
            self._enforce_session_sample_limit()

    def stop_session(self) -> np.ndarray:
        """Stop accumulation and return the captured samples."""
        with self._lock:
            self._session_active = False
            chunks = self._session_chunks
            self._reset_session_runtime_state()
        if not chunks:
            return np.zeros((0,), dtype=self.dtype)
        return np.concatenate(chunks).astype(self.dtype, copy=False)

    def abort_session(self) -> None:
        """Stop accumulation and discard any captured session audio."""
        with self._lock:
            self._session_active = False
            self._reset_session_runtime_state()

    def stop_session_with_streaming(self) -> tuple[np.ndarray, list[np.ndarray], np.ndarray]:
        """Stop accumulation and return captured samples plus streaming slices.

        Returns: (full_audio, ready_chunks, leftover_buffer)
        """
        with self._lock:
            self._session_active = False
            chunks = self._session_chunks
            ready = self._stream_ready
            tail = self._stream_buffer.copy()
            self._reset_session_runtime_state()
        audio = (
            np.concatenate(chunks).astype(self.dtype, copy=False)
            if chunks
            else np.zeros((0,), dtype=self.dtype)
        )
        return audio, ready, tail

    def session_limit_exceeded(self) -> bool:
        with self._lock:
            return self._session_limit_exceeded

    def session_sample_limit(self) -> int | None:
        return self._max_session_samples

    def configure_stream_chunk_size(self, chunk_samples: int) -> None:
        """Set desired streaming chunk size in samples."""
        with self._lock:
            self._stream_chunk_size = max(1, int(chunk_samples))
            self._stream_ready = []
            self._level_ready = []
            self._stream_buffer = np.zeros((0,), dtype=np.float32)

    def take_stream_chunks(self) -> list[np.ndarray]:
        """Take any ready-to-process streaming chunks."""
        with self._lock:
            ready = self._stream_ready
            self._stream_ready = []
        return ready

    def take_audio_levels(self) -> list[float]:
        """Take any ready-to-process RMS audio levels for the active session."""
        with self._lock:
            levels = self._level_ready
            self._level_ready = []
        return levels

    def _callback(
        self, indata: np.ndarray, frames: int, time: Any, status: sd.CallbackFlags
    ) -> None:  # noqa: ANN401
        if status:
            logger.debug("Sounddevice status: {}", status)

        # Flatten to mono array and copy because the buffer is reused by sounddevice.
        chunk = np.copy(np.reshape(indata, (frames, self.channels))[:, 0])
        with self._lock:
            self._pre_roll.append(chunk)
            self._pre_roll_frames += len(chunk)
            self._trim_pre_roll()
            if self._session_active:
                accepted = self._clip_chunk_to_session_limit(chunk)
                if accepted is None:
                    return
                self._session_chunks.append(accepted)
                self._session_samples += int(accepted.size)
                self._collect_stream_chunks(accepted)
                self._collect_audio_level(accepted)
                self._enforce_session_sample_limit()

    def _trim_pre_roll(self) -> None:
        while self._pre_roll_frames > self._pre_roll_capacity and self._pre_roll:
            removed = self._pre_roll.popleft()
            self._pre_roll_frames -= len(removed)

    def _collect_stream_chunks(self, chunk: np.ndarray) -> None:
        if not self._stream_chunk_size:
            return
        combined = (
            chunk
            if self._stream_buffer.size == 0
            else np.concatenate([self._stream_buffer, chunk], dtype=np.float32)
        )
        idx = 0
        chunk_size = self._stream_chunk_size
        while combined.size - idx >= chunk_size:
            next_idx = idx + chunk_size
            self._stream_ready.append(np.array(combined[idx:next_idx], copy=True))
            idx = next_idx
        self._stream_buffer = combined[idx:]

    def _collect_audio_level(self, chunk: np.ndarray) -> None:
        audio = np.asarray(chunk, dtype=np.float32).reshape(-1)
        if audio.size == 0:
            return
        finite = np.isfinite(audio)
        if not bool(np.all(finite)):
            audio = audio[finite]
        if audio.size == 0:
            return
        rms = float(np.sqrt(np.mean(audio * audio)))
        if np.isfinite(rms):
            self._level_ready.append(rms)

    def _bounded_pre_roll_chunks(self) -> list[np.ndarray]:
        if self._max_session_samples is None:
            return [chunk.copy() for chunk in self._pre_roll]

        remaining = self._max_session_samples
        if remaining <= 0:
            return []

        retained: list[np.ndarray] = []
        for chunk in reversed(self._pre_roll):
            if remaining <= 0:
                break
            if chunk.size <= remaining:
                retained.append(chunk.copy())
                remaining -= int(chunk.size)
                continue
            retained.append(np.array(chunk[-remaining:], copy=True))
            remaining = 0
        retained.reverse()
        return retained

    def _clip_chunk_to_session_limit(self, chunk: np.ndarray) -> np.ndarray | None:
        if self._max_session_samples is None:
            return chunk
        remaining = self._max_session_samples - self._session_samples
        if remaining <= 0:
            self._session_limit_exceeded = True
            self._session_active = False
            return None
        if chunk.size <= remaining:
            return chunk
        return np.array(chunk[:remaining], copy=True)

    def _enforce_session_sample_limit(self) -> None:
        if self._max_session_samples is None:
            return
        if self._session_samples < self._max_session_samples:
            return
        self._session_limit_exceeded = True
        self._session_active = False

    def _reset_session_runtime_state(self) -> None:
        self._session_chunks = []
        self._session_samples = 0
        self._session_limit_exceeded = False
        self._stream_ready = []
        self._level_ready = []
        self._stream_buffer = np.zeros((0,), dtype=np.float32)


__all__ = ["AudioInput"]
