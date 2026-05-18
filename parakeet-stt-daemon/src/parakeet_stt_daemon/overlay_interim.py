"""Stabilize Overlay interim text from the Stream path and Seal path.

The Daemon feeds raw Stream path and stop-replay Seal path candidates into this
module during one Session. The module owns the mutable Overlay transcript tail,
source sequence counters, overlap reconciliation, pending-tail flush behavior,
and debug logging for interim text decisions.
"""

from __future__ import annotations

import json
import os
from dataclasses import dataclass, field
from uuid import UUID

import numpy as np
from loguru import logger


@dataclass(frozen=True)
class OverlayInterimTranscriptContext:
    session_id: UUID
    context_samples: int
    sample_rate: int


@dataclass(frozen=True)
class StabilizedInterimText:
    text: str


@dataclass
class _OverlayInterimTranscriptState:
    committed_tokens: list[str] = field(default_factory=list)
    draft_tokens: list[str] = field(default_factory=list)


class OverlayInterimTranscriptStabilizer:
    """Reconcile one Session's Overlay interim text across Stream and Seal paths."""

    def __init__(self) -> None:
        self._state = _OverlayInterimTranscriptState()
        self._source_seq_by_source: dict[str, int] = {}

    def next_source_seq(self, source: str) -> int:
        current = int(self._source_seq_by_source.get(source, 0))
        self._source_seq_by_source[source] = current + 1
        return current

    def accept(
        self,
        source: str,
        source_seq: int,
        text: str,
        context: OverlayInterimTranscriptContext,
    ) -> StabilizedInterimText | None:
        current_next = int(self._source_seq_by_source.get(source, 0))
        self._source_seq_by_source[source] = max(current_next, source_seq + 1)

        committed_before = [*self._state.committed_tokens]
        draft_before = [*self._state.draft_tokens]
        previous_display_tokens = [*committed_before, *draft_before]
        previous_display = " ".join(previous_display_tokens)
        normalized = " ".join(text.split()).strip()
        if not normalized:
            self._log_event(
                context,
                source=source,
                source_seq=source_seq,
                raw_text=text,
                normalized_text=normalized,
                previous_display=previous_display,
                committed_before=committed_before,
                draft_before=draft_before,
                raw_tokens=[],
                overlap=0,
                committed_after=committed_before,
                draft_after=draft_before,
                current_display=previous_display,
                action="empty",
            )
            return None

        raw_tokens = normalized.split()
        overlap = _longest_casefold_suffix_prefix_overlap(previous_display_tokens, raw_tokens)

        if overlap > 0:
            self._state.committed_tokens = previous_display_tokens[:-overlap]
        self._state.draft_tokens = raw_tokens

        committed_after = [*self._state.committed_tokens]
        draft_after = [*self._state.draft_tokens]
        current_display = " ".join([*committed_after, *draft_after])
        action = "emit"
        if current_display == previous_display:
            action = "no_change"
            self._log_event(
                context,
                source=source,
                source_seq=source_seq,
                raw_text=text,
                normalized_text=normalized,
                previous_display=previous_display,
                committed_before=committed_before,
                draft_before=draft_before,
                raw_tokens=raw_tokens,
                overlap=overlap,
                committed_after=committed_after,
                draft_after=draft_after,
                current_display=current_display,
                action=action,
            )
            return None

        self._log_event(
            context,
            source=source,
            source_seq=source_seq,
            raw_text=text,
            normalized_text=normalized,
            previous_display=previous_display,
            committed_before=committed_before,
            draft_before=draft_before,
            raw_tokens=raw_tokens,
            overlap=overlap,
            committed_after=committed_after,
            draft_after=draft_after,
            current_display=current_display,
            action=action,
        )
        return StabilizedInterimText(current_display)

    def flush_pending_tail(self) -> StabilizedInterimText | None:
        display_tokens = [*self._state.committed_tokens, *self._state.draft_tokens]
        if not display_tokens:
            return None
        return StabilizedInterimText(" ".join(display_tokens))

    def record_skip(
        self,
        source: str,
        source_seq: int,
        context: OverlayInterimTranscriptContext,
        *,
        reason: str,
        error_class: str | None = None,
    ) -> None:
        if not _streaming_debug_enabled():
            return
        context_secs = (
            context.context_samples / context.sample_rate if context.sample_rate > 0 else 0.0
        )
        logger.info(
            (
                "overlay_stabilizer_skip session_id={} source={} source_seq={} "
                "context_samples={} context_secs={:.3f} reason={} error_class={}"
            ),
            context.session_id,
            source,
            source_seq,
            context.context_samples,
            context_secs,
            reason,
            error_class,
        )

    def _log_event(
        self,
        context: OverlayInterimTranscriptContext,
        *,
        source: str,
        source_seq: int,
        raw_text: str,
        normalized_text: str,
        previous_display: str,
        committed_before: list[str],
        draft_before: list[str],
        raw_tokens: list[str],
        overlap: int,
        committed_after: list[str],
        draft_after: list[str],
        current_display: str,
        action: str,
    ) -> None:
        if not _streaming_debug_enabled():
            return
        context_secs = (
            context.context_samples / context.sample_rate if context.sample_rate > 0 else 0.0
        )
        logger.info(
            (
                "overlay_stabilizer session_id={} source={} source_seq={} context_samples={} "
                "context_secs={:.3f} action={} raw_text={} normalized_text={} "
                "previous_display={} committed_before={} draft_before={} raw_tokens={} "
                "overlap={} committed_after={} draft_after={} current_display={}"
            ),
            context.session_id,
            source,
            source_seq,
            context.context_samples,
            context_secs,
            action,
            json.dumps(raw_text, ensure_ascii=True),
            json.dumps(normalized_text, ensure_ascii=True),
            json.dumps(previous_display, ensure_ascii=True),
            json.dumps(committed_before, ensure_ascii=True),
            json.dumps(draft_before, ensure_ascii=True),
            json.dumps(raw_tokens, ensure_ascii=True),
            overlap,
            json.dumps(committed_after, ensure_ascii=True),
            json.dumps(draft_after, ensure_ascii=True),
            json.dumps(current_display, ensure_ascii=True),
        )


def append_overlay_interim_context(existing: np.ndarray, chunk_audio: np.ndarray) -> np.ndarray:
    if existing.size == 0:
        return np.array(chunk_audio, copy=True)
    return np.concatenate((existing, chunk_audio))


def _streaming_debug_enabled() -> bool:
    raw = os.getenv("PARAKEET_STREAMING_DEBUG", "")
    return raw.strip().casefold() in {"1", "true", "yes", "on"}


def _longest_casefold_suffix_prefix_overlap(existing: list[str], current: list[str]) -> int:
    max_overlap = min(len(existing), len(current))
    for overlap in range(max_overlap, 0, -1):
        if all(
            existing[len(existing) - overlap + index].casefold() == current[index].casefold()
            for index in range(overlap)
        ):
            return overlap
    return 0


__all__ = [
    "OverlayInterimTranscriptContext",
    "OverlayInterimTranscriptStabilizer",
    "StabilizedInterimText",
    "append_overlay_interim_context",
]
