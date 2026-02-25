"""Quick inference probe plus repeatable offline benchmark harness for Parakeet."""

from __future__ import annotations

import argparse
import json
import re
import tempfile
import time
import unicodedata
import wave
from dataclasses import dataclass
from pathlib import Path
from typing import Any

import numpy as np

from parakeet_stt_daemon.model import (
    DEFAULT_MODEL_NAME,
    ParakeetStreamingTranscriber,
    ParakeetTranscriber,
    load_parakeet_model,
)

SAMPLE_RATE = 16_000
BENCH_AUDIO_DIR = Path(__file__).resolve().parent / "bench_audio"
DEFAULT_BENCH_OUTPUT = BENCH_AUDIO_DIR / "offline_benchmark_results.json"
_TRANSCRIPT_LINE_RE = re.compile(r"^\s*(?P<index>\d+)\.\s*(?P<text>.+?)\s*$")
_TOKEN_RE = re.compile(r"\w+", flags=re.UNICODE)


@dataclass(frozen=True)
class BenchmarkCase:
    sample_id: str
    audio_path: Path
    reference: str


def _strip_wrapping_quotes(text: str) -> str:
    stripped = text.strip()
    quote_chars = "\"'“”‘’"
    if len(stripped) >= 2 and stripped[0] in quote_chars and stripped[-1] in quote_chars:
        return stripped[1:-1].strip()
    return stripped


def parse_benchmark_transcripts(path: Path) -> dict[str, str]:
    if not path.exists():
        raise FileNotFoundError(f"Benchmark transcript file not found: {path}")

    parsed: dict[str, str] = {}
    for line_no, raw_line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        line = raw_line.strip()
        if not line:
            continue
        match = _TRANSCRIPT_LINE_RE.match(line)
        if match is None:
            raise ValueError(f"Invalid transcript line at {path}:{line_no}: {line}")
        sample_id = f"sample_{int(match.group('index')):02d}"
        reference = _strip_wrapping_quotes(match.group("text"))
        if not reference:
            raise ValueError(f"Empty transcript text at {path}:{line_no}")
        if sample_id in parsed:
            raise ValueError(f"Duplicate transcript entry for {sample_id} in {path}")
        parsed[sample_id] = reference

    if not parsed:
        raise ValueError(f"No benchmark transcripts found in {path}")
    return parsed


def collect_benchmark_cases(bench_dir: Path, transcripts: dict[str, str]) -> list[BenchmarkCase]:
    audio_paths = sorted(bench_dir.glob("sample_*.wav"))
    if not audio_paths:
        raise ValueError(f"No benchmark wav files found in {bench_dir}")

    cases: list[BenchmarkCase] = []
    seen: set[str] = set()
    for audio_path in audio_paths:
        sample_id = audio_path.stem
        seen.add(sample_id)
        if sample_id not in transcripts:
            raise ValueError(f"Missing transcript entry for {sample_id}")
        cases.append(
            BenchmarkCase(
                sample_id=sample_id,
                audio_path=audio_path,
                reference=transcripts[sample_id],
            )
        )

    missing_audio = sorted(set(transcripts) - seen)
    if missing_audio:
        formatted = ", ".join(missing_audio)
        raise ValueError(f"Transcript entries missing matching audio files: {formatted}")

    return cases


def normalize_transcript(text: str) -> str:
    normalized = unicodedata.normalize("NFKC", text).casefold().replace("_", " ")
    tokens = _TOKEN_RE.findall(normalized)
    return " ".join(tokens)


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


def evaluate_regression_thresholds(
    *,
    avg_wer: float,
    infer_p95_ms: float,
    finalize_p95_ms: float,
    max_avg_wer: float | None,
    max_p95_infer_ms: float | None,
    max_p95_finalize_ms: float | None,
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
    return failures


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


def run_offline_benchmark(args: argparse.Namespace) -> int:
    bench_dir: Path = args.bench_dir.resolve()
    transcripts_path: Path = (
        args.bench_transcripts.resolve()
        if args.bench_transcripts is not None
        else (bench_dir / "transcripts.txt")
    )
    output_path: Path = (
        args.bench_output.resolve()
        if args.bench_output is not None
        else DEFAULT_BENCH_OUTPUT.resolve()
    )

    transcripts = parse_benchmark_transcripts(transcripts_path)
    cases = collect_benchmark_cases(bench_dir, transcripts)

    model = load_parakeet_model(args.model, device=args.device)
    transcriber = ParakeetTranscriber(model)
    effective_device = str(getattr(model, "_parakeet_effective_device", args.device))

    sample_rows: list[dict[str, Any]] = []
    wers: list[float] = []
    infer_ms_values: list[float] = []
    finalize_ms_values: list[float] = []

    for case in cases:
        finalize_start = time.perf_counter()
        samples, sample_rate = _read_wav_samples(case.audio_path)
        infer_start = time.perf_counter()
        hypothesis = transcriber.transcribe_samples(samples, sample_rate=sample_rate)
        infer_ms = (time.perf_counter() - infer_start) * 1000.0
        finalize_ms = (time.perf_counter() - finalize_start) * 1000.0
        normalized_reference = normalize_transcript(case.reference)
        normalized_hypothesis = normalize_transcript(hypothesis)
        wer = compute_normalized_wer(case.reference, hypothesis)

        sample_rows.append(
            {
                "sample_id": case.sample_id,
                "audio_path": str(case.audio_path),
                "sample_rate": sample_rate,
                "duration_seconds": samples.size / float(sample_rate) if sample_rate > 0 else 0.0,
                "reference": case.reference,
                "hypothesis": hypothesis,
                "normalized_reference": normalized_reference,
                "normalized_hypothesis": normalized_hypothesis,
                "wer": wer,
                "infer_ms": infer_ms,
                "finalize_ms": finalize_ms,
            }
        )
        wers.append(wer)
        infer_ms_values.append(infer_ms)
        finalize_ms_values.append(finalize_ms)

    avg_wer = sum(wers) / len(wers) if wers else 0.0
    infer_summary = summarize_timings_ms(infer_ms_values)
    finalize_summary = summarize_timings_ms(finalize_ms_values)
    failures = evaluate_regression_thresholds(
        avg_wer=avg_wer,
        infer_p95_ms=infer_summary["p95"],
        finalize_p95_ms=finalize_summary["p95"],
        max_avg_wer=args.max_avg_wer,
        max_p95_infer_ms=args.max_p95_infer_ms,
        max_p95_finalize_ms=args.max_p95_finalize_ms,
    )

    report = {
        "benchmark": "offline",
        "model": args.model,
        "requested_device": args.device,
        "effective_device": effective_device,
        "bench_dir": str(bench_dir),
        "transcripts_path": str(transcripts_path),
        "sample_count": len(sample_rows),
        "aggregate": {
            "avg_wer": avg_wer,
            "infer_ms": infer_summary,
            "finalize_ms": finalize_summary,
        },
        "thresholds": {
            "max_avg_wer": args.max_avg_wer,
            "max_p95_infer_ms": args.max_p95_infer_ms,
            "max_p95_finalize_ms": args.max_p95_finalize_ms,
        },
        "regression_gate": {
            "pass": not failures,
            "failures": failures,
        },
        "samples": sample_rows,
    }

    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(
        json.dumps(report, indent=2, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )

    print(f"Offline benchmark completed for {len(sample_rows)} sample(s)")
    print(f"Model: {args.model} (requested={args.device}, effective={effective_device})")
    print(f"Average WER: {avg_wer:.4f}")
    print(
        "Infer ms (avg/p50/p95): "
        f"{infer_summary['avg']:.2f}/{infer_summary['p50']:.2f}/{infer_summary['p95']:.2f}"
    )
    print(
        "Finalize ms (avg/p50/p95): "
        f"{finalize_summary['avg']:.2f}/{finalize_summary['p50']:.2f}/{finalize_summary['p95']:.2f}"
    )
    for row in sample_rows:
        print(
            f" - {row['sample_id']}: wer={row['wer']:.4f}, "
            f"infer_ms={row['infer_ms']:.2f}, finalize_ms={row['finalize_ms']:.2f}"
        )
    print(f"JSON report written to: {output_path}")
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
    try:
        import soundfile as sf

        sf.write(path, samples, SAMPLE_RATE)
    except Exception:  # pragma: no cover - minimal fallback
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
    except Exception as exc:
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
    except Exception as exc:  # pragma: no cover - runtime probe
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
        "--bench-dir",
        type=Path,
        default=BENCH_AUDIO_DIR,
        help="Directory containing sample_*.wav benchmark files",
    )
    parser.add_argument(
        "--bench-transcripts",
        type=Path,
        default=None,
        help=(
            "Override path to benchmark transcripts file (defaults to <bench-dir>/transcripts.txt)"
        ),
    )
    parser.add_argument(
        "--bench-output",
        type=Path,
        default=None,
        help=(
            "Path for benchmark JSON output "
            f"(defaults to {DEFAULT_BENCH_OUTPUT.relative_to(Path(__file__).resolve().parent)})"
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
