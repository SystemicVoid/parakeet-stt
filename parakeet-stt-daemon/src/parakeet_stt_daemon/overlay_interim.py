"""Stabilize Overlay interim text from the Stream path and Seal path.

The Daemon feeds raw Stream path and stop-replay Seal path candidates into this
module during one Session. The module owns the mutable Overlay transcript tail,
source sequence counters, overlap reconciliation, pending-tail flush behavior,
and debug logging for interim text decisions.
"""

from __future__ import annotations

import json
import os
from collections.abc import Awaitable, Callable
from dataclasses import dataclass, field
from typing import Literal
from uuid import UUID

import numpy as np
from loguru import logger

InterimTranscriptSource = Literal["live", "stop_replay"]
InterimTranscriber = Callable[[np.ndarray], Awaitable[str]]


@dataclass(frozen=True)
class OverlayInterimTranscriptContext:
    session_id: UUID
    context_samples: int
    sample_rate: int


@dataclass(frozen=True)
class InterimTranscriptRuntimeFacts:
    enabled: bool
    last_source: InterimTranscriptSource | None
    live_chunks_processed: int
    live_updates_emitted: int
    live_failed: bool
    stop_replay_chunks_processed: int
    stop_replay_updates_emitted: int
    stop_replay_failed: bool
    source_fallback_reason: str | None


@dataclass(frozen=True)
class StabilizedInterimText:
    text: str


@dataclass
class _OverlayInterimTranscriptState:
    committed_tokens: list[str] = field(default_factory=list)
    draft_tokens: list[str] = field(default_factory=list)


@dataclass
class _InterimTranscriptSourceState:
    rolling_audio: np.ndarray = field(default_factory=lambda: np.zeros((0,), dtype=np.float32))
    chunks_processed: int = 0
    updates_emitted: int = 0
    failed: bool = False
    fallback_reason: str | None = None


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


class OverlayInterimTranscriptSession:
    """Own one Session's user-visible interim transcript production."""

    def __init__(
        self,
        *,
        session_id: UUID,
        sample_rate: int,
        enabled: bool,
    ) -> None:
        self._session_id = session_id
        self._sample_rate = int(sample_rate)
        self._enabled = bool(enabled)
        self._stabilizer = OverlayInterimTranscriptStabilizer()
        self._source_state: dict[InterimTranscriptSource, _InterimTranscriptSourceState] = {
            "live": _InterimTranscriptSourceState(),
            "stop_replay": _InterimTranscriptSourceState(),
        }
        self._last_source: InterimTranscriptSource | None = None
        self._source_fallback_reason: str | None = None

    @property
    def runtime_facts(self) -> InterimTranscriptRuntimeFacts:
        live = self._source_state["live"]
        stop_replay = self._source_state["stop_replay"]
        return InterimTranscriptRuntimeFacts(
            enabled=self._enabled,
            last_source=self._last_source,
            live_chunks_processed=live.chunks_processed,
            live_updates_emitted=live.updates_emitted,
            live_failed=live.failed,
            stop_replay_chunks_processed=stop_replay.chunks_processed,
            stop_replay_updates_emitted=stop_replay.updates_emitted,
            stop_replay_failed=stop_replay.failed,
            source_fallback_reason=self._source_fallback_reason,
        )

    async def accept_live_chunk(
        self,
        chunk: np.ndarray,
        transcribe: InterimTranscriber,
    ) -> str | None:
        """Return the next visible live interim text update for a chunk, if any."""
        return await self._accept_source_chunk("live", chunk, transcribe)

    async def collect_stop_replay_updates(
        self,
        ready_chunks: list[np.ndarray],
        transcribe: InterimTranscriber,
    ) -> list[str]:
        """Return stop-replay interim updates plus the final pending-tail flush."""
        updates: list[str] = []
        state = self._source_state["stop_replay"]
        for chunk in ready_chunks:
            if state.failed:
                break
            update = await self._accept_source_chunk("stop_replay", chunk, transcribe)
            if update is not None:
                updates.append(update)
        flushed = self.flush_pending_tail()
        if flushed is not None:
            updates.append(flushed)
        return updates

    def flush_pending_tail(self) -> str | None:
        flushed = self._stabilizer.flush_pending_tail()
        return flushed.text if flushed is not None else None

    async def _accept_source_chunk(
        self,
        source: InterimTranscriptSource,
        chunk: np.ndarray,
        transcribe: InterimTranscriber,
    ) -> str | None:
        if not self._enabled:
            return None

        state = self._source_state[source]
        if state.failed:
            return None

        chunk_audio = np.asarray(chunk, dtype=np.float32).reshape(-1)
        if chunk_audio.size == 0:
            return None

        state.chunks_processed += 1
        state.rolling_audio = append_overlay_interim_context(state.rolling_audio, chunk_audio)
        source_seq = self._stabilizer.next_source_seq(source)
        context = self._context(int(state.rolling_audio.size))
        try:
            candidate = await transcribe(state.rolling_audio)
        except Exception as exc:  # noqa: BLE001
            self._record_source_failure(source, source_seq, context, exc.__class__.__name__)
            return None

        stabilized = self._stabilizer.accept(source, source_seq, candidate, context)
        if stabilized is None:
            return None

        state.updates_emitted += 1
        self._last_source = source
        return stabilized.text

    def _context(self, context_samples: int) -> OverlayInterimTranscriptContext:
        return OverlayInterimTranscriptContext(
            session_id=self._session_id,
            context_samples=context_samples,
            sample_rate=self._sample_rate,
        )

    def _record_source_failure(
        self,
        source: InterimTranscriptSource,
        source_seq: int,
        context: OverlayInterimTranscriptContext,
        error_class: str,
    ) -> None:
        state = self._source_state[source]
        state.failed = True
        state.fallback_reason = f"{source}_transcribe_error:{error_class}"
        self._source_fallback_reason = state.fallback_reason
        logger.debug(
            "Interim transcript source {} unavailable for session {}: {}",
            source,
            self._session_id,
            error_class,
        )
        self._stabilizer.record_skip(
            source=source,
            source_seq=source_seq,
            context=context,
            reason="transcribe_error",
            error_class=error_class,
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
    "InterimTranscriptRuntimeFacts",
    "InterimTranscriptSource",
    "InterimTranscriber",
    "OverlayInterimTranscriptContext",
    "OverlayInterimTranscriptSession",
    "OverlayInterimTranscriptStabilizer",
    "StabilizedInterimText",
    "append_overlay_interim_context",
]
