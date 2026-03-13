"""Constants, regex patterns, profile defaults, and path configuration for the eval harness."""

from __future__ import annotations

import re
from pathlib import Path

HARNESS_DIR = Path(__file__).resolve().parent.parent

SAMPLE_RATE = 16_000
BENCH_AUDIO_DIR = HARNESS_DIR / "bench_audio"
DEFAULT_BENCH_OUTPUT = BENCH_AUDIO_DIR / "offline_benchmark_results.json"
DEFAULT_BASELINE_OUTPUT = BENCH_AUDIO_DIR / "offline_benchmark_baseline.json"
_TRANSCRIPT_LINE_RE = re.compile(r"^\s*(?P<index>\d+)\.\s*(?P<text>.+?)\s*$")
_TOKEN_RE = re.compile(r"\w+", flags=re.UNICODE)
_PUNCT_TOKEN_RE = re.compile(r"[.,?!;:]", flags=re.UNICODE)
_TERMINAL_PUNCT_RE = re.compile(r"([.?!])\s*$", flags=re.UNICODE)
_ALLOWED_DOMAINS = {"command", "dictation"}

DEFAULT_STREAM_CHUNK_SECS = 2.4
DEFAULT_STREAM_RIGHT_CONTEXT_SECS = 1.6
DEFAULT_STREAM_LEFT_CONTEXT_SECS = 10.0
DEFAULT_STREAM_BATCH_SIZE = 32
DEFAULT_STREAM_SILENCE_FLOOR_DB = -40.0
DEFAULT_STREAM_MAX_TAIL_TRIM_SECS = 0.35

_COMMAND_ARTICLES = frozenset({"a", "an", "the"})
_COMMAND_FILLER_TOKENS = frozenset(
    {
        "and",
        "can",
        "could",
        "for",
        "i",
        "like",
        "me",
        "my",
        "of",
        "on",
        "or",
        "please",
        "then",
        "to",
        "uh",
        "um",
        "we",
        "with",
        "you",
    }
)
_COMMAND_INTENT_SYNONYMS = {
    "begin": "start",
    "browse": "open",
    "build": "run",
    "change": "edit",
    "create": "make",
    "delete": "remove",
    "execute": "run",
    "find": "search",
    "launch": "open",
    "modify": "edit",
    "new": "make",
    "open": "open",
    "remove": "remove",
    "rename": "move",
    "run": "run",
    "search": "search",
    "start": "start",
    "update": "edit",
}

_COMMAND_TRANSLATION = str.maketrans(
    {
        "\u201c": '"',
        "\u201d": '"',
        "\u2018": "'",
        "\u2019": "'",
        "\u2014": "-",
        "\u2013": "-",
        "\u2011": "-",
        "\u2010": "-",
    }
)

PROFILE_DEFAULTS: dict[str, dict[str, float | int]] = {
    "all": {
        "bench_runs": 2,
        "warmup_samples": 1,
        "max_weighted_wer": 0.20,
        "min_command_exact_match": 0.70,
        "min_critical_token_recall": 0.94,
        "min_punctuation_f1": 0.70,
        "min_terminal_punctuation_accuracy": 0.85,
        "max_warm_p95_finalize_ms": 180.0,
        "max_weighted_wer_delta": 0.03,
        "max_command_exact_match_drop": 0.05,
        "max_critical_token_recall_drop": 0.03,
        "max_punctuation_f1_drop": 0.08,
        "max_terminal_punctuation_accuracy_drop": 0.08,
        "max_warm_p95_finalize_ms_delta": 40.0,
    },
    "smoke": {
        "bench_runs": 1,
        "warmup_samples": 1,
        "max_weighted_wer": 0.28,
        "min_command_exact_match": 0.60,
        "min_critical_token_recall": 0.90,
        "max_warm_p95_finalize_ms": 240.0,
    },
    "daily": {
        "bench_runs": 2,
        "warmup_samples": 1,
        "max_weighted_wer": 0.20,
        "min_command_exact_match": 0.70,
        "min_critical_token_recall": 0.94,
        "max_warm_p95_finalize_ms": 180.0,
        "max_weighted_wer_delta": 0.03,
        "max_command_exact_match_drop": 0.05,
        "max_critical_token_recall_drop": 0.03,
        "max_warm_p95_finalize_ms_delta": 40.0,
    },
    "weekly": {
        "bench_runs": 3,
        "warmup_samples": 1,
        "max_weighted_wer": 0.18,
        "min_command_exact_match": 0.75,
        "min_critical_token_recall": 0.95,
        "max_warm_p95_finalize_ms": 160.0,
        "max_weighted_wer_delta": 0.02,
        "max_command_exact_match_drop": 0.04,
        "max_critical_token_recall_drop": 0.02,
        "max_warm_p95_finalize_ms_delta": 30.0,
    },
}
