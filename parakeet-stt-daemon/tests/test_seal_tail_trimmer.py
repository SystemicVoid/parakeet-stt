"""Unit coverage for Seal path tail trimming Runtime truth."""

from __future__ import annotations

from collections.abc import Callable

import numpy as np

from parakeet_stt_daemon.tail_trim import SealPathTailTrimmer


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
