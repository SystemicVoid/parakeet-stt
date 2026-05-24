"""Non-blocking audio capture with a rolling pre-roll buffer."""

from __future__ import annotations

import threading
from typing import Any

import numpy as np
import sounddevice as sd
from loguru import logger

from .audio_buffer import CaptureSessionResult, SessionAudioBuffer


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
        self._buffer = SessionAudioBuffer(
            sample_rate=sample_rate,
            dtype=dtype,
            pre_roll_seconds=pre_roll_seconds,
            max_session_samples=max_session_samples,
        )
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
            self._buffer.start_session()

    def stop_session(self) -> np.ndarray:
        """Stop accumulation and return the captured samples."""
        with self._lock:
            chunks = self._buffer.stop_session_chunks()
        return self._buffer.audio_samples_from_chunks(chunks)

    def abort_session(self) -> None:
        """Stop accumulation and discard any captured session audio."""
        with self._lock:
            self._buffer.abort_session()

    def stop_session_with_streaming(self) -> CaptureSessionResult:
        """Stop accumulation and return captured samples plus streaming slices."""
        with self._lock:
            snapshot = self._buffer.stop_session_with_streaming_snapshot()
        return self._buffer.capture_result_from_snapshot(snapshot)

    def session_limit_exceeded(self) -> bool:
        with self._lock:
            return self._buffer.session_limit_exceeded()

    def session_sample_limit(self) -> int | None:
        return self._buffer.session_sample_limit()

    def configure_stream_chunk_size(self, chunk_samples: int) -> None:
        """Set desired streaming chunk size in samples."""
        with self._lock:
            self._buffer.configure_stream_chunk_size(chunk_samples)

    def take_stream_chunks(self) -> list[np.ndarray]:
        """Take any ready-to-process streaming chunks."""
        with self._lock:
            return self._buffer.take_stream_chunks()

    def take_audio_levels(self) -> list[float]:
        """Take any ready-to-process RMS audio levels for the active session."""
        with self._lock:
            return self._buffer.take_audio_levels()

    def _callback(
        self, indata: np.ndarray, frames: int, time: Any, status: sd.CallbackFlags
    ) -> None:  # noqa: ANN401
        if status:
            logger.debug("Sounddevice status: {}", status)

        # Flatten to mono array and copy because the buffer is reused by sounddevice.
        chunk = np.copy(np.reshape(indata, (frames, self.channels))[:, 0])
        with self._lock:
            self._buffer.ingest_chunk(chunk)


__all__ = ["AudioInput", "CaptureSessionResult"]
