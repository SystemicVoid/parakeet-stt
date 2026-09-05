"""Unit coverage for Seal path tail trimming Runtime truth."""

from __future__ import annotations

from collections.abc import Callable

import numpy as np

from parakeet_stt_daemon.tail_trim import (
    DEFAULT_MAX_TAIL_TRIM_SECS,
    DEFAULT_SILENCE_FLOOR_DB,
    SealPathTailTrimmer,
)


class FakeVadAdapter:
    def __init__(
        self,
        *,
        prepare_error: Exception | None = None,
        trim_result: Callable[[np.ndarray], np.ndarray] | None = None,
        trim_failure_call: int | None = None,
    ) -> None:
        self.prepare_error = prepare_error
        self.trim_result = trim_result
        self.trim_failure_call = trim_failure_call
        self.prepare_calls = 0
        self.trim_calls: list[tuple[np.ndarray, int]] = []

    def prepare(self) -> None:
        self.prepare_calls += 1
        if self.prepare_error is not None:
            raise self.prepare_error

    def trim(self, samples: np.ndarray, sample_rate: int) -> np.ndarray:
        self.trim_calls.append((samples.copy(), sample_rate))
        if self.trim_failure_call == len(self.trim_calls):
            raise RuntimeError("vad inference failed")
        if self.trim_result is not None:
            return self.trim_result(samples)
        return samples.astype(np.float32, copy=False)


def test_vad_trim_success_uses_adapter_and_reports_vad_truth() -> None:
    samples = np.arange(8, dtype=np.float32)
    fake_vad = FakeVadAdapter(trim_result=lambda input_samples: input_samples[:3])
    trimmer = SealPathTailTrimmer(
        vad_enabled=True,
        silence_floor_db=-40.0,
        vad_adapter=fake_vad,
    )

    outcome = trimmer.trim(samples, sample_rate=16_000)

    np.testing.assert_array_equal(outcome.samples, samples[:3])
    assert outcome.tail_trim_mode == "vad"
    assert outcome.vad_active is True
    assert outcome.vad_fallback_reason is None
    assert fake_vad.prepare_calls == 1
    trim_samples, trim_sample_rate = fake_vad.trim_calls[-1]
    np.testing.assert_array_equal(trim_samples, samples)
    assert trim_sample_rate == 16_000


def test_vad_load_failure_falls_back_to_rms_and_records_reason() -> None:
    exc = ModuleNotFoundError("No module named 'onnxruntime'")
    exc.name = "onnxruntime"
    samples = np.array([0.4, 0.4, 0.0, 0.0], dtype=np.float32)
    trimmer = SealPathTailTrimmer(
        vad_enabled=True,
        silence_floor_db=-40.0,
        vad_adapter=FakeVadAdapter(prepare_error=exc),
    )

    outcome = trimmer.trim(samples, sample_rate=20)

    np.testing.assert_array_equal(outcome.samples, samples[:2])
    assert outcome.tail_trim_mode == "rms"
    assert outcome.vad_active is False
    assert outcome.vad_fallback_reason == "load_failed:missing_dependency:onnxruntime"


def test_vad_runtime_failure_falls_back_to_rms_and_records_reason() -> None:
    samples = np.array([0.25, 0.25, 0.0, 0.0], dtype=np.float32)
    trimmer = SealPathTailTrimmer(
        vad_enabled=True,
        silence_floor_db=-40.0,
        vad_adapter=FakeVadAdapter(trim_failure_call=2),
    )

    outcome = trimmer.trim(samples, sample_rate=20)

    np.testing.assert_array_equal(outcome.samples, samples[:2])
    assert outcome.tail_trim_mode == "rms"
    assert outcome.vad_active is False
    assert outcome.vad_fallback_reason == "runtime_failed:RuntimeError"


def test_all_silence_rms_trim_returns_empty_samples() -> None:
    samples = np.zeros((4,), dtype=np.float32)
    trimmer = SealPathTailTrimmer(vad_enabled=False, silence_floor_db=-40.0)

    outcome = trimmer.trim(samples, sample_rate=20)

    assert outcome.samples.size == 0
    assert outcome.tail_trim_mode == "rms"
    assert outcome.vad_active is False
    assert outcome.vad_fallback_reason is None


def test_samples_shorter_than_trim_window_are_kept_when_above_floor() -> None:
    samples = np.array([0.2, 0.1, 0.05], dtype=np.float32)
    trimmer = SealPathTailTrimmer(vad_enabled=False, silence_floor_db=-40.0)

    outcome = trimmer.trim(samples, sample_rate=16_000)

    np.testing.assert_array_equal(outcome.samples, samples)
    assert outcome.tail_trim_mode == "rms"
    assert outcome.vad_active is False
    assert outcome.vad_fallback_reason is None


def _tone(seconds: float, level_db: float, sample_rate: int = 16_000) -> np.ndarray:
    """Constant-RMS sine at the requested dBFS level."""
    t = np.arange(int(seconds * sample_rate), dtype=np.float32) / sample_rate
    amplitude = np.sqrt(2.0) * 10 ** (level_db / 20)
    return (amplitude * np.sin(2 * np.pi * 440.0 * t)).astype(np.float32)


def test_default_floor_keeps_quiet_trailing_fricative() -> None:
    # Regression: on a quiet mic (speech around -32 dBFS, noise around -55 dBFS) a
    # trailing "s" sits near -50 dBFS. The old -40 dBFS default trimmed it away and
    # the Seal path dropped or changed the last word (personal eval cmd_079).
    speech = _tone(1.0, -32.0)
    fricative = _tone(0.3, -50.0)
    silence = _tone(0.5, -70.0)
    samples = np.concatenate([speech, fricative, silence])
    trimmer = SealPathTailTrimmer(vad_enabled=False, silence_floor_db=DEFAULT_SILENCE_FLOOR_DB)

    outcome = trimmer.trim(samples, sample_rate=16_000)

    assert outcome.samples.size >= speech.size + fricative.size
    assert outcome.samples.size < samples.size


def test_rms_trim_never_removes_more_than_cap() -> None:
    # Regression: a floor above the speech level once trimmed 4.1 s / 9 words from
    # personal eval cmd_073. The cap bounds the damage regardless of floor.
    speech = _tone(1.0, -32.0)
    quiet_speech = _tone(2.0, -50.0)
    samples = np.concatenate([speech, quiet_speech])
    trimmer = SealPathTailTrimmer(
        vad_enabled=False, silence_floor_db=-40.0, max_tail_trim_secs=0.35
    )

    outcome = trimmer.trim(samples, sample_rate=16_000)

    assert outcome.samples.size == samples.size - int(0.35 * 16_000)


def test_vad_trim_is_capped_too() -> None:
    samples = np.arange(16_000, dtype=np.float32)
    fake_vad = FakeVadAdapter(trim_result=lambda input_samples: input_samples[:100])
    trimmer = SealPathTailTrimmer(
        vad_enabled=True,
        silence_floor_db=-40.0,
        vad_adapter=fake_vad,
        max_tail_trim_secs=0.25,
    )

    outcome = trimmer.trim(samples, sample_rate=16_000)

    np.testing.assert_array_equal(outcome.samples, samples[: 16_000 - 4_000])
    assert outcome.tail_trim_mode == "vad"


def test_daemon_settings_and_eval_harness_share_tail_trim_defaults() -> None:
    from check_model_lib.constants import (
        DEFAULT_STREAM_MAX_TAIL_TRIM_SECS,
        DEFAULT_STREAM_SILENCE_FLOOR_DB,
    )
    from parakeet_stt_daemon.config import ServerSettings

    assert ServerSettings(_env_file=None).silence_floor_db == DEFAULT_SILENCE_FLOOR_DB
    assert DEFAULT_STREAM_SILENCE_FLOOR_DB == DEFAULT_SILENCE_FLOOR_DB
    assert DEFAULT_STREAM_MAX_TAIL_TRIM_SECS == DEFAULT_MAX_TAIL_TRIM_SECS
    assert DEFAULT_SILENCE_FLOOR_DB <= -60.0
