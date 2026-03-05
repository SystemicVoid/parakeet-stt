#!/usr/bin/env python3
"""Build a local candidate phrase list for personal STT evaluation."""

from __future__ import annotations

import argparse
import csv
import json
import re
import sqlite3
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_OUTPUT = PROJECT_ROOT / "parakeet-stt-daemon/bench_audio/personal/candidates.tsv"
DEFAULT_BASH_HISTORY = Path.home() / ".bash_history"
DEFAULT_CODEX_DB = Path.home() / ".codex/state_5.sqlite"
DEFAULT_CODEX_SOURCES = "cli"
DEFAULT_CODEX_MAX_THREADS = 20

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

_PROMPT_ACTION_RE = re.compile(
    r"\b("
    r"can you|please|implement|fix|review|help|add|remove|update|make|refactor|"
    r"investigate|analy[sz]e|migrate|clean|plan|build|create|improve|check|run|test|"
    r"document|explain|walk me through|i want|i need|lets|let's|should we|could we"
    r")\b",
    flags=re.IGNORECASE,
)
_PROMPT_NOISE_RE = (
    re.compile(r"^\s*(>>>|\$|\[NeMo|\d{4}-\d{2}-\d{2}T)"),
    re.compile(r"\tcodex_prompt\t"),
    re.compile(r"^<context>"),
    re.compile(r"</context>$"),
    re.compile(r"review output from reviewer model", flags=re.IGNORECASE),
    re.compile(r"user may select one or more comments to resolve", flags=re.IGNORECASE),
    re.compile(r"^\s*\d+\s+[+\-]"),
    re.compile(r"^\s*[A-Za-z][A-Za-z _-]+\((ms|s)\):\s*\d+\s*$"),
    re.compile(
        r"\b(pid|tmux|exec_command|toolcall|bash -lc|cargo run|uv run|tee -a|nemo_logging)\b",
        flags=re.IGNORECASE,
    ),
)
_INJECTED_USER_MESSAGE_RE = (
    re.compile(r"^# AGENTS\.md instructions for ", flags=re.MULTILINE),
    re.compile(r"<INSTRUCTIONS>|</INSTRUCTIONS>", flags=re.IGNORECASE),
    re.compile(r"<environment_context>|</environment_context>", flags=re.IGNORECASE),
    re.compile(r"<collaboration_mode>|</collaboration_mode>", flags=re.IGNORECASE),
    re.compile(r"<skill>|</skill>", flags=re.IGNORECASE),
    re.compile(r"The user interrupted the previous turn on purpose\.", flags=re.IGNORECASE),
    re.compile(r"Any running unified exec processes were terminated\.", flags=re.IGNORECASE),
)
_MESSAGE_STRIP_RE = (
    re.compile(r"<INSTRUCTIONS>.*?</INSTRUCTIONS>", flags=re.IGNORECASE | re.DOTALL),
    re.compile(r"<environment_context>.*?</environment_context>", flags=re.IGNORECASE | re.DOTALL),
    re.compile(r"<collaboration_mode>.*?</collaboration_mode>", flags=re.IGNORECASE | re.DOTALL),
    re.compile(r"<skill>.*?</skill>", flags=re.IGNORECASE | re.DOTALL),
)


@dataclass(frozen=True)
class Candidate:
    source: str
    raw_text: str
    suggested_clean_text: str
    redaction_flags: str


def _looks_relevant_command(command: str) -> bool:
    if any(pattern.search(command) for pattern in EXCLUDE_PATTERNS):
        return False
    return any(pattern.search(command) for pattern in KEYWORD_PATTERNS)


def _looks_relevant_prompt(prompt: str) -> bool:
    if not bool(re.search(r"[a-zA-Z]", prompt)):
        return False
    if "?" in prompt:
        return True
    return bool(_PROMPT_ACTION_RE.search(prompt))


def _text_letter_ratio(text: str) -> float:
    if not text:
        return 0.0
    return sum(character.isalpha() for character in text) / len(text)


def _normalize_whitespace(text: str) -> str:
    return re.sub(r"\s+", " ", text).strip()


def _intent_cleanup(command: str) -> str:
    cleaned = command.strip()
    cleaned = cleaned.replace(str(PROJECT_ROOT), "parakeet-stt")
    cleaned = cleaned.replace("&&", " then ")
    cleaned = cleaned.replace("||", " or ")
    cleaned = re.sub(r"\s+-{2,}", " --", cleaned)
    cleaned = cleaned.replace('"', "")
    cleaned = cleaned.replace("'", "")
    return _normalize_whitespace(cleaned)


def _extract_prompt_phrases(text: str) -> list[str]:
    phrases: list[str] = []
    in_code_block = False
    for raw_line in text.splitlines():
        line = raw_line.strip()
        if line.startswith("```"):
            in_code_block = not in_code_block
            continue
        if in_code_block or not line:
            continue
        line = re.sub(r"^#+\s*", "", line)
        line = re.sub(r"^\s*[-*]\s+", "", line)
        line = re.sub(r"^\s*\d+[.)]\s+", "", line)
        line = line.replace("`", "")
        if not line:
            continue
        if "\t" in line:
            continue
        if _text_letter_ratio(line) < 0.50:
            continue
        if any(pattern.search(line) for pattern in _PROMPT_NOISE_RE):
            continue
        segments = re.split(r"(?<=[.?!])\s+", line)
        for segment in segments:
            phrase = _normalize_whitespace(segment)
            if not phrase:
                continue
            word_count = len(phrase.split())
            if not _looks_relevant_prompt(phrase):
                continue
            if _text_letter_ratio(phrase) < 0.55:
                continue
            if 5 <= word_count <= 35 and 24 <= len(phrase) <= 220:
                phrases.append(phrase)
    deduped: list[str] = []
    seen: set[str] = set()
    for phrase in phrases:
        key = phrase.casefold()
        if key in seen:
            continue
        seen.add(key)
        deduped.append(phrase)
    return deduped


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


def _extract_exec_commands_from_codex_logs(path: Path) -> list[str]:
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


def _should_skip_user_message(text: str) -> bool:
    if not text.strip():
        return True
    if any(pattern.search(text) for pattern in _INJECTED_USER_MESSAGE_RE):
        return True
    non_empty_lines = [line for line in text.splitlines() if line.strip()]
    if len(non_empty_lines) > 16:
        return True
    if len(text) > 1600:
        return True
    return False


def _strip_message_boilerplate(text: str) -> str:
    stripped = text
    for pattern in _MESSAGE_STRIP_RE:
        stripped = pattern.sub(" ", stripped)
    return stripped


def _iter_codex_rollout_paths(
    path: Path,
    *,
    sources: list[str],
    cwd: Path | None,
    max_threads: int,
) -> list[Path]:
    if not path.exists() or not sources:
        return []
    placeholders = ",".join("?" for _ in sources)
    query = (
        f"SELECT rollout_path FROM threads WHERE source IN ({placeholders}) AND rollout_path <> ''"
    )
    parameters: list[str] = list(sources)
    if cwd is not None:
        query += " AND cwd = ?"
        parameters.append(str(cwd))
    query += " ORDER BY created_at DESC"
    if max_threads > 0:
        query += " LIMIT ?"
        parameters.append(str(max_threads))

    paths: list[Path] = []
    connection = sqlite3.connect(path)
    try:
        cursor = connection.execute(query, tuple(parameters))
        for (rollout_path,) in cursor.fetchall():
            if not isinstance(rollout_path, str):
                continue
            candidate = Path(rollout_path)
            if candidate.exists():
                paths.append(candidate)
    finally:
        connection.close()
    return paths


def _extract_user_messages_from_rollout(path: Path) -> list[str]:
    messages: list[str] = []
    with path.open(encoding="utf-8", errors="ignore") as handle:
        for line in handle:
            raw = line.strip()
            if not raw:
                continue
            try:
                payload = json.loads(raw)
            except json.JSONDecodeError:
                continue
            if payload.get("type") != "response_item":
                continue
            response_item = payload.get("payload")
            if not isinstance(response_item, dict):
                continue
            if response_item.get("type") != "message" or response_item.get("role") != "user":
                continue
            content = response_item.get("content")
            if not isinstance(content, list):
                continue
            user_texts: list[str] = []
            for block in content:
                if not isinstance(block, dict):
                    continue
                text = block.get("text")
                if isinstance(text, str) and text.strip():
                    user_texts.append(text)
            if user_texts:
                messages.append("\n".join(user_texts))
    return messages


def _extract_prompts_from_codex_threads(
    path: Path,
    *,
    sources: list[str],
    cwd: Path | None,
    max_threads: int,
) -> list[str]:
    prompts: list[str] = []
    rollout_paths = _iter_codex_rollout_paths(
        path,
        sources=sources,
        cwd=cwd,
        max_threads=max_threads,
    )
    for rollout_path in rollout_paths:
        for message in _extract_user_messages_from_rollout(rollout_path):
            if _should_skip_user_message(message):
                continue
            cleaned_message = _strip_message_boilerplate(message)
            prompts.extend(_extract_prompt_phrases(cleaned_message))
    return prompts


def _to_candidate(source: str, raw_text: str, *, kind: str) -> Candidate | None:
    if kind == "command" and not _looks_relevant_command(raw_text):
        return None
    if kind == "prompt" and not _looks_relevant_prompt(raw_text):
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
    parser.add_argument("--codex-db", type=Path, default=DEFAULT_CODEX_DB)
    parser.add_argument(
        "--codex-sources",
        default=DEFAULT_CODEX_SOURCES,
        help="Comma-separated Codex thread sources to read from rollout messages (default: cli)",
    )
    parser.add_argument(
        "--codex-cwd",
        type=Path,
        default=PROJECT_ROOT,
        help="Only include Codex prompts recorded with this cwd (default: repo root)",
    )
    parser.add_argument(
        "--all-cwds",
        action="store_true",
        help="Disable cwd filter for Codex prompts (less focused corpus)",
    )
    parser.add_argument(
        "--codex-max-threads",
        type=int,
        default=DEFAULT_CODEX_MAX_THREADS,
        help="Max recent Codex threads to scan for user messages (default: 20)",
    )
    parser.add_argument("--bash-history", type=Path, default=DEFAULT_BASH_HISTORY)
    parser.add_argument(
        "--include-bash-history",
        action="store_true",
        help="Opt-in: include shell command history candidates",
    )
    parser.add_argument(
        "--include-codex-exec-commands",
        action="store_true",
        help="Opt-in: include Codex exec_command tool calls",
    )
    parser.add_argument("--limit", type=int, default=300)
    parser.add_argument("--output", type=Path, default=DEFAULT_OUTPUT)
    return parser.parse_args()


def _parse_sources(raw_sources: str) -> list[str]:
    seen: set[str] = set()
    parsed: list[str] = []
    for source in (chunk.strip() for chunk in raw_sources.split(",")):
        if not source:
            continue
        key = source.casefold()
        if key in seen:
            continue
        seen.add(key)
        parsed.append(source)
    return parsed


def main() -> int:
    args = parse_args()
    if args.limit <= 0:
        raise ValueError("--limit must be > 0")
    if args.codex_max_threads < 0:
        raise ValueError("--codex-max-threads must be >= 0")
    sources = _parse_sources(args.codex_sources)
    if not sources:
        raise ValueError("--codex-sources must include at least one source")
    codex_cwd = None if args.all_cwds else args.codex_cwd.resolve()

    raw_candidates: list[Candidate] = []
    for phrase in _extract_prompts_from_codex_threads(
        args.codex_db,
        sources=sources,
        cwd=codex_cwd,
        max_threads=args.codex_max_threads,
    ):
        candidate = _to_candidate("codex_prompt", phrase, kind="prompt")
        if candidate is not None:
            raw_candidates.append(candidate)
    if args.include_bash_history:
        for command in _extract_from_bash_history(args.bash_history):
            candidate = _to_candidate("bash_history", command, kind="command")
            if candidate is not None:
                raw_candidates.append(candidate)
    if args.include_codex_exec_commands:
        for command in _extract_exec_commands_from_codex_logs(args.codex_db):
            candidate = _to_candidate("codex_exec_command", command, kind="command")
            if candidate is not None:
                raw_candidates.append(candidate)

    grouped: Counter[tuple[str, str, str]] = Counter()
    grouped_sources: dict[tuple[str, str, str], set[str]] = defaultdict(set)
    for candidate in raw_candidates:
        key = (
            candidate.suggested_clean_text,
            candidate.raw_text,
            candidate.redaction_flags,
        )
        grouped[key] += 1
        grouped_sources[key].add(candidate.source)

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
                    ",".join(
                        sorted(grouped_sources[(suggested_clean_text, raw_text, redaction_flags)])
                    ),
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
