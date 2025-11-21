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
        self._stream_chunk_size: int | None = None
        self._stream_ready: list[np.ndarray] = []
        self._stream_buffer: np.ndarray = np.zeros((0,), dtype=np.float32)
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
            self._session_chunks = [chunk.copy() for chunk in self._pre_roll]
            self._session_active = True
            self._stream_ready = []
            self._stream_buffer = np.zeros((0,), dtype=np.float32)

    def stop_session(self) -> np.ndarray:
        """Stop accumulation and return the captured samples."""
        with self._lock:
            self._session_active = False
            chunks = self._session_chunks
            self._session_chunks = []
            self._stream_ready = []
            self._stream_buffer = np.zeros((0,), dtype=np.float32)
        if not chunks:
            return np.zeros((0,), dtype=self.dtype)
        return np.concatenate(chunks).astype(self.dtype, copy=False)

    def stop_session_with_streaming(self) -> tuple[np.ndarray, list[np.ndarray], np.ndarray]:
        """Stop accumulation and return captured samples plus streaming slices.

        Returns: (full_audio, ready_chunks, leftover_buffer)
        """
        with self._lock:
            self._session_active = False
            chunks = self._session_chunks
            self._session_chunks = []
            ready = self._stream_ready
            tail = self._stream_buffer.copy()
            self._stream_ready = []
            self._stream_buffer = np.zeros((0,), dtype=np.float32)
        audio = np.concatenate(chunks).astype(self.dtype, copy=False) if chunks else np.zeros(
            (0,), dtype=self.dtype
        )
        return audio, ready, tail

    def configure_stream_chunk_size(self, chunk_samples: int) -> None:
        """Set desired streaming chunk size in samples."""
        with self._lock:
            self._stream_chunk_size = max(1, int(chunk_samples))
            self._stream_ready = []
            self._stream_buffer = np.zeros((0,), dtype=np.float32)

    def take_stream_chunks(self) -> list[np.ndarray]:
        """Take any ready-to-process streaming chunks."""
        with self._lock:
            ready = self._stream_ready
            self._stream_ready = []
        return ready

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
                self._session_chunks.append(chunk)
                self._collect_stream_chunks(chunk)

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


__all__ = ["AudioInput"]
