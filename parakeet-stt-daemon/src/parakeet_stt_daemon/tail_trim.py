"""Trim Seal path tail silence while preserving Runtime truth.

The Daemon uses this module before offline Seal path finalization. The trimmer
owns RMS fallback, optional VAD lifecycle, and the Runtime truth facts reported
through status/logging.

Policy: trimming must never remove speech. On a quiet mic (speech near -32 dBFS,
noise near -55 dBFS) a trailing fricative sits around -50 dBFS, so the silence
floor defaults well below that, and every trim is capped so a mis-set floor can
cost at most ``DEFAULT_MAX_TAIL_TRIM_SECS`` of audio instead of whole words.
"""

from __future__ import annotations

from dataclasses import dataclass
from typing import Literal, Protocol

import numpy as np
from loguru import logger

TailTrimMode = Literal["rms", "vad"]
VadFailureStage = Literal["load_failed", "runtime_failed", "warmup_failed"]

# Personal eval sweep (2026-09): -40 dBFS changed the last words of 17/100 clips,
# -50 left 2, -60 left 0. Keep the default at or below -60.
DEFAULT_SILENCE_FLOOR_DB = -60.0
DEFAULT_MAX_TAIL_TRIM_SECS = 0.35


class VadAdapter(Protocol):
    def prepare(self) -> None: ...

    def trim(self, samples: np.ndarray, sample_rate: int) -> np.ndarray: ...


@dataclass
class TailTrimOutcome:
    samples: np.ndarray
    tail_trim_mode: TailTrimMode
    vad_active: bool
    vad_fallback_reason: str | None


class SileroVadAdapter:
    def __init__(self) -> None:
        self._model: object | None = None

    def prepare(self) -> None:
        if self._model is not None:
            return
        from silero_vad import load_silero_vad

        self._model = load_silero_vad(onnx=True)

    def trim(self, samples: np.ndarray, sample_rate: int) -> np.ndarray:
        import torch
        from silero_vad import get_speech_timestamps

        self.prepare()
        if self._model is None:  # pragma: no cover - guarded by prepare
            raise RuntimeError("Silero VAD model not loaded")

        audio = samples.astype(np.float32, copy=False)
        waveform = torch.from_numpy(audio)
        speech_spans = get_speech_timestamps(
            waveform,
            self._model,
            sampling_rate=sample_rate,
        )
        if not speech_spans:
            return np.zeros((0,), dtype=np.float32)
        end_sample = int(speech_spans[-1].get("end", 0))
        end_sample = max(0, min(end_sample, audio.size))
        return audio[:end_sample]


class SealPathTailTrimmer:
    """Own Seal path tail trimming and Runtime truth for the Daemon."""

    def __init__(
        self,
        *,
        vad_enabled: bool,
        silence_floor_db: float,
        vad_adapter: VadAdapter | None = None,
        warmup_sample_rate: int = 16_000,
        max_tail_trim_secs: float = DEFAULT_MAX_TAIL_TRIM_SECS,
    ) -> None:
        self._vad_enabled = vad_enabled
        self._silence_floor_db = silence_floor_db
        self._max_tail_trim_secs = max(0.0, float(max_tail_trim_secs))
        self._vad_adapter = vad_adapter if vad_adapter is not None else SileroVadAdapter()
        self._warmup_sample_rate = warmup_sample_rate
        self._last_outcome = TailTrimOutcome(
            samples=np.zeros((0,), dtype=np.float32),
            tail_trim_mode="rms",
            vad_active=False,
            vad_fallback_reason="load_not_attempted" if vad_enabled else None,
        )

    @property
    def vad_enabled(self) -> bool:
        return self._vad_enabled

    @property
    def last_outcome(self) -> TailTrimOutcome:
        return self._last_outcome

    def prepare(self) -> None:
        if not self._vad_enabled:
            return
        if self._last_outcome.vad_active:
            return
        if self._last_outcome.vad_fallback_reason not in {None, "load_not_attempted"}:
            return

        try:
            self._vad_adapter.prepare()
        except (ImportError, OSError, RuntimeError, TypeError, ValueError) as exc:
            self._record_vad_fallback("load_failed", exc)
            logger.warning("Silero VAD unavailable; falling back to RMS trim: {}", exc)
            return

        try:
            self._vad_adapter.trim(
                np.zeros((self._warmup_sample_rate,), dtype=np.float32),
                self._warmup_sample_rate,
            )
        except (ImportError, OSError, RuntimeError, TypeError, ValueError) as exc:
            self._record_vad_fallback("warmup_failed", exc)
            logger.warning("Silero VAD warmup failed; disabling VAD tail trim: {}", exc)
            return

        self._last_outcome = TailTrimOutcome(
            samples=np.zeros((0,), dtype=np.float32),
            tail_trim_mode="vad",
            vad_active=True,
            vad_fallback_reason=None,
        )

    def trim(self, samples: np.ndarray, sample_rate: int) -> TailTrimOutcome:
        if samples.size == 0:
            return self._remember(samples.astype(np.float32, copy=False))

        if self._vad_enabled:
            trimmed = self._trim_with_vad(samples, sample_rate)
            if trimmed is not None:
                return self._remember(
                    self._cap_trim(samples, trimmed, sample_rate),
                    tail_trim_mode="vad",
                    vad_active=True,
                    vad_fallback_reason=None,
                )

        trimmed = self._trim_with_rms(samples, sample_rate)
        return self._remember(self._cap_trim(samples, trimmed, sample_rate))

    def _cap_trim(self, samples: np.ndarray, trimmed: np.ndarray, sample_rate: int) -> np.ndarray:
        """Keep at least ``size - max_tail_trim_secs`` samples whatever the trimmer said."""
        audio = samples.astype(np.float32, copy=False)
        minimum_keep = max(0, audio.size - int(sample_rate * self._max_tail_trim_secs))
        if trimmed.size >= minimum_keep:
            return trimmed
        return audio[:minimum_keep]

    def _trim_with_vad(self, samples: np.ndarray, sample_rate: int) -> np.ndarray | None:
        self.prepare()
        if not self._last_outcome.vad_active:
            return None

        try:
            return self._vad_adapter.trim(samples, sample_rate)
        except (ImportError, OSError, RuntimeError, TypeError, ValueError) as exc:
            self._record_vad_fallback("runtime_failed", exc)
            logger.warning("Silero VAD tail trim failed; falling back to RMS trim: {}", exc)
            return None

    def _trim_with_rms(
        self,
        samples: np.ndarray,
        sample_rate: int,
        window_ms: int = 50,
    ) -> np.ndarray:
        window = max(1, int(sample_rate * window_ms / 1000))
        audio = samples.astype(np.float32, copy=False)
        idx = audio.size
        while idx > 0:
            start = max(0, idx - window)
            window_slice = audio[start:idx]
            rms = np.sqrt(np.mean(window_slice**2))
            db = 20 * np.log10(max(rms, 1e-6))
            if db > self._silence_floor_db:
                break
            idx = start
        return audio[:idx]

    def _record_vad_fallback(self, stage: VadFailureStage, exc: Exception) -> None:
        self._last_outcome = TailTrimOutcome(
            samples=np.zeros((0,), dtype=np.float32),
            tail_trim_mode="rms",
            vad_active=False,
            vad_fallback_reason=_format_vad_failure_reason(stage, exc),
        )

    def _remember(
        self,
        samples: np.ndarray,
        *,
        tail_trim_mode: TailTrimMode | None = None,
        vad_active: bool | None = None,
        vad_fallback_reason: str | None = None,
    ) -> TailTrimOutcome:
        previous = self._last_outcome
        outcome = TailTrimOutcome(
            samples=samples,
            tail_trim_mode=tail_trim_mode or previous.tail_trim_mode,
            vad_active=previous.vad_active if vad_active is None else vad_active,
            vad_fallback_reason=(
                previous.vad_fallback_reason
                if vad_fallback_reason is None and tail_trim_mode != "vad"
                else vad_fallback_reason
            ),
        )
        self._last_outcome = outcome
        return outcome


def _format_vad_failure_reason(stage: VadFailureStage, exc: Exception) -> str:
    if isinstance(exc, ModuleNotFoundError):
        missing_dependency = exc.name or "unknown"
        return f"{stage}:missing_dependency:{missing_dependency}"
    return f"{stage}:{exc.__class__.__name__}"


__all__ = [
    "DEFAULT_MAX_TAIL_TRIM_SECS",
    "DEFAULT_SILENCE_FLOOR_DB",
    "SealPathTailTrimmer",
    "SileroVadAdapter",
    "TailTrimMode",
    "TailTrimOutcome",
    "VadAdapter",
]
