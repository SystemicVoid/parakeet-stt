"""Unit tests for offline benchmark helpers in check_model.py."""

from __future__ import annotations

import argparse
import importlib.util
import sys
from pathlib import Path

import numpy as np
import pytest

_CHECK_MODEL_PATH = Path(__file__).resolve().parents[1] / "check_model.py"
_SPEC = importlib.util.spec_from_file_location("check_model", _CHECK_MODEL_PATH)
if _SPEC is None or _SPEC.loader is None:  # pragma: no cover - import bootstrap guard
    raise RuntimeError(f"Unable to load check_model module from {_CHECK_MODEL_PATH}")
_CHECK_MODEL = importlib.util.module_from_spec(_SPEC)
sys.modules[_SPEC.name] = _CHECK_MODEL
_SPEC.loader.exec_module(_CHECK_MODEL)

collect_benchmark_cases = _CHECK_MODEL.collect_benchmark_cases
compute_command_exact_match_rate = _CHECK_MODEL.compute_command_exact_match_rate
compute_command_match_metrics = _CHECK_MODEL.compute_command_match_metrics
compute_critical_token_recall = _CHECK_MODEL.compute_critical_token_recall
compute_normalized_wer = _CHECK_MODEL.compute_normalized_wer
compute_punctuation_metrics = _CHECK_MODEL.compute_punctuation_metrics
compute_weighted_wer = _CHECK_MODEL.compute_weighted_wer
evaluate_regression_thresholds = _CHECK_MODEL.evaluate_regression_thresholds
normalize_command_text = _CHECK_MODEL.normalize_command_text
normalize_transcript = _CHECK_MODEL.normalize_transcript
parse_command_intent_slots = _CHECK_MODEL.parse_command_intent_slots
parse_benchmark_manifest = _CHECK_MODEL.parse_benchmark_manifest
parse_benchmark_transcripts = _CHECK_MODEL.parse_benchmark_transcripts
summarize_timings_ms = _CHECK_MODEL.summarize_timings_ms
resolve_benchmark_cases = _CHECK_MODEL._resolve_benchmark_cases
apply_profile_defaults = _CHECK_MODEL._apply_profile_defaults
transcribe_stream_seal = _CHECK_MODEL._transcribe_stream_seal

_CMD_073_REFERENCE = (
    "Push, and give me the command to run the tool for testing. "
    "I am not sure I want the integration test if not absolutely necessary."
)
_CMD_073_SUFFIX = "if not absolutely necessary"
_CMD_073_TRIMMED_HYPOTHESIS = "Push and give me the command to run the tool for testing."
_CMD_073_RECOVERED_HYPOTHESIS = (
    "Push and give me the command to run the tool for testing. "
    "I'm not sure I want the integration test if not absolutely necessary."
)

_CMD_087_REFERENCE = "Can you get rid of it, then, if it does nothing?"
_CMD_087_RECOVERED_HYPOTHESIS = "Can you get rid of it then if it does nothing?"


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


def test_parse_benchmark_manifest_filters_tier_and_normalizes_tokens(tmp_path: Path) -> None:
    audio_daily = tmp_path / "daily_01.wav"
    audio_smoke = tmp_path / "smoke_01.wav"
    audio_daily.write_bytes(b"")
    audio_smoke.write_bytes(b"")
    manifest_path = tmp_path / "manifest.jsonl"
    manifest_path.write_text(
        (
            '{"sample_id":"daily_01","audio_path":"daily_01.wav","reference":"stt start --paste",'
            '"tier":"daily","domain":"command","critical_tokens":["stt","--paste"]}\n'
            '{"sample_id":"smoke_01","audio_path":"smoke_01.wav","reference":"hello world",'
            '"tier":"smoke","domain":"dictation","critical_tokens":[]}\n'
        ),
        encoding="utf-8",
    )

    cases = parse_benchmark_manifest(manifest_path, bench_dir=tmp_path, bench_tier="daily")

    assert len(cases) == 1
    assert cases[0].sample_id == "daily_01"
    assert cases[0].domain == "command"
    assert cases[0].critical_tokens == ("stt", "paste")


def test_resolve_manifest_cases_can_append_legacy_transcripts(tmp_path: Path) -> None:
    bench_dir = tmp_path
    (bench_dir / "sample_01.wav").write_bytes(b"")
    (bench_dir / "transcripts.txt").write_text('1. "legacy prompt"\n', encoding="utf-8")

    manifest_path = bench_dir / "manifest.jsonl"
    (bench_dir / "personal").mkdir(parents=True, exist_ok=True)
    (bench_dir / "personal/audio").mkdir(parents=True, exist_ok=True)
    (bench_dir / "personal/audio/cmd_001.wav").write_bytes(b"")
    manifest_path.write_text(
        (
            '{"sample_id":"cmd_001","audio_path":"personal/audio/cmd_001.wav",'
            '"reference":"new prompt","tier":"daily","domain":"command"}\n'
        ),
        encoding="utf-8",
    )

    cases, resolved_manifest_path, appended_transcripts_path = resolve_benchmark_cases(
        bench_dir=bench_dir,
        bench_tier="all",
        bench_manifest=manifest_path,
        bench_transcripts=bench_dir / "transcripts.txt",
        bench_append_legacy=True,
    )

    assert len(cases) == 2
    assert {case.sample_id for case in cases} == {"cmd_001", "sample_01"}
    assert resolved_manifest_path == manifest_path.resolve()
    assert appended_transcripts_path == (bench_dir / "transcripts.txt").resolve()


def test_resolve_manifest_cases_rejects_duplicate_sample_ids_when_appending_legacy(
    tmp_path: Path,
) -> None:
    bench_dir = tmp_path
    (bench_dir / "sample_01.wav").write_bytes(b"")
    (bench_dir / "transcripts.txt").write_text('1. "legacy prompt"\n', encoding="utf-8")

    manifest_path = bench_dir / "manifest.jsonl"
    manifest_path.write_text(
        (
            '{"sample_id":"sample_01","audio_path":"sample_01.wav",'
            '"reference":"new prompt","tier":"daily","domain":"command"}\n'
        ),
        encoding="utf-8",
    )

    with pytest.raises(ValueError, match="sample_id already exists"):
        resolve_benchmark_cases(
            bench_dir=bench_dir,
            bench_tier="all",
            bench_manifest=manifest_path,
            bench_transcripts=bench_dir / "transcripts.txt",
            bench_append_legacy=True,
        )


def test_normalize_and_wer_behavior() -> None:
    assert normalize_transcript("Ghostty, Pop!_OS, and Parakeet‑TDT!") == (
        "ghostty pop os and parakeet tdt"
    )
    assert normalize_command_text("Open the browser — and go to “GitHub”.") == (
        "open browser go github"
    )
    assert parse_command_intent_slots("Could you open the browser and go to GitHub?") == {
        "intent": "open",
        "slots": ["browser", "go", "github"],
        "signature": "open|browser go github",
    }
    assert normalize_command_text("Sort imports by module name.") == "sort imports by module name"
    assert parse_command_intent_slots("Sort imports by module name.") == {
        "intent": "sort",
        "slots": ["imports", "by", "module", "name"],
        "signature": "sort|imports by module name",
    }
    assert compute_normalized_wer("The QUICK brown fox.", "the quick brown fox") == 0.0
    assert compute_normalized_wer("one two three", "one four three") == pytest.approx(1.0 / 3.0)


def test_summarize_timings_ms_reports_expected_percentiles() -> None:
    summary = summarize_timings_ms([10.0, 20.0, 30.0, 40.0])

    assert summary["avg"] == pytest.approx(25.0)
    assert summary["p50"] == pytest.approx(25.0)
    assert summary["p95"] == pytest.approx(38.5)


def test_command_and_critical_token_metrics() -> None:
    rows = [
        {
            "domain": "command",
            "wer": 0.2,
            "normalized_reference": "stt start paste",
            "normalized_hypothesis": "stt start paste",
            "normalized_critical_tokens": ["stt", "paste"],
        },
        {
            "domain": "command",
            "wer": 0.4,
            "normalized_reference": "uv run pytest",
            "normalized_hypothesis": "uv run pytests",
            "normalized_critical_tokens": ["uv", "pytest"],
        },
        {
            "domain": "dictation",
            "wer": 0.1,
            "normalized_reference": "hello world",
            "normalized_hypothesis": "hello world",
            "normalized_critical_tokens": [],
        },
    ]

    weighted_wer, command_wer, dictation_wer = compute_weighted_wer(rows)

    assert command_wer == pytest.approx(0.3)
    assert dictation_wer == pytest.approx(0.1)
    assert weighted_wer == pytest.approx(0.8 * 0.3 + 0.2 * 0.1)
    assert compute_command_exact_match_rate(rows) == pytest.approx(0.5)
    assert compute_critical_token_recall(rows) == pytest.approx(0.75)


def test_command_match_metrics_expose_strict_normalized_and_intent_scores() -> None:
    rows = [
        {
            "domain": "command",
            "reference": "Open the browser and go to GitHub",
            "hypothesis": "open browser and go to github",
            "normalized_reference": "open the browser and go to github",
            "normalized_hypothesis": "open browser and go to github",
            "command_normalized_reference": "open browser go github",
            "command_normalized_hypothesis": "open browser go github",
            "command_reference_signature": "open|browser go github",
            "command_hypothesis_signature": "open|browser go github",
        },
        {
            "domain": "command",
            "reference": "Could you open browser and go to GitHub",
            "hypothesis": "open browser go github",
            "normalized_reference": "could you open browser and go to github",
            "normalized_hypothesis": "open browser go github",
            "command_normalized_reference": "open browser go github",
            "command_normalized_hypothesis": "open browser go github",
            "command_reference_signature": "open|browser go github",
            "command_hypothesis_signature": "open|browser go github",
        },
        {
            "domain": "command",
            "reference": "Open browser and go to GitHub",
            "hypothesis": "close browser and go to github",
            "normalized_reference": "open browser and go to github",
            "normalized_hypothesis": "close browser and go to github",
            "command_normalized_reference": "open browser go github",
            "command_normalized_hypothesis": "close browser go github",
            "command_reference_signature": "open|browser go github",
            "command_hypothesis_signature": "close|browser go github",
        },
    ]

    metrics = compute_command_match_metrics(rows)

    assert metrics["strict_exact_match_rate"] == pytest.approx(0.0)
    assert metrics["normalized_exact_match_rate"] == pytest.approx(2.0 / 3.0)
    assert metrics["intent_slot_match_rate"] == pytest.approx(2.0 / 3.0)


def test_command_match_metrics_do_not_drop_sort_keyword() -> None:
    rows = [
        {
            "domain": "command",
            "reference": "sort imports",
            "hypothesis": "imports",
            "normalized_reference": "sort imports",
            "normalized_hypothesis": "imports",
            "command_normalized_reference": "sort imports",
            "command_normalized_hypothesis": "imports",
            "command_reference_signature": "sort|imports",
            "command_hypothesis_signature": "imports|",
        }
    ]

    metrics = compute_command_match_metrics(rows)

    assert metrics["strict_exact_match_rate"] == 0.0
    assert metrics["normalized_exact_match_rate"] == 0.0
    assert metrics["intent_slot_match_rate"] == 0.0


def test_punctuation_metrics_capture_order_and_terminal_accuracy() -> None:
    rows = [
        {
            "reference": "Hello, world. Is this good?",
            "hypothesis": "Hello world. Is this good.",
        },
        {
            "reference": "Ship it!",
            "hypothesis": "Ship it!",
        },
    ]

    metrics = compute_punctuation_metrics(rows)

    assert metrics["reference_count"] == pytest.approx(4.0)
    assert metrics["hypothesis_count"] == pytest.approx(3.0)
    assert metrics["matched_count"] == pytest.approx(2.0)
    assert metrics["f1"] == pytest.approx((2.0 * 2.0) / (4.0 + 3.0))
    assert metrics["terminal_accuracy"] == pytest.approx(0.5)


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


def test_evaluate_regression_thresholds_requires_baseline_for_relative_gates() -> None:
    failures = evaluate_regression_thresholds(
        avg_wer=0.10,
        infer_p95_ms=100.0,
        finalize_p95_ms=100.0,
        max_avg_wer=None,
        max_p95_infer_ms=None,
        max_p95_finalize_ms=None,
        weighted_wer=0.12,
        command_exact_match_rate=0.80,
        critical_token_recall=0.90,
        warm_finalize_p95_ms=110.0,
        max_weighted_wer_delta=0.02,
        max_command_exact_match_drop=0.05,
        max_critical_token_recall_drop=0.03,
        max_warm_p95_finalize_ms_delta=20.0,
    )

    assert len(failures) == 4
    assert "baseline weighted_wer" in failures[0]


def test_evaluate_regression_thresholds_relative_gate_detects_drift() -> None:
    failures = evaluate_regression_thresholds(
        avg_wer=0.10,
        infer_p95_ms=100.0,
        finalize_p95_ms=100.0,
        max_avg_wer=None,
        max_p95_infer_ms=None,
        max_p95_finalize_ms=None,
        weighted_wer=0.21,
        command_exact_match_rate=0.61,
        critical_token_recall=0.91,
        warm_finalize_p95_ms=150.0,
        baseline={
            "weighted_wer": 0.15,
            "command_exact_match_rate": 0.70,
            "critical_token_recall": 0.95,
            "warm_finalize_p95_ms": 120.0,
        },
        max_weighted_wer_delta=0.03,
        max_command_exact_match_drop=0.05,
        max_critical_token_recall_drop=0.03,
        max_warm_p95_finalize_ms_delta=20.0,
    )

    assert len(failures) == 4
    assert "weighted_wer" in failures[0]


def test_evaluate_regression_thresholds_with_punctuation_gates() -> None:
    failures = evaluate_regression_thresholds(
        avg_wer=0.10,
        infer_p95_ms=100.0,
        finalize_p95_ms=100.0,
        max_avg_wer=None,
        max_p95_infer_ms=None,
        max_p95_finalize_ms=None,
        punctuation_f1=0.70,
        terminal_punctuation_accuracy=0.80,
        min_punctuation_f1=0.75,
        min_terminal_punctuation_accuracy=0.90,
        baseline={
            "punctuation_f1": 0.85,
            "terminal_punctuation_accuracy": 0.95,
        },
        max_punctuation_f1_drop=0.05,
        max_terminal_punctuation_accuracy_drop=0.10,
    )

    assert len(failures) == 4
    assert "punctuation_f1" in failures[0]


def test_apply_profile_defaults_disables_relative_gates_without_baseline() -> None:
    args = argparse.Namespace(
        bench_tier="all",
        bench_runs=None,
        warmup_samples=None,
        max_weighted_wer=None,
        min_command_exact_match=None,
        min_command_normalized_exact_match=None,
        min_command_intent_slot_match=None,
        min_critical_token_recall=None,
        min_punctuation_f1=None,
        min_terminal_punctuation_accuracy=None,
        max_warm_p95_finalize_ms=None,
        max_weighted_wer_delta=None,
        max_command_exact_match_drop=None,
        max_command_normalized_exact_match_drop=None,
        max_command_intent_slot_match_drop=None,
        max_critical_token_recall_drop=None,
        max_punctuation_f1_drop=None,
        max_terminal_punctuation_accuracy_drop=None,
        max_warm_p95_finalize_ms_delta=None,
        calibrate_baseline=False,
        baseline=None,
    )

    apply_profile_defaults(args)

    assert args.max_weighted_wer_delta is None
    assert args.max_command_exact_match_drop is None
    assert args.max_command_normalized_exact_match_drop is None
    assert args.max_command_intent_slot_match_drop is None
    assert args.max_critical_token_recall_drop is None
    assert args.max_punctuation_f1_drop is None
    assert args.max_terminal_punctuation_accuracy_drop is None
    assert args.max_warm_p95_finalize_ms_delta is None


class _DummyStreamSession:
    def __init__(self, parent: _DummyStreamer) -> None:
        self._parent = parent
        self._chunks: list[list[float]] = []

    def feed(self, chunk: list[float]) -> None:
        self._chunks.append(list(chunk))

    def finalize(self) -> str:
        total_samples = sum(len(chunk) for chunk in self._chunks)
        self._parent.finalized_sample_counts.append(total_samples)
        if total_samples in self._parent.finalize_outputs_by_sample_count:
            return self._parent.finalize_outputs_by_sample_count[total_samples]
        if self._parent.finalize_outputs:
            return self._parent.finalize_outputs.pop(0)
        return "ok"


class _DummyStreamer:
    def __init__(
        self,
        *,
        chunk_secs: float,
        finalize_outputs: list[str] | None = None,
        finalize_outputs_by_sample_count: dict[int, str] | None = None,
    ) -> None:
        self.chunk_secs = chunk_secs
        self.finalize_outputs = list(finalize_outputs or [])
        self.finalize_outputs_by_sample_count = dict(finalize_outputs_by_sample_count or {})
        self.finalized_sample_counts: list[int] = []

    def start_session(self, sample_rate: int) -> _DummyStreamSession:
        del sample_rate
        return _DummyStreamSession(self)


def test_transcribe_stream_seal_caps_tail_trimming() -> None:
    streamer = _DummyStreamer(chunk_secs=1.0)
    samples = np.zeros((160,), dtype=np.float32)  # sample_rate=100 => ready=100, tail=60

    hypothesis, _ = transcribe_stream_seal(
        streamer,
        samples=samples,
        sample_rate=100,
        silence_floor_db=-40.0,
        max_tail_trim_secs=0.2,
    )

    # Tail trim is capped to 0.2s (20 samples), so at least 40 tail samples are kept.
    assert hypothesis == "ok"
    assert streamer.finalized_sample_counts == [140]


def test_transcribe_stream_seal_retries_with_full_tail_when_empty() -> None:
    streamer = _DummyStreamer(chunk_secs=1.0, finalize_outputs=["", "ok"])
    samples = np.zeros((160,), dtype=np.float32)  # ready=100, tail=60

    hypothesis, _ = transcribe_stream_seal(
        streamer,
        samples=samples,
        sample_rate=100,
        silence_floor_db=-40.0,
        max_tail_trim_secs=0.2,
    )

    # First finalize uses capped tail (140 samples), retry uses full tail (160 samples).
    assert hypothesis == "ok"
    assert streamer.finalized_sample_counts == [140, 160]


def test_stream_seal_regression_cmd_087_non_empty_for_non_empty_reference() -> None:
    streamer = _DummyStreamer(
        chunk_secs=1.0,
        finalize_outputs_by_sample_count={
            140: "",
            160: _CMD_087_RECOVERED_HYPOTHESIS,
        },
    )
    samples = np.zeros((160,), dtype=np.float32)  # ready=100, tail=60, capped trim keeps 40 tail.

    hypothesis, _ = transcribe_stream_seal(
        streamer,
        samples=samples,
        sample_rate=100,
        silence_floor_db=-40.0,
        max_tail_trim_secs=0.2,
    )

    assert normalize_transcript(_CMD_087_REFERENCE)
    assert hypothesis.strip() != ""
    assert compute_normalized_wer(_CMD_087_REFERENCE, hypothesis) <= 0.15
    assert streamer.finalized_sample_counts == [140, 160]


def test_stream_seal_regression_cmd_073_retains_suffix_with_tail_trim_cap() -> None:
    streamer = _DummyStreamer(
        chunk_secs=1.0,
        finalize_outputs_by_sample_count={
            # No cap can trim all 60 tail samples and lose the suffix clause.
            100: _CMD_073_TRIMMED_HYPOTHESIS,
            # 0.35s cap (35 samples @100Hz) keeps 25 tail samples and preserves suffix.
            125: _CMD_073_RECOVERED_HYPOTHESIS,
        },
    )
    samples = np.zeros((160,), dtype=np.float32)  # ready=100, tail=60

    hypothesis, _ = transcribe_stream_seal(
        streamer,
        samples=samples,
        sample_rate=100,
        silence_floor_db=-40.0,
        max_tail_trim_secs=0.35,
    )

    assert compute_normalized_wer(_CMD_073_REFERENCE, _CMD_073_TRIMMED_HYPOTHESIS) > 0.35
    assert compute_normalized_wer(_CMD_073_REFERENCE, hypothesis) <= 0.10
    assert _CMD_073_SUFFIX in normalize_transcript(hypothesis)
    assert streamer.finalized_sample_counts == [125]
