"""Quick inference probe plus repeatable offline benchmark harness for Parakeet."""

from __future__ import annotations

import argparse
import json
import sys
import tempfile
import time
import wave
from pathlib import Path
from typing import Any

# Bootstrap sibling package imports for both direct execution and importlib loading.
_THIS_DIR = Path(__file__).resolve().parent
if str(_THIS_DIR) not in sys.path:
    sys.path.insert(0, str(_THIS_DIR))

import numpy as np  # noqa: E402
from check_model_lib.constants import (  # noqa: E402
    _ALLOWED_DOMAINS,
    _COMMAND_ARTICLES,
    _COMMAND_FILLER_TOKENS,
    _COMMAND_INTENT_SYNONYMS,
    _COMMAND_TRANSLATION,
    _PUNCT_TOKEN_RE,
    _TERMINAL_PUNCT_RE,
    _TOKEN_RE,
    _TRANSCRIPT_LINE_RE,
    BENCH_AUDIO_DIR,
    DEFAULT_BASELINE_OUTPUT,
    DEFAULT_BENCH_OUTPUT,
    DEFAULT_STREAM_BATCH_SIZE,
    DEFAULT_STREAM_CHUNK_SECS,
    DEFAULT_STREAM_LEFT_CONTEXT_SECS,
    DEFAULT_STREAM_MAX_TAIL_TRIM_SECS,
    DEFAULT_STREAM_RIGHT_CONTEXT_SECS,
    DEFAULT_STREAM_SILENCE_FLOOR_DB,
    HARNESS_DIR,
    PROFILE_DEFAULTS,
    SAMPLE_RATE,
)
from parakeet_stt_daemon.model import (  # noqa: E402
    DEFAULT_MODEL_NAME,
    ParakeetStreamingTranscriber,
    ParakeetTranscriber,
    load_parakeet_model,
)
from check_model_lib.metrics import (  # noqa: E402
    _extract_punctuation_tokens,
    _extract_terminal_punctuation,
    _median,
    compute_command_exact_match_rate,  # noqa: F401 - re-export for test compatibility
    compute_command_match_metrics,
    compute_critical_token_recall,
    compute_normalized_wer,
    compute_punctuation_metrics,
    compute_weighted_wer,
    normalize_command_text,
    normalize_transcript,
    parse_command_intent_slots,
    summarize_timings_ms,
)
from check_model_lib.thresholds import evaluate_regression_thresholds  # noqa: E402, F401
from check_model_lib.corpus import (  # noqa: E402
    _resolve_benchmark_cases,  # noqa: F401 - re-export for test compatibility
    collect_benchmark_cases,  # noqa: F401 - re-export for test compatibility
    parse_benchmark_manifest,  # noqa: F401 - re-export for test compatibility
    parse_benchmark_transcripts,  # noqa: F401 - re-export for test compatibility
)


def _read_wav_samples(path: Path) -> tuple[np.ndarray, int]:
    try:
        import soundfile as sf

        samples, sample_rate = sf.read(path, dtype="float32", always_2d=False)
        sample_array = np.asarray(samples, dtype=np.float32)
    except Exception as err:  # pragma: no cover - minimal fallback
        with wave.open(str(path), "rb") as wf:
            sample_rate = wf.getframerate()
            channels = wf.getnchannels()
            sample_width = wf.getsampwidth()
            raw = wf.readframes(wf.getnframes())
        dtype_map: dict[int, tuple[str, float]] = {
            1: ("<u1", 128.0),
            2: ("<i2", 32768.0),
            4: ("<i4", 2147483648.0),
        }
        if sample_width not in dtype_map:
            raise ValueError(
                f"Unsupported wav sample width ({sample_width} bytes) in {path}"
            ) from err
        dtype, scale = dtype_map[sample_width]
        sample_array = np.frombuffer(raw, dtype=np.dtype(dtype)).astype(np.float32)
        if sample_width == 1:
            sample_array = (sample_array - 128.0) / scale
        else:
            sample_array = sample_array / scale
        if channels > 1:
            sample_array = sample_array.reshape(-1, channels).mean(axis=1)
        return sample_array.reshape(-1), sample_rate

    if sample_array.ndim > 1:
        sample_array = sample_array.mean(axis=1)
    return sample_array.reshape(-1), int(sample_rate)


def _trim_tail_with_rms(
    samples: np.ndarray,
    *,
    sample_rate: int,
    silence_floor_db: float,
    window_ms: int = 50,
) -> np.ndarray:
    if samples.size == 0:
        return samples
    window = max(1, int(sample_rate * window_ms / 1000))
    audio = samples.astype(np.float32, copy=False)
    idx = audio.size
    while idx > 0:
        start = max(0, idx - window)
        window_slice = audio[start:idx]
        rms = np.sqrt(np.mean(window_slice**2))
        db = 20 * np.log10(max(rms, 1e-6))
        if db > silence_floor_db:
            break
        idx = start
    return audio[:idx]


def _split_stream_ready_and_tail(
    samples: np.ndarray, *, sample_rate: int, chunk_secs: float
) -> tuple[list[np.ndarray], np.ndarray]:
    if samples.size == 0:
        return [], np.zeros((0,), dtype=np.float32)
    chunk_size = max(1, int(sample_rate * chunk_secs))
    ready_limit = (samples.size // chunk_size) * chunk_size
    ready_chunks = [
        np.asarray(samples[idx : idx + chunk_size], dtype=np.float32)
        for idx in range(0, ready_limit, chunk_size)
    ]
    tail = np.asarray(samples[ready_limit:], dtype=np.float32)
    return ready_chunks, tail


def _transcribe_stream_seal(
    streamer: ParakeetStreamingTranscriber,
    samples: np.ndarray,
    *,
    sample_rate: int,
    silence_floor_db: float,
    max_tail_trim_secs: float,
) -> tuple[str, float]:
    ready_chunks, tail = _split_stream_ready_and_tail(
        samples, sample_rate=sample_rate, chunk_secs=streamer.chunk_secs
    )
    trimmed_tail = tail
    if tail.size:
        candidate_tail = _trim_tail_with_rms(
            tail,
            sample_rate=sample_rate,
            silence_floor_db=silence_floor_db,
        )
        max_trim_samples = max(0, int(sample_rate * max_tail_trim_secs))
        minimum_keep = max(0, tail.size - max_trim_samples)
        if candidate_tail.size < minimum_keep:
            candidate_tail = tail[:minimum_keep]
        trimmed_tail = candidate_tail

    def _finalize_with_tail(active_tail: np.ndarray) -> str:
        session = streamer.start_session(sample_rate)
        for chunk in ready_chunks:
            session.feed(chunk)
        if active_tail.size:
            session.feed(active_tail)
        return session.finalize()

    infer_start = time.perf_counter()
    hypothesis = _finalize_with_tail(trimmed_tail)
    # Avoid catastrophic empty outputs when tail trimming removed endpoint cues.
    if not hypothesis.strip() and tail.size and trimmed_tail.size < tail.size:
        hypothesis = _finalize_with_tail(tail)
    infer_ms = (time.perf_counter() - infer_start) * 1000.0
    return hypothesis, infer_ms


def _run_benchmark_once(
    *,
    cases: list[BenchmarkCase],
    transcriber: ParakeetTranscriber,
    streaming_transcriber: ParakeetStreamingTranscriber | None,
    bench_runtime: str,
    stream_silence_floor_db: float,
    stream_max_tail_trim_secs: float,
    warmup_samples: int,
    run_index: int,
) -> dict[str, Any]:
    sample_rows: list[dict[str, Any]] = []
    infer_ms_values: list[float] = []
    finalize_ms_values: list[float] = []

    for case in cases:
        finalize_start = time.perf_counter()
        samples, sample_rate = _read_wav_samples(case.audio_path)
        if bench_runtime == "stream-seal":
            if streaming_transcriber is None:
                raise ValueError("streaming transcriber is required for stream-seal runtime")
            hypothesis, infer_ms = _transcribe_stream_seal(
                streaming_transcriber,
                samples,
                sample_rate=sample_rate,
                silence_floor_db=stream_silence_floor_db,
                max_tail_trim_secs=stream_max_tail_trim_secs,
            )
        else:
            infer_start = time.perf_counter()
            hypothesis = transcriber.transcribe_samples(samples, sample_rate=sample_rate)
            infer_ms = (time.perf_counter() - infer_start) * 1000.0
        finalize_ms = (time.perf_counter() - finalize_start) * 1000.0
        normalized_reference = normalize_transcript(case.reference)
        normalized_hypothesis = normalize_transcript(hypothesis)
        command_normalized_reference = (
            normalize_command_text(case.reference) if case.domain == "command" else None
        )
        command_normalized_hypothesis = (
            normalize_command_text(hypothesis) if case.domain == "command" else None
        )
        command_reference_parse = (
            parse_command_intent_slots(case.reference) if case.domain == "command" else None
        )
        command_hypothesis_parse = (
            parse_command_intent_slots(hypothesis) if case.domain == "command" else None
        )
        command_reference_signature = (
            command_reference_parse["signature"] if command_reference_parse is not None else None
        )
        command_hypothesis_signature = (
            command_hypothesis_parse["signature"] if command_hypothesis_parse is not None else None
        )
        command_match_strict = (
            normalized_reference == normalized_hypothesis if case.domain == "command" else None
        )
        command_match_normalized = (
            command_normalized_reference == command_normalized_hypothesis
            if case.domain == "command"
            else None
        )
        command_match_intent_slot = (
            command_reference_signature == command_hypothesis_signature
            if case.domain == "command"
            else None
        )
        wer = compute_normalized_wer(case.reference, hypothesis)
        reference_punctuation = _extract_punctuation_tokens(case.reference)
        hypothesis_punctuation = _extract_punctuation_tokens(hypothesis)
        expected_terminal_punctuation = _extract_terminal_punctuation(case.reference)
        actual_terminal_punctuation = _extract_terminal_punctuation(hypothesis)

        row = {
            "run_index": run_index,
            "runtime": bench_runtime,
            "sample_id": case.sample_id,
            "audio_path": str(case.audio_path),
            "sample_rate": sample_rate,
            "duration_seconds": samples.size / float(sample_rate) if sample_rate > 0 else 0.0,
            "tier": case.tier,
            "domain": case.domain,
            "critical_tokens": list(case.critical_tokens),
            "reference": case.reference,
            "hypothesis": hypothesis,
            "normalized_reference": normalized_reference,
            "normalized_hypothesis": normalized_hypothesis,
            "command_normalized_reference": command_normalized_reference,
            "command_normalized_hypothesis": command_normalized_hypothesis,
            "command_reference_parse": command_reference_parse,
            "command_hypothesis_parse": command_hypothesis_parse,
            "command_reference_signature": command_reference_signature,
            "command_hypothesis_signature": command_hypothesis_signature,
            "command_match_strict": command_match_strict,
            "command_match_normalized": command_match_normalized,
            "command_match_intent_slot": command_match_intent_slot,
            "normalized_critical_tokens": list(case.critical_tokens),
            "reference_punctuation": reference_punctuation,
            "hypothesis_punctuation": hypothesis_punctuation,
            "expected_terminal_punctuation": expected_terminal_punctuation,
            "actual_terminal_punctuation": actual_terminal_punctuation,
            "wer": wer,
            "infer_ms": infer_ms,
            "finalize_ms": finalize_ms,
        }
        sample_rows.append(row)
        infer_ms_values.append(infer_ms)
        finalize_ms_values.append(finalize_ms)

    avg_wer = (
        sum(float(row["wer"]) for row in sample_rows) / len(sample_rows) if sample_rows else 0.0
    )
    infer_summary = summarize_timings_ms(infer_ms_values)
    finalize_summary = summarize_timings_ms(finalize_ms_values)
    weighted_wer, command_wer, dictation_wer = compute_weighted_wer(sample_rows)
    command_match_metrics = compute_command_match_metrics(sample_rows)
    command_exact_match_rate = command_match_metrics["strict_exact_match_rate"]
    command_exact_match_rate_normalized = command_match_metrics["normalized_exact_match_rate"]
    command_intent_slot_match_rate = command_match_metrics["intent_slot_match_rate"]
    critical_token_recall = compute_critical_token_recall(sample_rows)
    punctuation = compute_punctuation_metrics(sample_rows)

    warmup_slice = sample_rows[min(max(warmup_samples, 0), len(sample_rows)) :]
    warm_infer_summary = summarize_timings_ms([float(row["infer_ms"]) for row in warmup_slice])
    warm_finalize_summary = summarize_timings_ms(
        [float(row["finalize_ms"]) for row in warmup_slice]
    )
    cold_start_ms = float(sample_rows[0]["finalize_ms"]) if sample_rows else 0.0

    command_count = sum(1 for row in sample_rows if row["domain"] == "command")
    dictation_count = sum(1 for row in sample_rows if row["domain"] == "dictation")
    aggregate = {
        "avg_wer": avg_wer,
        "weighted_wer": weighted_wer,
        "command_exact_match_rate": command_exact_match_rate,
        "command_exact_match_rate_strict": command_exact_match_rate,
        "command_exact_match_rate_normalized": command_exact_match_rate_normalized,
        "command_intent_slot_match_rate": command_intent_slot_match_rate,
        "command_match": {
            "strict_exact_match_rate": command_exact_match_rate,
            "normalized_exact_match_rate": command_exact_match_rate_normalized,
            "intent_slot_match_rate": command_intent_slot_match_rate,
        },
        "critical_token_recall": critical_token_recall,
        "punctuation": punctuation,
        "punctuation_f1": float(punctuation["f1"]),
        "terminal_punctuation_accuracy": float(punctuation["terminal_accuracy"]),
        "infer_ms": infer_summary,
        "finalize_ms": finalize_summary,
        "warm_infer_ms": warm_infer_summary,
        "warm_finalize_ms": warm_finalize_summary,
        "cold_start_ms": cold_start_ms,
        "by_domain": {
            "command": {"avg_wer": command_wer, "count": command_count},
            "dictation": {"avg_wer": dictation_wer, "count": dictation_count},
        },
    }
    return {"aggregate": aggregate, "samples": sample_rows}


def _medianize_timing_summaries(values: list[dict[str, float]]) -> dict[str, float]:
    if not values:
        return {"avg": 0.0, "p50": 0.0, "p95": 0.0}
    return {
        "avg": _median([item["avg"] for item in values]),
        "p50": _median([item["p50"] for item in values]),
        "p95": _median([item["p95"] for item in values]),
    }


def _aggregate_run_results(run_results: list[dict[str, Any]]) -> dict[str, Any]:
    if not run_results:
        return {
            "aggregation_strategy": "median_of_runs",
            "run_count": 0,
            "avg_wer": 0.0,
            "weighted_wer": 0.0,
            "command_exact_match_rate": 1.0,
            "command_exact_match_rate_strict": 1.0,
            "command_exact_match_rate_normalized": 1.0,
            "command_intent_slot_match_rate": 1.0,
            "command_match": {
                "strict_exact_match_rate": 1.0,
                "normalized_exact_match_rate": 1.0,
                "intent_slot_match_rate": 1.0,
            },
            "critical_token_recall": 1.0,
            "punctuation": {
                "precision": 1.0,
                "recall": 1.0,
                "f1": 1.0,
                "terminal_accuracy": 1.0,
                "reference_count": 0.0,
                "hypothesis_count": 0.0,
                "matched_count": 0.0,
                "terminal_total": 0.0,
                "terminal_matched": 0.0,
            },
            "punctuation_f1": 1.0,
            "terminal_punctuation_accuracy": 1.0,
            "infer_ms": {"avg": 0.0, "p50": 0.0, "p95": 0.0},
            "finalize_ms": {"avg": 0.0, "p50": 0.0, "p95": 0.0},
            "warm_infer_ms": {"avg": 0.0, "p50": 0.0, "p95": 0.0},
            "warm_finalize_ms": {"avg": 0.0, "p50": 0.0, "p95": 0.0},
            "cold_start_ms": 0.0,
            "by_domain": {
                "command": {"avg_wer": 0.0, "count": 0},
                "dictation": {"avg_wer": 0.0, "count": 0},
            },
        }

    run_aggregates = [result["aggregate"] for result in run_results]
    first_domain = run_aggregates[0]["by_domain"]
    punctuation_aggregates = [aggregate["punctuation"] for aggregate in run_aggregates]
    return {
        "aggregation_strategy": "median_of_runs",
        "run_count": len(run_aggregates),
        "avg_wer": _median([aggregate["avg_wer"] for aggregate in run_aggregates]),
        "weighted_wer": _median([aggregate["weighted_wer"] for aggregate in run_aggregates]),
        "command_exact_match_rate": _median(
            [aggregate["command_exact_match_rate"] for aggregate in run_aggregates]
        ),
        "command_exact_match_rate_strict": _median(
            [aggregate["command_exact_match_rate_strict"] for aggregate in run_aggregates]
        ),
        "command_exact_match_rate_normalized": _median(
            [aggregate["command_exact_match_rate_normalized"] for aggregate in run_aggregates]
        ),
        "command_intent_slot_match_rate": _median(
            [aggregate["command_intent_slot_match_rate"] for aggregate in run_aggregates]
        ),
        "command_match": {
            "strict_exact_match_rate": _median(
                [
                    aggregate["command_match"]["strict_exact_match_rate"]
                    for aggregate in run_aggregates
                ]
            ),
            "normalized_exact_match_rate": _median(
                [
                    aggregate["command_match"]["normalized_exact_match_rate"]
                    for aggregate in run_aggregates
                ]
            ),
            "intent_slot_match_rate": _median(
                [
                    aggregate["command_match"]["intent_slot_match_rate"]
                    for aggregate in run_aggregates
                ]
            ),
        },
        "critical_token_recall": _median(
            [aggregate["critical_token_recall"] for aggregate in run_aggregates]
        ),
        "punctuation": {
            "precision": _median([item["precision"] for item in punctuation_aggregates]),
            "recall": _median([item["recall"] for item in punctuation_aggregates]),
            "f1": _median([item["f1"] for item in punctuation_aggregates]),
            "terminal_accuracy": _median(
                [item["terminal_accuracy"] for item in punctuation_aggregates]
            ),
            "reference_count": punctuation_aggregates[0]["reference_count"],
            "hypothesis_count": punctuation_aggregates[0]["hypothesis_count"],
            "matched_count": punctuation_aggregates[0]["matched_count"],
            "terminal_total": punctuation_aggregates[0]["terminal_total"],
            "terminal_matched": punctuation_aggregates[0]["terminal_matched"],
        },
        "punctuation_f1": _median([aggregate["punctuation_f1"] for aggregate in run_aggregates]),
        "terminal_punctuation_accuracy": _median(
            [aggregate["terminal_punctuation_accuracy"] for aggregate in run_aggregates]
        ),
        "infer_ms": _medianize_timing_summaries(
            [aggregate["infer_ms"] for aggregate in run_aggregates]
        ),
        "finalize_ms": _medianize_timing_summaries(
            [aggregate["finalize_ms"] for aggregate in run_aggregates]
        ),
        "warm_infer_ms": _medianize_timing_summaries(
            [aggregate["warm_infer_ms"] for aggregate in run_aggregates]
        ),
        "warm_finalize_ms": _medianize_timing_summaries(
            [aggregate["warm_finalize_ms"] for aggregate in run_aggregates]
        ),
        "cold_start_ms": _median([aggregate["cold_start_ms"] for aggregate in run_aggregates]),
        "by_domain": {
            "command": {
                "avg_wer": _median(
                    [aggregate["by_domain"]["command"]["avg_wer"] for aggregate in run_aggregates]
                ),
                "count": first_domain["command"]["count"],
            },
            "dictation": {
                "avg_wer": _median(
                    [aggregate["by_domain"]["dictation"]["avg_wer"] for aggregate in run_aggregates]
                ),
                "count": first_domain["dictation"]["count"],
            },
        },
    }


def _load_baseline_metrics(path: Path) -> dict[str, float]:
    if not path.exists():
        raise FileNotFoundError(f"Baseline file not found: {path}")
    payload = json.loads(path.read_text(encoding="utf-8"))
    aggregate = (
        payload["aggregate"] if isinstance(payload, dict) and "aggregate" in payload else payload
    )
    if not isinstance(aggregate, dict):
        raise ValueError(f"Invalid baseline payload in {path}")

    metrics: dict[str, float] = {}
    for key in (
        "weighted_wer",
        "command_exact_match_rate",
        "command_exact_match_rate_strict",
        "command_exact_match_rate_normalized",
        "command_intent_slot_match_rate",
        "critical_token_recall",
        "punctuation_f1",
        "terminal_punctuation_accuracy",
    ):
        value = aggregate.get(key)
        if isinstance(value, (int, float)):
            metrics[key] = float(value)

    punctuation = aggregate.get("punctuation")
    if isinstance(punctuation, dict):
        f1 = punctuation.get("f1")
        if isinstance(f1, (int, float)):
            metrics.setdefault("punctuation_f1", float(f1))
        terminal_accuracy = punctuation.get("terminal_accuracy")
        if isinstance(terminal_accuracy, (int, float)):
            metrics.setdefault("terminal_punctuation_accuracy", float(terminal_accuracy))

    warm_finalize_ms = aggregate.get("warm_finalize_ms")
    if isinstance(warm_finalize_ms, dict) and isinstance(warm_finalize_ms.get("p95"), (int, float)):
        metrics["warm_finalize_p95_ms"] = float(warm_finalize_ms["p95"])
    elif isinstance(aggregate.get("warm_finalize_p95_ms"), (int, float)):
        metrics["warm_finalize_p95_ms"] = float(aggregate["warm_finalize_p95_ms"])

    return metrics


def _compute_baseline_comparison(
    baseline_metrics: dict[str, float] | None,
    aggregate: dict[str, Any],
) -> dict[str, Any] | None:
    if not baseline_metrics:
        return None

    current_metrics = {
        "weighted_wer": float(aggregate["weighted_wer"]),
        "command_exact_match_rate": float(aggregate["command_exact_match_rate"]),
        "command_exact_match_rate_strict": float(aggregate["command_exact_match_rate_strict"]),
        "command_exact_match_rate_normalized": float(
            aggregate["command_exact_match_rate_normalized"]
        ),
        "command_intent_slot_match_rate": float(aggregate["command_intent_slot_match_rate"]),
        "critical_token_recall": float(aggregate["critical_token_recall"]),
        "punctuation_f1": float(aggregate["punctuation_f1"]),
        "terminal_punctuation_accuracy": float(aggregate["terminal_punctuation_accuracy"]),
        "warm_finalize_p95_ms": float(aggregate["warm_finalize_ms"]["p95"]),
    }
    comparison: dict[str, Any] = {}
    for key, baseline_value in baseline_metrics.items():
        if key not in current_metrics:
            continue
        current_value = current_metrics[key]
        comparison[key] = {
            "baseline": baseline_value,
            "current": current_value,
            "delta": current_value - baseline_value,
        }
    return comparison if comparison else None


def _write_baseline_output(path: Path, args: argparse.Namespace, aggregate: dict[str, Any]) -> None:
    payload = {
        "version": 1,
        "benchmark": "offline",
        "bench_runtime": args.bench_runtime,
        "model": args.model,
        "requested_device": args.device,
        "bench_tier": args.bench_tier,
        "bench_append_legacy": args.bench_append_legacy,
        "bench_runs": args.bench_runs,
        "warmup_samples": args.warmup_samples,
        "aggregate": aggregate,
    }
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(payload, indent=2, ensure_ascii=False) + "\n", encoding="utf-8")


def _apply_profile_defaults(args: argparse.Namespace) -> None:
    if args.bench_tier is None:
        args.bench_runs = 1 if args.bench_runs is None else args.bench_runs
        args.warmup_samples = 0 if args.warmup_samples is None else args.warmup_samples
        return

    profile = PROFILE_DEFAULTS[args.bench_tier]
    if args.bench_runs is None:
        args.bench_runs = int(profile["bench_runs"])
    if args.warmup_samples is None:
        args.warmup_samples = int(profile["warmup_samples"])

    profile_to_arg = {
        "max_weighted_wer": "max_weighted_wer",
        "min_command_exact_match": "min_command_exact_match",
        "min_command_normalized_exact_match": "min_command_normalized_exact_match",
        "min_command_intent_slot_match": "min_command_intent_slot_match",
        "min_critical_token_recall": "min_critical_token_recall",
        "min_punctuation_f1": "min_punctuation_f1",
        "min_terminal_punctuation_accuracy": "min_terminal_punctuation_accuracy",
        "max_warm_p95_finalize_ms": "max_warm_p95_finalize_ms",
        "max_weighted_wer_delta": "max_weighted_wer_delta",
        "max_command_exact_match_drop": "max_command_exact_match_drop",
        "max_command_normalized_exact_match_drop": "max_command_normalized_exact_match_drop",
        "max_command_intent_slot_match_drop": "max_command_intent_slot_match_drop",
        "max_critical_token_recall_drop": "max_critical_token_recall_drop",
        "max_punctuation_f1_drop": "max_punctuation_f1_drop",
        "max_terminal_punctuation_accuracy_drop": "max_terminal_punctuation_accuracy_drop",
        "max_warm_p95_finalize_ms_delta": "max_warm_p95_finalize_ms_delta",
    }
    for profile_key, arg_name in profile_to_arg.items():
        if getattr(args, arg_name) is None and profile_key in profile:
            setattr(args, arg_name, float(profile[profile_key]))

    if args.calibrate_baseline:
        args.max_weighted_wer_delta = None
        args.max_command_exact_match_drop = None
        args.max_command_normalized_exact_match_drop = None
        args.max_command_intent_slot_match_drop = None
        args.max_critical_token_recall_drop = None
        args.max_punctuation_f1_drop = None
        args.max_terminal_punctuation_accuracy_drop = None
        args.max_warm_p95_finalize_ms_delta = None
    elif args.baseline is None:
        # Keep profile defaults usable before a baseline exists.
        args.max_weighted_wer_delta = None
        args.max_command_exact_match_drop = None
        args.max_command_normalized_exact_match_drop = None
        args.max_command_intent_slot_match_drop = None
        args.max_critical_token_recall_drop = None
        args.max_punctuation_f1_drop = None
        args.max_terminal_punctuation_accuracy_drop = None
        args.max_warm_p95_finalize_ms_delta = None


def run_offline_benchmark(args: argparse.Namespace) -> int:
    _apply_profile_defaults(args)
    if args.bench_runs is None or args.bench_runs <= 0:
        raise ValueError("--bench-runs must be at least 1")
    if args.warmup_samples is None or args.warmup_samples < 0:
        raise ValueError("--warmup-samples must be >= 0")
    if args.bench_runtime == "stream-seal":
        if args.stream_chunk_secs <= 0:
            raise ValueError("--stream-chunk-secs must be > 0")
        if args.stream_right_context_secs < 0:
            raise ValueError("--stream-right-context-secs must be >= 0")
        if args.stream_left_context_secs < 0:
            raise ValueError("--stream-left-context-secs must be >= 0")
        if args.stream_batch_size <= 0:
            raise ValueError("--stream-batch-size must be > 0")
        if args.stream_max_tail_trim_secs < 0:
            raise ValueError("--stream-max-tail-trim-secs must be >= 0")

    bench_dir: Path = args.bench_dir.resolve()
    output_path: Path = (
        args.bench_output.resolve()
        if args.bench_output is not None
        else DEFAULT_BENCH_OUTPUT.resolve()
    )

    transcripts_path = (
        args.bench_transcripts.resolve()
        if args.bench_transcripts is not None
        else (bench_dir / "transcripts.txt")
    )
    manifest_path = args.bench_manifest.resolve() if args.bench_manifest is not None else None
    cases, resolved_manifest_path, appended_transcripts_path = _resolve_benchmark_cases(
        bench_dir=bench_dir,
        bench_tier=args.bench_tier,
        bench_manifest=manifest_path,
        bench_transcripts=transcripts_path,
        bench_append_legacy=args.bench_append_legacy,
    )

    model = load_parakeet_model(args.model, device=args.device)
    transcriber = ParakeetTranscriber(model)
    streaming_transcriber: ParakeetStreamingTranscriber | None = None
    if args.bench_runtime == "stream-seal":
        streaming_transcriber = ParakeetStreamingTranscriber(
            model,
            chunk_secs=args.stream_chunk_secs,
            right_context_secs=args.stream_right_context_secs,
            left_context_secs=args.stream_left_context_secs,
            batch_size=args.stream_batch_size,
        )
    effective_device = str(getattr(model, "_parakeet_effective_device", args.device))

    run_results: list[dict[str, Any]] = []
    for run_index in range(args.bench_runs):
        run_results.append(
            _run_benchmark_once(
                cases=cases,
                transcriber=transcriber,
                streaming_transcriber=streaming_transcriber,
                bench_runtime=args.bench_runtime,
                stream_silence_floor_db=args.stream_silence_floor_db,
                stream_max_tail_trim_secs=args.stream_max_tail_trim_secs,
                warmup_samples=args.warmup_samples,
                run_index=run_index,
            )
        )

    aggregate = _aggregate_run_results(run_results)
    baseline_metrics = _load_baseline_metrics(args.baseline.resolve()) if args.baseline else None
    failures = evaluate_regression_thresholds(
        avg_wer=float(aggregate["avg_wer"]),
        infer_p95_ms=float(aggregate["infer_ms"]["p95"]),
        finalize_p95_ms=float(aggregate["finalize_ms"]["p95"]),
        max_avg_wer=args.max_avg_wer,
        max_p95_infer_ms=args.max_p95_infer_ms,
        max_p95_finalize_ms=args.max_p95_finalize_ms,
        weighted_wer=float(aggregate["weighted_wer"]),
        command_exact_match_rate=float(aggregate["command_exact_match_rate"]),
        command_exact_match_rate_normalized=float(aggregate["command_exact_match_rate_normalized"]),
        command_intent_slot_match_rate=float(aggregate["command_intent_slot_match_rate"]),
        critical_token_recall=float(aggregate["critical_token_recall"]),
        warm_finalize_p95_ms=float(aggregate["warm_finalize_ms"]["p95"]),
        punctuation_f1=float(aggregate["punctuation_f1"]),
        terminal_punctuation_accuracy=float(aggregate["terminal_punctuation_accuracy"]),
        max_weighted_wer=args.max_weighted_wer,
        min_command_exact_match=args.min_command_exact_match,
        min_command_normalized_exact_match=args.min_command_normalized_exact_match,
        min_command_intent_slot_match=args.min_command_intent_slot_match,
        min_critical_token_recall=args.min_critical_token_recall,
        min_punctuation_f1=args.min_punctuation_f1,
        min_terminal_punctuation_accuracy=args.min_terminal_punctuation_accuracy,
        max_warm_p95_finalize_ms=args.max_warm_p95_finalize_ms,
        baseline=baseline_metrics,
        max_weighted_wer_delta=args.max_weighted_wer_delta,
        max_command_exact_match_drop=args.max_command_exact_match_drop,
        max_command_normalized_exact_match_drop=args.max_command_normalized_exact_match_drop,
        max_command_intent_slot_match_drop=args.max_command_intent_slot_match_drop,
        max_critical_token_recall_drop=args.max_critical_token_recall_drop,
        max_punctuation_f1_drop=args.max_punctuation_f1_drop,
        max_terminal_punctuation_accuracy_drop=args.max_terminal_punctuation_accuracy_drop,
        max_warm_p95_finalize_ms_delta=args.max_warm_p95_finalize_ms_delta,
    )

    baseline_comparison = _compute_baseline_comparison(baseline_metrics, aggregate)
    first_run_samples = run_results[0]["samples"] if run_results else []
    runtime_config: dict[str, Any] | None = None
    if args.bench_runtime == "stream-seal":
        runtime_config = {
            "chunk_secs": args.stream_chunk_secs,
            "right_context_secs": args.stream_right_context_secs,
            "left_context_secs": args.stream_left_context_secs,
            "batch_size": args.stream_batch_size,
            "silence_floor_db": args.stream_silence_floor_db,
            "max_tail_trim_secs": args.stream_max_tail_trim_secs,
            "helper_active": bool(
                streaming_transcriber is not None and streaming_transcriber.helper_active
            ),
            "helper_class": (
                getattr(streaming_transcriber, "_helper_class_name", None)
                if streaming_transcriber is not None
                else None
            ),
            "fallback_reason": (
                streaming_transcriber.fallback_reason if streaming_transcriber is not None else None
            ),
        }
    report = {
        "benchmark": "offline",
        "bench_runtime": args.bench_runtime,
        "model": args.model,
        "requested_device": args.device,
        "effective_device": effective_device,
        "bench_dir": str(bench_dir),
        "bench_tier": args.bench_tier,
        "bench_append_legacy": args.bench_append_legacy,
        "bench_runs": args.bench_runs,
        "warmup_samples": args.warmup_samples,
        "manifest_path": str(resolved_manifest_path)
        if resolved_manifest_path is not None
        else None,
        "transcripts_path": str(transcripts_path) if resolved_manifest_path is None else None,
        "appended_transcripts_path": (
            str(appended_transcripts_path) if appended_transcripts_path is not None else None
        ),
        "sample_count": len(first_run_samples),
        "aggregate": aggregate,
        "runtime_config": runtime_config,
        "thresholds": {
            "max_avg_wer": args.max_avg_wer,
            "max_p95_infer_ms": args.max_p95_infer_ms,
            "max_p95_finalize_ms": args.max_p95_finalize_ms,
            "max_weighted_wer": args.max_weighted_wer,
            "min_command_exact_match": args.min_command_exact_match,
            "min_command_normalized_exact_match": args.min_command_normalized_exact_match,
            "min_command_intent_slot_match": args.min_command_intent_slot_match,
            "min_critical_token_recall": args.min_critical_token_recall,
            "min_punctuation_f1": args.min_punctuation_f1,
            "min_terminal_punctuation_accuracy": args.min_terminal_punctuation_accuracy,
            "max_warm_p95_finalize_ms": args.max_warm_p95_finalize_ms,
            "max_weighted_wer_delta": args.max_weighted_wer_delta,
            "max_command_exact_match_drop": args.max_command_exact_match_drop,
            "max_command_normalized_exact_match_drop": args.max_command_normalized_exact_match_drop,
            "max_command_intent_slot_match_drop": args.max_command_intent_slot_match_drop,
            "max_critical_token_recall_drop": args.max_critical_token_recall_drop,
            "max_punctuation_f1_drop": args.max_punctuation_f1_drop,
            "max_terminal_punctuation_accuracy_drop": args.max_terminal_punctuation_accuracy_drop,
            "max_warm_p95_finalize_ms_delta": args.max_warm_p95_finalize_ms_delta,
            "baseline_path": str(args.baseline.resolve()) if args.baseline is not None else None,
        },
        "baseline_comparison": baseline_comparison,
        "regression_gate": {
            "pass": not failures,
            "failures": failures,
        },
        "samples": first_run_samples,
        "runs": run_results,
    }

    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(
        json.dumps(report, indent=2, ensure_ascii=False) + "\n", encoding="utf-8"
    )

    print(
        f"Benchmark completed for {len(first_run_samples)} sample(s) (runtime={args.bench_runtime})"
    )
    if appended_transcripts_path is not None:
        print(f"Manifest mode with appended legacy transcripts: {appended_transcripts_path}")
    print(
        f"Model: {args.model} (requested={args.device}, effective={effective_device}), "
        f"runs={args.bench_runs}, aggregation={aggregate['aggregation_strategy']}"
    )
    if runtime_config is not None:
        print(
            "Stream+seal config: "
            f"chunk={runtime_config['chunk_secs']}, "
            f"right_context={runtime_config['right_context_secs']}, "
            f"left_context={runtime_config['left_context_secs']}, "
            f"batch={runtime_config['batch_size']}, "
            f"max_tail_trim={runtime_config['max_tail_trim_secs']}, "
            f"helper_active={runtime_config['helper_active']}, "
            f"helper_class={runtime_config['helper_class']}, "
            f"fallback_reason={runtime_config['fallback_reason']}"
        )
    print(f"Average WER: {aggregate['avg_wer']:.4f}")
    print(f"Weighted WER: {aggregate['weighted_wer']:.4f}")
    print(f"Command exact-match (strict): {aggregate['command_exact_match_rate_strict']:.4f}")
    print(
        f"Command exact-match (normalized): {aggregate['command_exact_match_rate_normalized']:.4f}"
    )
    print(f"Command intent+slot match: {aggregate['command_intent_slot_match_rate']:.4f}")
    print(f"Critical token recall: {aggregate['critical_token_recall']:.4f}")
    print(
        "Punctuation (precision/recall/F1/terminal): "
        f"{aggregate['punctuation']['precision']:.4f}/"
        f"{aggregate['punctuation']['recall']:.4f}/"
        f"{aggregate['punctuation_f1']:.4f}/"
        f"{aggregate['terminal_punctuation_accuracy']:.4f}"
    )
    print(
        "Infer ms (avg/p50/p95): "
        f"{aggregate['infer_ms']['avg']:.2f}/{aggregate['infer_ms']['p50']:.2f}/"
        f"{aggregate['infer_ms']['p95']:.2f}"
    )
    print(
        "Finalize ms (avg/p50/p95): "
        f"{aggregate['finalize_ms']['avg']:.2f}/{aggregate['finalize_ms']['p50']:.2f}/"
        f"{aggregate['finalize_ms']['p95']:.2f}"
    )
    print(
        "Warm finalize ms (avg/p50/p95): "
        f"{aggregate['warm_finalize_ms']['avg']:.2f}/{aggregate['warm_finalize_ms']['p50']:.2f}/"
        f"{aggregate['warm_finalize_ms']['p95']:.2f}"
    )
    for row in first_run_samples:
        print(
            f" - {row['sample_id']}: domain={row['domain']}, wer={row['wer']:.4f}, "
            f"infer_ms={row['infer_ms']:.2f}, finalize_ms={row['finalize_ms']:.2f}"
        )
    print(f"JSON report written to: {output_path}")

    if args.calibrate_baseline:
        baseline_output = (
            args.baseline_output.resolve()
            if args.baseline_output is not None
            else DEFAULT_BASELINE_OUTPUT
        )
        _write_baseline_output(baseline_output, args, aggregate)
        print(f"Baseline written to: {baseline_output}")

    if failures:
        print("Regression gate: FAILED")
        for failure in failures:
            print(f" - {failure}")
        return 1

    print("Regression gate: PASSED")
    return 0


def generate_sine(duration_secs: float, freq_hz: float, amplitude: float) -> np.ndarray:
    t = np.linspace(0, duration_secs, int(SAMPLE_RATE * duration_secs), endpoint=False)
    return (amplitude * np.sin(2 * np.pi * freq_hz * t)).astype(np.float32)


def write_wav(path: Path, samples: np.ndarray) -> None:
    sf: Any | None
    try:
        import soundfile as sf_mod
    except ImportError:
        sf = None  # pragma: no cover - minimal fallback
    else:
        sf = sf_mod

    if sf is not None:
        try:
            sf.write(path, samples, SAMPLE_RATE)
            return
        except (OSError, RuntimeError, TypeError, ValueError):
            pass  # pragma: no cover - minimal fallback

    # Minimal fallback path for environments without soundfile or where writes fail.
    pcm = (np.clip(samples, -1.0, 1.0) * 32767).astype("<i2")
    with wave.open(str(path), "wb") as wf:
        wf.setnchannels(1)
        wf.setsampwidth(2)
        wf.setframerate(SAMPLE_RATE)
        wf.writeframes(pcm.tobytes())


def split_chunks(samples: np.ndarray, chunk_size: int) -> list[np.ndarray]:
    return [samples[idx : idx + chunk_size] for idx in range(0, samples.size, chunk_size)]


def run_streaming_probe(model, samples: np.ndarray) -> str:
    try:
        streamer = ParakeetStreamingTranscriber(
            model,
            chunk_secs=1.6,
            right_context_secs=2.4,
            left_context_secs=2.0,
            batch_size=4,
        )
    except Exception as exc:  # noqa: BLE001 - probe command must report any helper init failure
        return f"streaming helper init failed: {exc}"

    helper_available = streamer.chunk_helper is not None
    try:
        session = streamer.start_session(SAMPLE_RATE)
        chunk_size = max(1, int(SAMPLE_RATE * streamer.chunk_secs))
        for chunk in split_chunks(samples, chunk_size):
            session.feed(chunk)
        result = session.finalize()
        mode = "streaming helper" if helper_available else "offline fallback"
        return f"{mode} succeeded (transcript='{result}')"
    except Exception as exc:  # noqa: BLE001 - probe command must report any runtime helper failure
        return f"streaming helper blew up during run: {exc}"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Verify Parakeet inference locally.")
    parser.add_argument(
        "--device",
        choices=["cuda", "cpu"],
        default="cuda",
        help="Target device for model inference",
    )
    parser.add_argument(
        "--model",
        default=DEFAULT_MODEL_NAME,
        help="Model name or path (defaults to the TDT 0.6B checkpoint)",
    )
    parser.add_argument(
        "--duration",
        type=float,
        default=2.0,
        help="Length of generated sine wave (seconds)",
    )
    parser.add_argument(
        "--freq",
        type=float,
        default=440.0,
        help="Sine frequency in Hz",
    )
    parser.add_argument(
        "--amplitude",
        type=float,
        default=0.2,
        help="Amplitude for generated sine wave (0.0-1.0)",
    )
    parser.add_argument(
        "--skip-streaming",
        action="store_true",
        help="Do not attempt streaming helper initialisation",
    )
    parser.add_argument(
        "--bench-offline",
        action="store_true",
        help="Run repeatable offline benchmark harness over bench_audio",
    )
    parser.add_argument(
        "--bench-runtime",
        choices=["offline", "stream-seal"],
        default="offline",
        help=(
            "Benchmark transcription runtime path: "
            "'offline' uses direct in-memory transcribe; "
            "'stream-seal' simulates daemon stream+seal finalize path."
        ),
    )
    parser.add_argument(
        "--bench-dir",
        type=Path,
        default=BENCH_AUDIO_DIR,
        help="Directory containing benchmark files",
    )
    parser.add_argument(
        "--bench-manifest",
        type=Path,
        default=None,
        help="Optional JSONL manifest defining benchmark cases",
    )
    parser.add_argument(
        "--bench-tier",
        choices=sorted(PROFILE_DEFAULTS),
        default=None,
        help=(
            "Apply profile defaults. With manifest mode, values other than 'all' also filter "
            "rows by matching manifest tier."
        ),
    )
    parser.add_argument(
        "--bench-append-legacy",
        action="store_true",
        help=(
            "When --bench-manifest is set, append legacy numbered transcript/audio samples "
            "from --bench-transcripts (or <bench-dir>/transcripts.txt)."
        ),
    )
    parser.add_argument(
        "--bench-transcripts",
        type=Path,
        default=None,
        help=(
            "Override path to benchmark transcripts file (legacy mode; "
            "defaults to <bench-dir>/transcripts.txt)"
        ),
    )
    parser.add_argument(
        "--bench-output",
        type=Path,
        default=None,
        help=(
            "Path for benchmark JSON output "
            f"(defaults to {DEFAULT_BENCH_OUTPUT.relative_to(HARNESS_DIR)})"
        ),
    )
    parser.add_argument(
        "--bench-runs",
        type=int,
        default=None,
        help="Number of benchmark repeats to run (median aggregate is reported)",
    )
    parser.add_argument(
        "--warmup-samples",
        type=int,
        default=None,
        help="Exclude first N samples per run from warm latency gates",
    )
    parser.add_argument(
        "--stream-chunk-secs",
        type=float,
        default=DEFAULT_STREAM_CHUNK_SECS,
        help="Chunk size (seconds) used when --bench-runtime=stream-seal",
    )
    parser.add_argument(
        "--stream-right-context-secs",
        type=float,
        default=DEFAULT_STREAM_RIGHT_CONTEXT_SECS,
        help="Right context seconds used when --bench-runtime=stream-seal",
    )
    parser.add_argument(
        "--stream-left-context-secs",
        type=float,
        default=DEFAULT_STREAM_LEFT_CONTEXT_SECS,
        help="Left context seconds used when --bench-runtime=stream-seal",
    )
    parser.add_argument(
        "--stream-batch-size",
        type=int,
        default=DEFAULT_STREAM_BATCH_SIZE,
        help="Streaming helper batch size when --bench-runtime=stream-seal",
    )
    parser.add_argument(
        "--stream-silence-floor-db",
        type=float,
        default=DEFAULT_STREAM_SILENCE_FLOOR_DB,
        help="Tail-trim silence floor (dB) for stream-seal benchmarking",
    )
    parser.add_argument(
        "--stream-max-tail-trim-secs",
        type=float,
        default=DEFAULT_STREAM_MAX_TAIL_TRIM_SECS,
        help=(
            "Maximum trailing-tail trim (seconds) during stream-seal simulation; "
            "caps aggressive tail removal."
        ),
    )
    parser.add_argument(
        "--baseline",
        type=Path,
        default=None,
        help="Path to baseline JSON for relative regression checks",
    )
    parser.add_argument(
        "--calibrate-baseline",
        action="store_true",
        help="Write a baseline snapshot from current aggregate metrics",
    )
    parser.add_argument(
        "--baseline-output",
        type=Path,
        default=None,
        help=(
            "Path to write baseline JSON when --calibrate-baseline is set "
            f"(defaults to {DEFAULT_BASELINE_OUTPUT.relative_to(HARNESS_DIR)})"
        ),
    )
    parser.add_argument(
        "--max-avg-wer",
        type=float,
        default=None,
        help="Fail benchmark when aggregate avg WER exceeds this threshold",
    )
    parser.add_argument(
        "--max-p95-infer-ms",
        type=float,
        default=None,
        help="Fail benchmark when infer p95 (ms) exceeds this threshold",
    )
    parser.add_argument(
        "--max-p95-finalize-ms",
        type=float,
        default=None,
        help="Fail benchmark when finalize p95 (ms) exceeds this threshold",
    )
    parser.add_argument(
        "--max-weighted-wer",
        type=float,
        default=None,
        help="Fail benchmark when weighted WER exceeds this threshold",
    )
    parser.add_argument(
        "--min-command-exact-match",
        type=float,
        default=None,
        help="Fail benchmark when command exact-match ratio falls below this threshold",
    )
    parser.add_argument(
        "--min-command-normalized-exact-match",
        type=float,
        default=None,
        help="Fail benchmark when normalized command exact-match ratio falls below this threshold",
    )
    parser.add_argument(
        "--min-command-intent-slot-match",
        type=float,
        default=None,
        help="Fail benchmark when command intent+slot match ratio falls below this threshold",
    )
    parser.add_argument(
        "--min-critical-token-recall",
        type=float,
        default=None,
        help="Fail benchmark when critical token recall falls below this threshold",
    )
    parser.add_argument(
        "--min-punctuation-f1",
        type=float,
        default=None,
        help="Fail benchmark when punctuation F1 falls below this threshold",
    )
    parser.add_argument(
        "--min-terminal-punctuation-accuracy",
        type=float,
        default=None,
        help="Fail benchmark when terminal punctuation accuracy falls below this threshold",
    )
    parser.add_argument(
        "--max-warm-p95-finalize-ms",
        type=float,
        default=None,
        help="Fail benchmark when warm finalize p95 (ms) exceeds this threshold",
    )
    parser.add_argument(
        "--max-weighted-wer-delta",
        type=float,
        default=None,
        help="Relative gate: weighted WER may not exceed baseline + delta",
    )
    parser.add_argument(
        "--max-command-exact-match-drop",
        type=float,
        default=None,
        help="Relative gate: command exact-match may not drop by more than this amount",
    )
    parser.add_argument(
        "--max-command-normalized-exact-match-drop",
        type=float,
        default=None,
        help=(
            "Relative gate: normalized command exact-match may not drop by more than this amount"
        ),
    )
    parser.add_argument(
        "--max-command-intent-slot-match-drop",
        type=float,
        default=None,
        help="Relative gate: command intent+slot match may not drop by more than this amount",
    )
    parser.add_argument(
        "--max-critical-token-recall-drop",
        type=float,
        default=None,
        help="Relative gate: critical token recall may not drop by more than this amount",
    )
    parser.add_argument(
        "--max-punctuation-f1-drop",
        type=float,
        default=None,
        help="Relative gate: punctuation F1 may not drop by more than this amount",
    )
    parser.add_argument(
        "--max-terminal-punctuation-accuracy-drop",
        type=float,
        default=None,
        help="Relative gate: terminal punctuation accuracy may not drop by more than this amount",
    )
    parser.add_argument(
        "--max-warm-p95-finalize-ms-delta",
        type=float,
        default=None,
        help="Relative gate: warm finalize p95 may not exceed baseline + delta",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.bench_offline:
        return run_offline_benchmark(args)

    samples = generate_sine(args.duration, args.freq, args.amplitude)
    with tempfile.NamedTemporaryFile(suffix=".wav", delete=False) as tmp:
        wav_path = Path(tmp.name)

    try:
        write_wav(wav_path, samples)
        model = load_parakeet_model(args.model, device=args.device)
        transcriber = ParakeetTranscriber(model)

        offline_text = transcriber.transcribe_wav(str(wav_path))
        print(f"Offline transcription: '{offline_text}'")
        if args.skip_streaming:
            print("Streaming probe skipped by flag")
            return 0

        streaming_status = run_streaming_probe(model, samples)
        print(streaming_status)
    finally:
        wav_path.unlink(missing_ok=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
