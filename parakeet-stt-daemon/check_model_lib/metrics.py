"""Transcript normalization, WER computation, command matching, and punctuation metrics."""

from __future__ import annotations

import unicodedata
from typing import Any

from check_model_lib.constants import (
    _COMMAND_ARTICLES,
    _COMMAND_FILLER_TOKENS,
    _COMMAND_INTENT_SYNONYMS,
    _COMMAND_TRANSLATION,
    _PUNCT_TOKEN_RE,
    _TERMINAL_PUNCT_RE,
    _TOKEN_RE,
)


def normalize_transcript(text: str) -> str:
    normalized = unicodedata.normalize("NFKC", text).casefold().replace("_", " ")
    tokens = _TOKEN_RE.findall(normalized)
    return " ".join(tokens)


def _normalize_command_tokens(
    text: str,
    *,
    remove_articles: bool = True,
    drop_fillers: bool = False,
) -> list[str]:
    normalized = unicodedata.normalize("NFKC", text).translate(_COMMAND_TRANSLATION)
    tokens = normalize_transcript(normalized).split()
    if remove_articles:
        tokens = [token for token in tokens if token not in _COMMAND_ARTICLES]
    if drop_fillers:
        tokens = [token for token in tokens if token not in _COMMAND_FILLER_TOKENS]
    return tokens


def normalize_command_text(text: str) -> str:
    return " ".join(_normalize_command_tokens(text, remove_articles=True, drop_fillers=True))


def parse_command_intent_slots(text: str) -> dict[str, Any]:
    tokens = _normalize_command_tokens(text, remove_articles=True)
    if not tokens:
        return {"intent": "", "slots": [], "signature": ""}

    canonical_tokens = [_COMMAND_INTENT_SYNONYMS.get(token, token) for token in tokens]
    intent = canonical_tokens[0]
    for token in canonical_tokens:
        if token in _COMMAND_INTENT_SYNONYMS.values():
            intent = token
            break

    slots: list[str] = []
    seen_slots: set[str] = set()
    for token in canonical_tokens:
        if token == intent or token in _COMMAND_FILLER_TOKENS:
            continue
        if token in seen_slots:
            continue
        slots.append(token)
        seen_slots.add(token)

    signature = f"{intent}|{' '.join(slots)}"
    return {"intent": intent, "slots": slots, "signature": signature}


def _levenshtein_distance(reference_tokens: list[str], hypothesis_tokens: list[str]) -> int:
    rows = len(reference_tokens) + 1
    cols = len(hypothesis_tokens) + 1
    table = [[0] * cols for _ in range(rows)]
    for row in range(rows):
        table[row][0] = row
    for col in range(cols):
        table[0][col] = col
    for row, ref_token in enumerate(reference_tokens, start=1):
        for col, hyp_token in enumerate(hypothesis_tokens, start=1):
            substitution_cost = 0 if ref_token == hyp_token else 1
            table[row][col] = min(
                table[row - 1][col] + 1,
                table[row][col - 1] + 1,
                table[row - 1][col - 1] + substitution_cost,
            )
    return table[-1][-1]


def compute_normalized_wer(reference: str, hypothesis: str) -> float:
    reference_tokens = normalize_transcript(reference).split()
    hypothesis_tokens = normalize_transcript(hypothesis).split()
    if not reference_tokens:
        return 0.0 if not hypothesis_tokens else 1.0
    return _levenshtein_distance(reference_tokens, hypothesis_tokens) / len(reference_tokens)


def _percentile(values: list[float], percentile: float) -> float:
    if not values:
        return 0.0
    sorted_values = sorted(values)
    if len(sorted_values) == 1:
        return float(sorted_values[0])
    rank = (percentile / 100.0) * (len(sorted_values) - 1)
    lower = int(rank)
    upper = min(lower + 1, len(sorted_values) - 1)
    weight = rank - lower
    return sorted_values[lower] * (1.0 - weight) + sorted_values[upper] * weight


def summarize_timings_ms(values: list[float]) -> dict[str, float]:
    if not values:
        return {"avg": 0.0, "p50": 0.0, "p95": 0.0}
    return {
        "avg": sum(values) / len(values),
        "p50": _percentile(values, 50.0),
        "p95": _percentile(values, 95.0),
    }


def _median(values: list[float]) -> float:
    return _percentile(values, 50.0)


def _summarize_domain_wers(rows: list[dict[str, Any]], domain: str) -> tuple[float, int]:
    domain_wers = [float(row["wer"]) for row in rows if row.get("domain") == domain]
    if not domain_wers:
        return 0.0, 0
    return sum(domain_wers) / len(domain_wers), len(domain_wers)


def compute_command_exact_match_rate(rows: list[dict[str, Any]]) -> float:
    return compute_command_match_metrics(rows)["strict_exact_match_rate"]


def compute_command_match_metrics(rows: list[dict[str, Any]]) -> dict[str, float]:
    command_rows = [row for row in rows if row.get("domain") == "command"]
    if not command_rows:
        return {
            "strict_exact_match_rate": 1.0,
            "normalized_exact_match_rate": 1.0,
            "intent_slot_match_rate": 1.0,
        }

    strict_exact_matches = sum(
        1
        for row in command_rows
        if row.get("normalized_reference") == row.get("normalized_hypothesis")
    )
    normalized_exact_matches = sum(
        1
        for row in command_rows
        if row.get("command_normalized_reference", row.get("normalized_reference"))
        == row.get("command_normalized_hypothesis", row.get("normalized_hypothesis"))
    )
    intent_slot_matches = 0
    for row in command_rows:
        reference_signature = row.get("command_reference_signature")
        hypothesis_signature = row.get("command_hypothesis_signature")
        if reference_signature is None:
            reference_signature = parse_command_intent_slots(
                str(row.get("reference", row.get("normalized_reference", "")))
            )["signature"]
        if hypothesis_signature is None:
            hypothesis_signature = parse_command_intent_slots(
                str(row.get("hypothesis", row.get("normalized_hypothesis", "")))
            )["signature"]
        if reference_signature == hypothesis_signature:
            intent_slot_matches += 1
    total = len(command_rows)
    return {
        "strict_exact_match_rate": strict_exact_matches / total,
        "normalized_exact_match_rate": normalized_exact_matches / total,
        "intent_slot_match_rate": intent_slot_matches / total,
    }


def compute_critical_token_recall(rows: list[dict[str, Any]]) -> float:
    total_tokens = 0
    matched_tokens = 0
    for row in rows:
        critical_tokens = tuple(row.get("normalized_critical_tokens", ()))
        if not critical_tokens:
            continue
        hypothesis_tokens = set(str(row.get("normalized_hypothesis", "")).split())
        total_tokens += len(critical_tokens)
        matched_tokens += sum(1 for token in critical_tokens if token in hypothesis_tokens)
    if total_tokens == 0:
        return 1.0
    return matched_tokens / total_tokens


def compute_weighted_wer(rows: list[dict[str, Any]]) -> tuple[float, float, float]:
    command_wer, command_count = _summarize_domain_wers(rows, domain="command")
    dictation_wer, dictation_count = _summarize_domain_wers(rows, domain="dictation")

    if command_count == 0 and dictation_count == 0:
        return 0.0, 0.0, 0.0
    if command_count == 0:
        return dictation_wer, dictation_wer, dictation_wer
    if dictation_count == 0:
        return command_wer, command_wer, command_wer

    weighted_wer = 0.8 * command_wer + 0.2 * dictation_wer
    return weighted_wer, command_wer, dictation_wer


def _extract_punctuation_tokens(text: str) -> list[str]:
    normalized = unicodedata.normalize("NFKC", text)
    return _PUNCT_TOKEN_RE.findall(normalized)


def _extract_terminal_punctuation(text: str) -> str | None:
    normalized = unicodedata.normalize("NFKC", text).strip()
    match = _TERMINAL_PUNCT_RE.search(normalized)
    if match is None:
        return None
    return match.group(1)


def _lcs_length(left: list[str], right: list[str]) -> int:
    if not left or not right:
        return 0
    rows = len(left) + 1
    cols = len(right) + 1
    table = [[0] * cols for _ in range(rows)]
    for row_idx, left_token in enumerate(left, start=1):
        row = table[row_idx]
        prev_row = table[row_idx - 1]
        for col_idx, right_token in enumerate(right, start=1):
            if left_token == right_token:
                row[col_idx] = prev_row[col_idx - 1] + 1
                continue
            row[col_idx] = max(prev_row[col_idx], row[col_idx - 1])
    return table[-1][-1]


def compute_punctuation_metrics(rows: list[dict[str, Any]]) -> dict[str, float]:
    reference_count = 0
    hypothesis_count = 0
    true_positive = 0
    terminal_total = 0
    terminal_match = 0

    for row in rows:
        reference_tokens = _extract_punctuation_tokens(str(row.get("reference", "")))
        hypothesis_tokens = _extract_punctuation_tokens(str(row.get("hypothesis", "")))
        reference_count += len(reference_tokens)
        hypothesis_count += len(hypothesis_tokens)
        true_positive += _lcs_length(reference_tokens, hypothesis_tokens)

        expected_terminal = _extract_terminal_punctuation(str(row.get("reference", "")))
        if expected_terminal is None:
            continue
        terminal_total += 1
        actual_terminal = _extract_terminal_punctuation(str(row.get("hypothesis", "")))
        if actual_terminal == expected_terminal:
            terminal_match += 1

    precision = true_positive / hypothesis_count if hypothesis_count else 1.0
    recall = true_positive / reference_count if reference_count else 1.0
    f1 = (2.0 * precision * recall) / (precision + recall) if (precision + recall) > 0.0 else 0.0
    terminal_accuracy = terminal_match / terminal_total if terminal_total else 1.0

    return {
        "precision": precision,
        "recall": recall,
        "f1": f1,
        "terminal_accuracy": terminal_accuracy,
        "reference_count": float(reference_count),
        "hypothesis_count": float(hypothesis_count),
        "matched_count": float(true_positive),
        "terminal_total": float(terminal_total),
        "terminal_matched": float(terminal_match),
    }
