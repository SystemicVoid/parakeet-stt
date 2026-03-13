"""CLI argument parsing, profile defaults, and main entrypoint for the eval harness."""

from __future__ import annotations

import argparse
import tempfile
from pathlib import Path

from parakeet_stt_daemon.model import (
    DEFAULT_MODEL_NAME,
    ParakeetTranscriber,
    load_parakeet_model,
)

from check_model_lib.constants import (
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
)
from check_model_lib.runner import run_offline_benchmark, run_streaming_probe
from check_model_lib.runtime import generate_sine, write_wav


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
    relative_threshold_args = (
        "max_weighted_wer_delta",
        "max_command_exact_match_drop",
        "max_command_normalized_exact_match_drop",
        "max_command_intent_slot_match_drop",
        "max_critical_token_recall_drop",
        "max_punctuation_f1_drop",
        "max_terminal_punctuation_accuracy_drop",
        "max_warm_p95_finalize_ms_delta",
    )
    profile_injected: set[str] = set()
    for profile_key, arg_name in profile_to_arg.items():
        if getattr(args, arg_name) is None and profile_key in profile:
            setattr(args, arg_name, float(profile[profile_key]))
            profile_injected.add(arg_name)

    if args.calibrate_baseline:
        for arg_name in relative_threshold_args:
            setattr(args, arg_name, None)
    elif args.baseline is None:
        # Keep profile defaults usable before a baseline exists.
        for arg_name in relative_threshold_args:
            if arg_name in profile_injected:
                setattr(args, arg_name, None)


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
