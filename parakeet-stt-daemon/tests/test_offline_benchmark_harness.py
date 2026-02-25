"""Unit tests for offline benchmark helpers in check_model.py."""

from __future__ import annotations

import importlib.util
import sys
from pathlib import Path

import pytest

_CHECK_MODEL_PATH = Path(__file__).resolve().parents[1] / "check_model.py"
_SPEC = importlib.util.spec_from_file_location("check_model", _CHECK_MODEL_PATH)
if _SPEC is None or _SPEC.loader is None:  # pragma: no cover - import bootstrap guard
    raise RuntimeError(f"Unable to load check_model module from {_CHECK_MODEL_PATH}")
_CHECK_MODEL = importlib.util.module_from_spec(_SPEC)
sys.modules[_SPEC.name] = _CHECK_MODEL
_SPEC.loader.exec_module(_CHECK_MODEL)

collect_benchmark_cases = _CHECK_MODEL.collect_benchmark_cases
compute_normalized_wer = _CHECK_MODEL.compute_normalized_wer
evaluate_regression_thresholds = _CHECK_MODEL.evaluate_regression_thresholds
normalize_transcript = _CHECK_MODEL.normalize_transcript
parse_benchmark_transcripts = _CHECK_MODEL.parse_benchmark_transcripts
summarize_timings_ms = _CHECK_MODEL.summarize_timings_ms


def test_parse_benchmark_transcripts_extracts_numbered_entries(tmp_path: Path) -> None:
    transcript_path = tmp_path / "transcripts.txt"
    transcript_path.write_text(
        '  1. "Hello, WORLD."\n'
        "2. “Call me at 415-555-0199.”\n"
        '3.    "Trailing spaces should trim."   \n',
        encoding="utf-8",
    )

    parsed = parse_benchmark_transcripts(transcript_path)

    assert parsed == {
        "sample_01": "Hello, WORLD.",
        "sample_02": "Call me at 415-555-0199.",
        "sample_03": "Trailing spaces should trim.",
    }


def test_collect_benchmark_cases_validates_transcript_audio_parity(tmp_path: Path) -> None:
    (tmp_path / "sample_01.wav").write_bytes(b"")
    transcripts = {"sample_01": "present", "sample_02": "missing"}

    with pytest.raises(ValueError, match="missing matching audio files"):
        collect_benchmark_cases(tmp_path, transcripts)


def test_normalize_and_wer_behavior() -> None:
    assert normalize_transcript("Ghostty, Pop!_OS, and Parakeet‑TDT!") == (
        "ghostty pop os and parakeet tdt"
    )
    assert compute_normalized_wer("The QUICK brown fox.", "the quick brown fox") == 0.0
    assert compute_normalized_wer("one two three", "one four three") == pytest.approx(1.0 / 3.0)


def test_summarize_timings_ms_reports_expected_percentiles() -> None:
    summary = summarize_timings_ms([10.0, 20.0, 30.0, 40.0])

    assert summary["avg"] == pytest.approx(25.0)
    assert summary["p50"] == pytest.approx(25.0)
    assert summary["p95"] == pytest.approx(38.5)


def test_evaluate_regression_thresholds_flags_only_exceedances() -> None:
    failures = evaluate_regression_thresholds(
        avg_wer=0.42,
        infer_p95_ms=900.0,
        finalize_p95_ms=1400.0,
        max_avg_wer=0.30,
        max_p95_infer_ms=950.0,
        max_p95_finalize_ms=1200.0,
    )

    assert len(failures) == 2
    assert "avg_wer" in failures[0]
    assert "finalize_p95_ms" in failures[1]
