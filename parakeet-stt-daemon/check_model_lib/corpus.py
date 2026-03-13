"""Corpus loading, manifest parsing, and benchmark case resolution."""

from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from check_model_lib.constants import _ALLOWED_DOMAINS, _TRANSCRIPT_LINE_RE
from check_model_lib.metrics import normalize_transcript


@dataclass(frozen=True)
class BenchmarkCase:
    sample_id: str
    audio_path: Path
    reference: str
    tier: str = "default"
    domain: str = "dictation"
    critical_tokens: tuple[str, ...] = ()


def _strip_wrapping_quotes(text: str) -> str:
    stripped = text.strip()
    quote_chars = "\"'\u201c\u201d\u2018\u2019"
    if len(stripped) >= 2 and stripped[0] in quote_chars and stripped[-1] in quote_chars:
        return stripped[1:-1].strip()
    return stripped


def _normalize_critical_tokens(tokens: list[str]) -> tuple[str, ...]:
    normalized_tokens: list[str] = []
    seen: set[str] = set()
    for token in tokens:
        for normalized in normalize_transcript(token).split():
            if normalized and normalized not in seen:
                normalized_tokens.append(normalized)
                seen.add(normalized)
    return tuple(normalized_tokens)


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


def parse_benchmark_manifest(
    path: Path,
    *,
    bench_dir: Path,
    bench_tier: str | None = None,
) -> list[BenchmarkCase]:
    if not path.exists():
        raise FileNotFoundError(f"Benchmark manifest not found: {path}")
    if path.is_dir():
        raise ValueError(
            "Benchmark manifest path points to a directory; pass a JSONL file like "
            f"'{path / 'manifest.jsonl'}'"
        )

    cases: list[BenchmarkCase] = []
    seen_sample_ids: set[str] = set()
    for line_no, raw_line in enumerate(path.read_text(encoding="utf-8").splitlines(), start=1):
        line = raw_line.strip()
        if not line or line.startswith("#"):
            continue
        try:
            payload = json.loads(line)
        except json.JSONDecodeError as err:
            raise ValueError(f"Invalid JSON line at {path}:{line_no}") from err
        if not isinstance(payload, dict):
            raise ValueError(f"Manifest row must be a JSON object at {path}:{line_no}")

        sample_id_raw = str(payload.get("sample_id", "")).strip()
        if not sample_id_raw:
            raise ValueError(f"Manifest row missing sample_id at {path}:{line_no}")
        sample_id = sample_id_raw
        if sample_id in seen_sample_ids:
            raise ValueError(f"Duplicate manifest sample_id {sample_id} in {path}:{line_no}")

        reference = str(payload.get("reference", "")).strip()
        if not reference:
            raise ValueError(f"Manifest row missing reference for {sample_id} at {path}:{line_no}")

        audio_path_raw = str(payload.get("audio_path", "")).strip()
        if not audio_path_raw:
            raise ValueError(f"Manifest row missing audio_path for {sample_id} at {path}:{line_no}")
        audio_path = Path(audio_path_raw)
        if not audio_path.is_absolute():
            audio_path = (bench_dir / audio_path).resolve()
        if not audio_path.exists():
            raise ValueError(f"Manifest audio path not found for {sample_id}: {audio_path}")

        tier = str(payload.get("tier", "default")).strip().casefold()
        if bench_tier is not None and tier != bench_tier:
            continue

        domain = str(payload.get("domain", "dictation")).strip().casefold()
        if domain not in _ALLOWED_DOMAINS:
            raise ValueError(
                f"Manifest row domain must be one of {sorted(_ALLOWED_DOMAINS)} "
                f"for {sample_id} at {path}:{line_no}"
            )

        critical_tokens_raw = payload.get("critical_tokens", [])
        if critical_tokens_raw is None:
            critical_tokens_raw = []
        if not isinstance(critical_tokens_raw, list) or not all(
            isinstance(token, str) for token in critical_tokens_raw
        ):
            raise ValueError(
                f"Manifest row critical_tokens must be a list[str] for {sample_id} "
                f"at {path}:{line_no}"
            )
        critical_tokens = _normalize_critical_tokens(critical_tokens_raw)

        seen_sample_ids.add(sample_id)
        cases.append(
            BenchmarkCase(
                sample_id=sample_id,
                audio_path=audio_path,
                reference=reference,
                tier=tier,
                domain=domain,
                critical_tokens=critical_tokens,
            )
        )

    if not cases:
        if bench_tier is not None:
            raise ValueError(f"No manifest benchmark cases found for tier '{bench_tier}' in {path}")
        raise ValueError(f"No manifest benchmark cases found in {path}")
    return cases


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


def _collect_legacy_transcript_cases(
    *,
    bench_dir: Path,
    bench_transcripts: Path | None,
) -> tuple[list[BenchmarkCase], Path]:
    transcripts_path = (
        bench_transcripts.resolve()
        if bench_transcripts is not None
        else (bench_dir / "transcripts.txt")
    )
    transcripts = parse_benchmark_transcripts(transcripts_path)
    return collect_benchmark_cases(bench_dir, transcripts), transcripts_path


def _merge_benchmark_case_sets(
    primary_cases: list[BenchmarkCase],
    extra_cases: list[BenchmarkCase],
    *,
    extra_label: str,
) -> list[BenchmarkCase]:
    merged = list(primary_cases)
    seen_sample_ids = {case.sample_id for case in primary_cases}
    duplicates = sorted(case.sample_id for case in extra_cases if case.sample_id in seen_sample_ids)
    if duplicates:
        formatted = ", ".join(duplicates)
        raise ValueError(
            "Cannot append "
            f"{extra_label} benchmark cases because sample_id already exists: {formatted}"
        )
    merged.extend(extra_cases)
    return merged


def _resolve_benchmark_cases(
    *,
    bench_dir: Path,
    bench_tier: str | None,
    bench_manifest: Path | None,
    bench_transcripts: Path | None,
    bench_append_legacy: bool,
) -> tuple[list[BenchmarkCase], Path | None, Path | None]:
    effective_manifest_tier = None if bench_tier in {None, "all"} else bench_tier
    if bench_manifest is not None:
        manifest_path = bench_manifest.resolve()
        if manifest_path.is_dir():
            default_manifest = manifest_path / "manifest.jsonl"
            if not default_manifest.exists():
                raise FileNotFoundError(
                    "Benchmark manifest directory provided but manifest.jsonl is missing: "
                    f"{default_manifest}"
                )
            manifest_path = default_manifest
        cases = parse_benchmark_manifest(
            manifest_path,
            bench_dir=bench_dir,
            bench_tier=effective_manifest_tier,
        )
        appended_transcripts_path: Path | None = None
        if bench_append_legacy:
            legacy_cases, appended_transcripts_path = _collect_legacy_transcript_cases(
                bench_dir=bench_dir,
                bench_transcripts=bench_transcripts,
            )
            cases = _merge_benchmark_case_sets(
                cases,
                legacy_cases,
                extra_label="legacy transcript",
            )
        return cases, manifest_path, appended_transcripts_path

    cases, transcripts_path = _collect_legacy_transcript_cases(
        bench_dir=bench_dir,
        bench_transcripts=bench_transcripts,
    )
    return cases, None, transcripts_path
