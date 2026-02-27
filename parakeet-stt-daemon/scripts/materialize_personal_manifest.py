#!/usr/bin/env python3
"""Create benchmark manifest JSONL from reviewed candidate TSV rows."""

from __future__ import annotations

import argparse
import csv
import json
import re
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_CANDIDATES = PROJECT_ROOT / "parakeet-stt-daemon/bench_audio/personal/candidates.tsv"
DEFAULT_MANIFEST = PROJECT_ROOT / "parakeet-stt-daemon/bench_audio/personal/manifest.jsonl"
DEFAULT_PROMPTS = PROJECT_ROOT / "parakeet-stt-daemon/bench_audio/personal/prompts.tsv"
DEFAULT_AUDIO_DIR = Path("personal/audio")

_TOKEN_RE = re.compile(r"[a-z0-9]+")
_STOPWORDS = {
    "and",
    "for",
    "the",
    "with",
    "from",
    "into",
    "then",
    "true",
    "false",
    "run",
}


def _is_included(value: str) -> bool:
    return value.strip().casefold() in {"1", "y", "yes", "true"}


def _normalize_reference(text: str) -> str:
    cleaned = re.sub(r"\s+", " ", text).strip()
    if not cleaned:
        return cleaned
    return cleaned[0].upper() + cleaned[1:]


def _extract_critical_tokens(text: str, limit: int) -> list[str]:
    tokens: list[str] = []
    seen: set[str] = set()
    for token in _TOKEN_RE.findall(text.casefold()):
        if token in _STOPWORDS:
            continue
        if len(token) < 3 and not token.isdigit():
            continue
        if token not in seen:
            tokens.append(token)
            seen.add(token)
        if len(tokens) >= limit:
            break
    return tokens


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--input", type=Path, default=DEFAULT_CANDIDATES)
    parser.add_argument("--output", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--prompts-output", type=Path, default=DEFAULT_PROMPTS)
    parser.add_argument("--audio-dir", type=Path, default=DEFAULT_AUDIO_DIR)
    parser.add_argument("--tier", choices=["smoke", "daily", "weekly"], default="daily")
    parser.add_argument("--critical-limit", type=int, default=8)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.critical_limit <= 0:
        raise ValueError("--critical-limit must be > 0")
    if not args.input.exists():
        raise FileNotFoundError(f"Candidate TSV not found: {args.input}")

    approved_rows: list[dict[str, str]] = []
    with args.input.open("r", encoding="utf-8", newline="") as handle:
        reader = csv.DictReader(handle, delimiter="\t")
        for row in reader:
            include_value = row.get("include", "")
            if not _is_included(include_value):
                continue
            cleaned = _normalize_reference(row.get("suggested_clean_text", ""))
            if not cleaned:
                continue
            approved_rows.append(
                {
                    "reference": cleaned,
                    "source": row.get("source", "mixed"),
                }
            )

    if not approved_rows:
        raise ValueError("No approved rows found in input TSV")

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.prompts_output.parent.mkdir(parents=True, exist_ok=True)

    manifest_lines: list[str] = []
    prompt_lines: list[str] = []
    for index, row in enumerate(approved_rows, start=1):
        sample_id = f"cmd_{index:03d}"
        reference = row["reference"]
        record = {
            "sample_id": sample_id,
            "audio_path": str(args.audio_dir / f"{sample_id}.wav"),
            "reference": reference,
            "tier": args.tier,
            "domain": "command",
            "critical_tokens": _extract_critical_tokens(reference, args.critical_limit),
            "source": row["source"],
        }
        manifest_lines.append(json.dumps(record, ensure_ascii=False))
        prompt_lines.append(f"{sample_id}\t{reference}")

    args.output.write_text("\n".join(manifest_lines) + "\n", encoding="utf-8")
    args.prompts_output.write_text("\n".join(prompt_lines) + "\n", encoding="utf-8")

    print(f"Wrote {len(approved_rows)} manifest rows to {args.output}")
    print(f"Wrote prompt list to {args.prompts_output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
