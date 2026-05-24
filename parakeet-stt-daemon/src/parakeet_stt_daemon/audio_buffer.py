"""Pure Pre-roll buffer and Session audio buffer policy."""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass

import numpy as np


@dataclass(frozen=True, slots=True)
class CaptureSessionResult:
    """Stop-time audio facts captured for one Session."""

    audio_samples: np.ndarray
    ready_chunks: list[np.ndarray]
    tail_buffer: np.ndarray
    pre_roll_samples: int
    post_start_samples: int

    @property
    def captured_samples(self) -> int:
        return int(self.audio_samples.size)

    @property
    def ready_chunk_count(self) -> int:
        return len(self.ready_chunks)

    @property
    def tail_samples(self) -> int:
        return int(self.tail_buffer.size)


@dataclass(frozen=True, slots=True)
class _CaptureSessionSnapshot:
    audio_chunks: list[np.ndarray]
    ready_chunks: list[np.ndarray]
    tail_buffer: np.ndarray
    pre_roll_samples: int
    post_start_samples: int


class SessionAudioBuffer:
    """Own Pre-roll retention, Session accumulation, streaming slices, and levels."""

    def __init__(
        self,
        *,
        sample_rate: int = 16_000,
        dtype: str = "float32",
        pre_roll_seconds: float = 2.5,
        max_session_samples: int | None = None,
    ) -> None:
        self.sample_rate = sample_rate
        self.dtype = dtype
        self._pre_roll_capacity = int(pre_roll_seconds * sample_rate)
        self._pre_roll: deque[np.ndarray] = deque()
        self._pre_roll_frames = 0
        self._session_chunks: list[np.ndarray] = []
        self._session_active = False
        self._max_session_samples = (
            max(1, int(max_session_samples)) if max_session_samples is not None else None
        )
        self._session_pre_roll_samples = 0
        self._session_samples = 0
        self._session_limit_exceeded = False
        self._stream_chunk_size: int | None = None
        self._stream_ready: list[np.ndarray] = []
        self._stream_buffer: np.ndarray = np.zeros((0,), dtype=np.float32)
        self._level_ready: list[float] = []

    def ingest_chunk(self, chunk: np.ndarray) -> None:
        """Append one flat audio chunk to the Pre-roll buffer and active Session."""
        audio = np.array(chunk, copy=True).reshape(-1)
        self._pre_roll.append(audio)
        self._pre_roll_frames += int(audio.size)
        self._trim_pre_roll()
        if not self._session_active:
            return

        accepted = self._clip_chunk_to_session_limit(audio)
        if accepted is None:
            return
        self._session_chunks.append(accepted)
        self._session_samples += int(accepted.size)
        self._collect_stream_chunks(accepted)
        self._collect_audio_level(accepted)
        self._enforce_session_sample_limit()

    def start_session(self) -> None:
        """Begin accumulating audio for a new Session, including bounded Pre-roll."""
        pre_roll_chunks = self._bounded_pre_roll_chunks()
        self._session_chunks = pre_roll_chunks
        self._session_pre_roll_samples = sum(int(chunk.size) for chunk in pre_roll_chunks)
        # Pre-roll is clipped up front so low session caps still start cleanly.
        # The live session budget applies to post-start capture.
        self._session_samples = 0
        self._session_limit_exceeded = False
        self._session_active = True
        self._stream_ready = []
        self._level_ready = []
        if self._stream_chunk_size:
            # Seed the Stream path with Pre-roll so live chunks keep the leading audio.
            self._stream_buffer = (
                np.concatenate(pre_roll_chunks, dtype=np.float32)
                if pre_roll_chunks
                else np.zeros((0,), dtype=np.float32)
            )
        else:
            self._stream_buffer = np.zeros((0,), dtype=np.float32)

    def stop_session(self) -> np.ndarray:
        """Stop accumulation and return the captured samples."""
        return self.audio_samples_from_chunks(self.stop_session_chunks())

    def stop_session_chunks(self) -> list[np.ndarray]:
        """Stop accumulation and return captured chunks for out-of-lock assembly."""
        self._session_active = False
        chunks = self._session_chunks
        self._reset_session_runtime_state()
        return chunks

    def abort_session(self) -> None:
        """Stop accumulation and discard any captured Session audio."""
        self._session_active = False
        self._reset_session_runtime_state()

    def stop_session_with_streaming(self) -> CaptureSessionResult:
        """Stop accumulation and return captured samples plus Stream path facts."""
        return self.capture_result_from_snapshot(self.stop_session_with_streaming_snapshot())

    def stop_session_with_streaming_snapshot(self) -> _CaptureSessionSnapshot:
        """Stop accumulation and snapshot stop-time Stream path facts."""
        self._session_active = False
        snapshot = _CaptureSessionSnapshot(
            audio_chunks=self._session_chunks,
            ready_chunks=self._stream_ready,
            tail_buffer=self._stream_buffer.copy(),
            pre_roll_samples=self._session_pre_roll_samples,
            post_start_samples=self._session_samples,
        )
        self._reset_session_runtime_state()
        return snapshot

    def audio_samples_from_chunks(self, chunks: list[np.ndarray]) -> np.ndarray:
        """Build captured audio samples from a stopped Session snapshot."""
        if not chunks:
            return np.zeros((0,), dtype=self.dtype)
        return np.concatenate(chunks).astype(self.dtype, copy=False)

    def capture_result_from_snapshot(
        self, snapshot: _CaptureSessionSnapshot
    ) -> CaptureSessionResult:
        """Build a compatible capture result from a stopped Session snapshot."""
        audio = (
            self.audio_samples_from_chunks(snapshot.audio_chunks)
            if snapshot.audio_chunks
            else np.zeros((0,), dtype=self.dtype)
        )
        return CaptureSessionResult(
            audio_samples=audio,
            ready_chunks=snapshot.ready_chunks,
            tail_buffer=snapshot.tail_buffer,
            pre_roll_samples=snapshot.pre_roll_samples,
            post_start_samples=snapshot.post_start_samples,
        )

    def session_limit_exceeded(self) -> bool:
        return self._session_limit_exceeded

    def session_sample_limit(self) -> int | None:
        return self._max_session_samples

    def configure_stream_chunk_size(self, chunk_samples: int) -> None:
        """Set desired Stream path chunk size in samples."""
        self._stream_chunk_size = max(1, int(chunk_samples))
        self._stream_ready = []
        self._level_ready = []
        self._stream_buffer = np.zeros((0,), dtype=np.float32)

    def take_stream_chunks(self) -> list[np.ndarray]:
        """Take any ready-to-process Stream path chunks."""
        ready = self._stream_ready
        self._stream_ready = []
        return ready

    def take_audio_levels(self) -> list[float]:
        """Take RMS audio levels collected for the active Session."""
        levels = self._level_ready
        self._level_ready = []
        return levels

    def _trim_pre_roll(self) -> None:
        while self._pre_roll_frames > self._pre_roll_capacity and self._pre_roll:
            removed = self._pre_roll.popleft()
            self._pre_roll_frames -= int(removed.size)

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
        retained_samples = self._session_pre_roll_samples + self._session_samples
        remaining = self._max_session_samples - retained_samples
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
        retained_samples = self._session_pre_roll_samples + self._session_samples
        if retained_samples < self._max_session_samples:
            return
        self._session_limit_exceeded = True
        self._session_active = False

    def _reset_session_runtime_state(self) -> None:
        self._session_chunks = []
        self._session_pre_roll_samples = 0
        self._session_samples = 0
        self._session_limit_exceeded = False
        self._stream_ready = []
        self._level_ready = []
        self._stream_buffer = np.zeros((0,), dtype=np.float32)


__all__ = ["CaptureSessionResult", "SessionAudioBuffer"]
