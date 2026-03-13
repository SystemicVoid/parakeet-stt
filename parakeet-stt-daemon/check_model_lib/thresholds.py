"""Regression threshold evaluation for the eval harness."""

from __future__ import annotations


def _metric_from_baseline(baseline: dict[str, float] | None, key: str) -> float | None:
    if baseline is None:
        return None
    return baseline.get(key)


def evaluate_regression_thresholds(
    *,
    avg_wer: float,
    infer_p95_ms: float,
    finalize_p95_ms: float,
    max_avg_wer: float | None,
    max_p95_infer_ms: float | None,
    max_p95_finalize_ms: float | None,
    weighted_wer: float | None = None,
    command_exact_match_rate: float | None = None,
    command_exact_match_rate_normalized: float | None = None,
    command_intent_slot_match_rate: float | None = None,
    critical_token_recall: float | None = None,
    warm_finalize_p95_ms: float | None = None,
    max_weighted_wer: float | None = None,
    min_command_exact_match: float | None = None,
    min_command_normalized_exact_match: float | None = None,
    min_command_intent_slot_match: float | None = None,
    min_critical_token_recall: float | None = None,
    max_warm_p95_finalize_ms: float | None = None,
    baseline: dict[str, float] | None = None,
    max_weighted_wer_delta: float | None = None,
    max_command_exact_match_drop: float | None = None,
    max_command_normalized_exact_match_drop: float | None = None,
    max_command_intent_slot_match_drop: float | None = None,
    max_critical_token_recall_drop: float | None = None,
    max_warm_p95_finalize_ms_delta: float | None = None,
    punctuation_f1: float | None = None,
    terminal_punctuation_accuracy: float | None = None,
    min_punctuation_f1: float | None = None,
    min_terminal_punctuation_accuracy: float | None = None,
    max_punctuation_f1_drop: float | None = None,
    max_terminal_punctuation_accuracy_drop: float | None = None,
) -> list[str]:
    failures: list[str] = []
    if max_avg_wer is not None and avg_wer > max_avg_wer:
        failures.append(f"avg_wer {avg_wer:.4f} exceeds threshold {max_avg_wer:.4f}")
    if max_p95_infer_ms is not None and infer_p95_ms > max_p95_infer_ms:
        failures.append(f"infer_p95_ms {infer_p95_ms:.2f} exceeds threshold {max_p95_infer_ms:.2f}")
    if max_p95_finalize_ms is not None and finalize_p95_ms > max_p95_finalize_ms:
        failures.append(
            f"finalize_p95_ms {finalize_p95_ms:.2f} exceeds threshold {max_p95_finalize_ms:.2f}"
        )

    if (
        max_weighted_wer is not None
        and weighted_wer is not None
        and weighted_wer > max_weighted_wer
    ):
        failures.append(f"weighted_wer {weighted_wer:.4f} exceeds threshold {max_weighted_wer:.4f}")
    if (
        min_command_exact_match is not None
        and command_exact_match_rate is not None
        and command_exact_match_rate < min_command_exact_match
    ):
        failures.append(
            "command_exact_match_rate "
            f"{command_exact_match_rate:.4f} below threshold {min_command_exact_match:.4f}"
        )
    if (
        min_critical_token_recall is not None
        and critical_token_recall is not None
        and critical_token_recall < min_critical_token_recall
    ):
        failures.append(
            f"critical_token_recall {critical_token_recall:.4f} below threshold "
            f"{min_critical_token_recall:.4f}"
        )
    if (
        min_command_normalized_exact_match is not None
        and command_exact_match_rate_normalized is not None
        and command_exact_match_rate_normalized < min_command_normalized_exact_match
    ):
        failures.append(
            "command_exact_match_rate_normalized "
            f"{command_exact_match_rate_normalized:.4f} below threshold "
            f"{min_command_normalized_exact_match:.4f}"
        )
    if (
        min_command_intent_slot_match is not None
        and command_intent_slot_match_rate is not None
        and command_intent_slot_match_rate < min_command_intent_slot_match
    ):
        failures.append(
            "command_intent_slot_match_rate "
            f"{command_intent_slot_match_rate:.4f} below threshold "
            f"{min_command_intent_slot_match:.4f}"
        )
    if (
        max_warm_p95_finalize_ms is not None
        and warm_finalize_p95_ms is not None
        and warm_finalize_p95_ms > max_warm_p95_finalize_ms
    ):
        failures.append(
            f"warm_finalize_p95_ms {warm_finalize_p95_ms:.2f} exceeds threshold "
            f"{max_warm_p95_finalize_ms:.2f}"
        )
    if (
        min_punctuation_f1 is not None
        and punctuation_f1 is not None
        and punctuation_f1 < min_punctuation_f1
    ):
        failures.append(
            f"punctuation_f1 {punctuation_f1:.4f} below threshold {min_punctuation_f1:.4f}"
        )
    if (
        min_terminal_punctuation_accuracy is not None
        and terminal_punctuation_accuracy is not None
        and terminal_punctuation_accuracy < min_terminal_punctuation_accuracy
    ):
        failures.append(
            "terminal_punctuation_accuracy "
            f"{terminal_punctuation_accuracy:.4f} below threshold "
            f"{min_terminal_punctuation_accuracy:.4f}"
        )

    if max_weighted_wer_delta is not None:
        baseline_weighted_wer = _metric_from_baseline(baseline, "weighted_wer")
        if baseline_weighted_wer is None:
            failures.append("baseline weighted_wer is required for weighted_wer_delta gating")
        elif (
            weighted_wer is not None
            and weighted_wer > baseline_weighted_wer + max_weighted_wer_delta
        ):
            failures.append(
                f"weighted_wer {weighted_wer:.4f} exceeds baseline+delta "
                f"{baseline_weighted_wer + max_weighted_wer_delta:.4f}"
            )

    if max_command_exact_match_drop is not None:
        baseline_command_match = _metric_from_baseline(baseline, "command_exact_match_rate")
        if baseline_command_match is None:
            failures.append(
                "baseline command_exact_match_rate is required for command_exact_match_drop gating"
            )
        elif (
            command_exact_match_rate is not None
            and command_exact_match_rate < baseline_command_match - max_command_exact_match_drop
        ):
            failures.append(
                f"command_exact_match_rate {command_exact_match_rate:.4f} below baseline-delta "
                f"{baseline_command_match - max_command_exact_match_drop:.4f}"
            )

    if max_command_normalized_exact_match_drop is not None:
        baseline_normalized_match = _metric_from_baseline(
            baseline, "command_exact_match_rate_normalized"
        )
        if baseline_normalized_match is None:
            failures.append(
                "baseline command_exact_match_rate_normalized is required for "
                "command_normalized_exact_match_drop gating"
            )
        elif (
            command_exact_match_rate_normalized is not None
            and command_exact_match_rate_normalized
            < baseline_normalized_match - max_command_normalized_exact_match_drop
        ):
            failures.append(
                "command_exact_match_rate_normalized "
                f"{command_exact_match_rate_normalized:.4f} below baseline-delta "
                f"{baseline_normalized_match - max_command_normalized_exact_match_drop:.4f}"
            )

    if max_command_intent_slot_match_drop is not None:
        baseline_intent_slot_match = _metric_from_baseline(
            baseline, "command_intent_slot_match_rate"
        )
        if baseline_intent_slot_match is None:
            failures.append(
                "baseline command_intent_slot_match_rate is required for "
                "command_intent_slot_match_drop gating"
            )
        elif (
            command_intent_slot_match_rate is not None
            and command_intent_slot_match_rate
            < baseline_intent_slot_match - max_command_intent_slot_match_drop
        ):
            failures.append(
                "command_intent_slot_match_rate "
                f"{command_intent_slot_match_rate:.4f} below baseline-delta "
                f"{baseline_intent_slot_match - max_command_intent_slot_match_drop:.4f}"
            )

    if max_critical_token_recall_drop is not None:
        baseline_critical_recall = _metric_from_baseline(baseline, "critical_token_recall")
        if baseline_critical_recall is None:
            failures.append(
                "baseline critical_token_recall is required for critical_token_recall_drop gating"
            )
        elif (
            critical_token_recall is not None
            and critical_token_recall < baseline_critical_recall - max_critical_token_recall_drop
        ):
            failures.append(
                f"critical_token_recall {critical_token_recall:.4f} below baseline-delta "
                f"{baseline_critical_recall - max_critical_token_recall_drop:.4f}"
            )

    if max_warm_p95_finalize_ms_delta is not None:
        baseline_warm_finalize = _metric_from_baseline(baseline, "warm_finalize_p95_ms")
        if baseline_warm_finalize is None:
            failures.append(
                "baseline warm_finalize_p95_ms is required for warm_p95_finalize_ms_delta gating"
            )
        elif (
            warm_finalize_p95_ms is not None
            and warm_finalize_p95_ms > baseline_warm_finalize + max_warm_p95_finalize_ms_delta
        ):
            failures.append(
                f"warm_finalize_p95_ms {warm_finalize_p95_ms:.2f} exceeds baseline+delta "
                f"{baseline_warm_finalize + max_warm_p95_finalize_ms_delta:.2f}"
            )

    if max_punctuation_f1_drop is not None:
        baseline_punctuation_f1 = _metric_from_baseline(baseline, "punctuation_f1")
        if baseline_punctuation_f1 is None:
            failures.append("baseline punctuation_f1 is required for punctuation_f1_drop gating")
        elif (
            punctuation_f1 is not None
            and punctuation_f1 < baseline_punctuation_f1 - max_punctuation_f1_drop
        ):
            failures.append(
                f"punctuation_f1 {punctuation_f1:.4f} below baseline-delta "
                f"{baseline_punctuation_f1 - max_punctuation_f1_drop:.4f}"
            )

    if max_terminal_punctuation_accuracy_drop is not None:
        baseline_terminal_accuracy = _metric_from_baseline(
            baseline, "terminal_punctuation_accuracy"
        )
        if baseline_terminal_accuracy is None:
            failures.append(
                "baseline terminal_punctuation_accuracy is required for "
                "terminal_punctuation_accuracy_drop gating"
            )
        elif (
            terminal_punctuation_accuracy is not None
            and terminal_punctuation_accuracy
            < baseline_terminal_accuracy - max_terminal_punctuation_accuracy_drop
        ):
            failures.append(
                "terminal_punctuation_accuracy "
                f"{terminal_punctuation_accuracy:.4f} below baseline-delta "
                f"{baseline_terminal_accuracy - max_terminal_punctuation_accuracy_drop:.4f}"
            )

    return failures
