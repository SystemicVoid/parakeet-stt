"""Result aggregation, baseline loading, comparison, and output writing for the eval harness."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any

from check_model_lib.metrics import _median


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
