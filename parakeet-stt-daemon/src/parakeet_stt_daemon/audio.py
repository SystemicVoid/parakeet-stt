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
        logger.info("Audio input stream started (device={}, rate={} Hz)", self.device, self.sample_rate)

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

    def stop_session(self) -> np.ndarray:
        """Stop accumulation and return the captured samples."""
        with self._lock:
            self._session_active = False
            chunks = self._session_chunks
            self._session_chunks = []
        if not chunks:
            return np.zeros((0,), dtype=self.dtype)
        return np.concatenate(chunks).astype(self.dtype, copy=False)

    def _callback(self, indata: np.ndarray, frames: int, time: Any, status: sd.CallbackFlags) -> None:  # noqa: ANN401
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

    def _trim_pre_roll(self) -> None:
        while self._pre_roll_frames > self._pre_roll_capacity and self._pre_roll:
            removed = self._pre_roll.popleft()
            self._pre_roll_frames -= len(removed)


__all__ = ["AudioInput"]
