"""Benchmark execution: single-run, multi-run offline benchmark, and streaming probe."""

from __future__ import annotations

import argparse
import json
import time
from pathlib import Path
from typing import Any

import numpy as np

from check_model_lib.constants import (
    DEFAULT_BASELINE_OUTPUT,
    DEFAULT_BENCH_OUTPUT,
    SAMPLE_RATE,
)
from check_model_lib.corpus import BenchmarkCase, _resolve_benchmark_cases
from check_model_lib.metrics import (
    _extract_punctuation_tokens,
    _extract_terminal_punctuation,
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
from check_model_lib.reporting import (
    _aggregate_run_results,
    _compute_baseline_comparison,
    _load_baseline_metrics,
    _write_baseline_output,
)
from check_model_lib.runtime import (
    _read_wav_samples,
    _transcribe_stream_seal,
    split_chunks,
)
from check_model_lib.thresholds import evaluate_regression_thresholds
from parakeet_stt_daemon.model import (
    ParakeetStreamingTranscriber,
    ParakeetTranscriber,
    load_parakeet_model,
)


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


def run_offline_benchmark(args: argparse.Namespace) -> int:
    from check_model_lib.cli import _apply_profile_defaults

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
