#!/usr/bin/env python3
"""Build a local candidate phrase list for personal STT evaluation."""

from __future__ import annotations

import argparse
import csv
import json
import re
import sqlite3
from collections import Counter
from dataclasses import dataclass
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = PROJECT_ROOT / "parakeet-stt-daemon/bench_audio/personal/candidates.tsv"
DEFAULT_BASH_HISTORY = Path.home() / ".bash_history"
DEFAULT_CODEX_DB = Path.home() / ".codex/state_5.sqlite"

_PATH_RE = re.compile(r"/home/[^/\s]+(?:/[^\s]+)*")
_EMAIL_RE = re.compile(r"\b[A-Z0-9._%+-]+@[A-Z0-9.-]+\.[A-Z]{2,}\b", flags=re.IGNORECASE)
_LONG_ID_RE = re.compile(r"\b[a-f0-9]{16,}\b", flags=re.IGNORECASE)
_URL_QUERY_RE = re.compile(r"https?://[^\s?]+(?:\?[^\s]+)")
_SECRET_ASSIGN_RE = re.compile(
    r"\b(api[_-]?key|token|secret|password|passwd|pwd)\s*=\s*['\"]?[^'\"\s]+",
    flags=re.IGNORECASE,
)

KEYWORD_PATTERNS = (
    re.compile(r"\bstt\b"),
    re.compile(r"\bparakeet\b"),
    re.compile(r"\buv run\b"),
    re.compile(r"\bcargo\b"),
    re.compile(r"\bpytest\b"),
    re.compile(r"\bruff\b"),
    re.compile(r"\bty\b"),
    re.compile(r"\bdeptry\b"),
    re.compile(r"\bprek\b"),
    re.compile(r"\bgit\b"),
)

EXCLUDE_PATTERNS = (
    re.compile(r"\bkill\s+-9\b"),
    re.compile(r"\bpkill\b"),
    re.compile(r"\brm\s+-rf\b"),
    re.compile(r"\bchmod\s+777\b"),
)


@dataclass(frozen=True)
class Candidate:
    source: str
    raw_text: str
    suggested_clean_text: str
    redaction_flags: str


def _looks_relevant(command: str) -> bool:
    if any(pattern.search(command) for pattern in EXCLUDE_PATTERNS):
        return False
    return any(pattern.search(command) for pattern in KEYWORD_PATTERNS)


def _normalize_whitespace(text: str) -> str:
    return re.sub(r"\s+", " ", text).strip()


def _intent_cleanup(command: str) -> str:
    cleaned = command.strip()
    cleaned = cleaned.replace(str(PROJECT_ROOT), "parakeet-stt")
    cleaned = cleaned.replace("/home/hugo/Documents/Engineering/parakeet-stt", "parakeet-stt")
    cleaned = cleaned.replace("&&", " then ")
    cleaned = cleaned.replace("||", " or ")
    cleaned = re.sub(r"\s+-{2,}", " --", cleaned)
    cleaned = cleaned.replace('"', "")
    cleaned = cleaned.replace("'", "")
    return _normalize_whitespace(cleaned)


def _redact_sensitive(text: str) -> tuple[str, list[str]]:
    redaction_flags: list[str] = []
    redacted = text

    if _EMAIL_RE.search(redacted):
        redacted = _EMAIL_RE.sub("<EMAIL>", redacted)
        redaction_flags.append("email")
    if _URL_QUERY_RE.search(redacted):
        redacted = _URL_QUERY_RE.sub("<URL_WITH_QUERY>", redacted)
        redaction_flags.append("url_query")
    if _LONG_ID_RE.search(redacted):
        redacted = _LONG_ID_RE.sub("<LONG_ID>", redacted)
        redaction_flags.append("long_id")
    if _SECRET_ASSIGN_RE.search(redacted):
        redacted = _SECRET_ASSIGN_RE.sub("<SECRET_ASSIGNMENT>", redacted)
        redaction_flags.append("secret_assignment")
    if _PATH_RE.search(redacted):
        redacted = _PATH_RE.sub("<PATH>", redacted)
        redaction_flags.append("path")

    return _normalize_whitespace(redacted), sorted(set(redaction_flags))


def _extract_from_bash_history(path: Path) -> list[str]:
    if not path.exists():
        return []
    lines = path.read_text(encoding="utf-8", errors="ignore").splitlines()
    return [line.strip() for line in lines if line.strip()]


def _extract_from_codex_db(path: Path) -> list[str]:
    if not path.exists():
        return []
    commands: list[str] = []
    connection = sqlite3.connect(path)
    try:
        cursor = connection.execute(
            """
            SELECT message
            FROM logs
            WHERE target = 'codex_core::stream_events_utils'
              AND message LIKE 'ToolCall: exec_command%'
            """
        )
        for (message,) in cursor.fetchall():
            if not isinstance(message, str):
                continue
            start = message.find("{")
            if start == -1:
                continue
            try:
                payload = json.loads(message[start:])
            except json.JSONDecodeError:
                continue
            command = payload.get("cmd")
            if isinstance(command, str) and command.strip():
                commands.append(command.strip())
    finally:
        connection.close()
    return commands


def _to_candidate(source: str, raw_text: str) -> Candidate | None:
    if not _looks_relevant(raw_text):
        return None
    redacted_raw, redaction_flags = _redact_sensitive(raw_text)
    suggested = _intent_cleanup(redacted_raw)
    if len(suggested) < 5:
        return None
    return Candidate(
        source=source,
        raw_text=redacted_raw,
        suggested_clean_text=suggested,
        redaction_flags=",".join(redaction_flags),
    )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--bash-history", type=Path, default=DEFAULT_BASH_HISTORY)
    parser.add_argument("--codex-db", type=Path, default=DEFAULT_CODEX_DB)
    parser.add_argument("--limit", type=int, default=300)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if args.limit <= 0:
        raise ValueError("--limit must be > 0")

    raw_candidates: list[Candidate] = []
    for command in _extract_from_bash_history(args.bash_history):
        candidate = _to_candidate("bash_history", command)
        if candidate is not None:
            raw_candidates.append(candidate)
    for command in _extract_from_codex_db(args.codex_db):
        candidate = _to_candidate("codex_logs", command)
        if candidate is not None:
            raw_candidates.append(candidate)

    grouped: Counter[tuple[str, str, str]] = Counter(
        (candidate.suggested_clean_text, candidate.raw_text, candidate.redaction_flags)
        for candidate in raw_candidates
    )

    rows = grouped.most_common(args.limit)
    args.output.parent.mkdir(parents=True, exist_ok=True)
    with args.output.open("w", encoding="utf-8", newline="") as handle:
        writer = csv.writer(handle, delimiter="\t")
        writer.writerow(
            [
                "include",
                "frequency",
                "source",
                "raw_text",
                "suggested_clean_text",
                "redaction_flags",
            ]
        )
        for (suggested_clean_text, raw_text, redaction_flags), frequency in rows:
            writer.writerow(
                [
                    "",
                    str(frequency),
                    "mixed",
                    raw_text,
                    suggested_clean_text,
                    redaction_flags,
                ]
            )

    print(f"Wrote {len(rows)} candidates to {args.output}")
    print("Manual review required: mark include column with yes/true/1 for approved phrases.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
