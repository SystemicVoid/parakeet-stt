"""Direct tests for Overlay interim transcript stabilization."""

from __future__ import annotations

import io
from contextlib import contextmanager
from typing import Any
from uuid import uuid4

from loguru import logger

from parakeet_stt_daemon.overlay_interim import (
    OverlayInterimTranscriptContext,
    OverlayInterimTranscriptStabilizer,
    StabilizedInterimText,
)


def _context() -> OverlayInterimTranscriptContext:
    return OverlayInterimTranscriptContext(
        session_id=uuid4(),
        context_samples=16_000,
        sample_rate=16_000,
    )


def _text(stabilized: StabilizedInterimText | None) -> str | None:
    return stabilized.text if stabilized is not None else None


@contextmanager
def _capture_loguru_messages() -> Any:
    buffer = io.StringIO()
    handler_id = logger.add(buffer, format="{message}")
    try:
        yield buffer
    finally:
        logger.remove(handler_id)


def test_live_updates_extend_prior_overlay_text() -> None:
    stabilizer = OverlayInterimTranscriptStabilizer()
    context = _context()

    assert _text(stabilizer.accept("live", 0, "hello", context)) == "hello"
    assert _text(stabilizer.accept("live", 1, "hello world", context)) == "hello world"


def test_stream_path_and_seal_path_overlap_is_reconciled() -> None:
    stabilizer = OverlayInterimTranscriptStabilizer()
    context = _context()

    assert _text(stabilizer.accept("live", 0, "alpha beta", context)) == "alpha beta"
    assert _text(stabilizer.accept("stop_replay", 0, "beta gamma", context)) == "alpha beta gamma"


def test_empty_duplicate_and_no_change_inputs_are_suppressed() -> None:
    stabilizer = OverlayInterimTranscriptStabilizer()
    context = _context()

    assert stabilizer.accept("live", 0, "   ", context) is None
    assert _text(stabilizer.accept("live", 1, " hello   world ", context)) == "hello world"
    assert stabilizer.accept("live", 2, "hello world", context) is None


def test_pending_tail_flush_returns_current_overlay_text() -> None:
    stabilizer = OverlayInterimTranscriptStabilizer()
    context = _context()

    assert stabilizer.flush_pending_tail() is None
    stabilizer.accept("live", 0, "phase one", context)

    assert _text(stabilizer.flush_pending_tail()) == "phase one"


def test_source_sequence_tracking_is_per_source() -> None:
    stabilizer = OverlayInterimTranscriptStabilizer()

    assert stabilizer.next_source_seq("live") == 0
    assert stabilizer.next_source_seq("live") == 1
    assert stabilizer.next_source_seq("stop_replay") == 0


def test_debug_log_emits_stabilizer_decisions_when_streaming_debug_enabled(monkeypatch) -> None:
    monkeypatch.setenv("PARAKEET_STREAMING_DEBUG", "1")
    stabilizer = OverlayInterimTranscriptStabilizer()
    context = _context()

    with _capture_loguru_messages() as log_output:
        stabilizer.accept("live", 0, "alpha beta", context)
        stabilizer.accept("live", 1, "beta gamma", context)

    messages = log_output.getvalue()
    assert "overlay_stabilizer" in messages
    assert f"session_id={context.session_id}" in messages
    assert "source=live source_seq=0" in messages
    assert "source=live source_seq=1" in messages
    assert 'raw_text="beta gamma"' in messages
    assert "overlap=1" in messages
    assert 'current_display="alpha beta gamma"' in messages


def test_debug_log_records_skipped_transcription_source(monkeypatch) -> None:
    monkeypatch.setenv("PARAKEET_STREAMING_DEBUG", "yes")
    stabilizer = OverlayInterimTranscriptStabilizer()
    context = _context()

    with _capture_loguru_messages() as log_output:
        stabilizer.record_skip(
            "stop_replay",
            0,
            context,
            reason="transcribe_error",
            error_class="RuntimeError",
        )

    messages = log_output.getvalue()
    assert "overlay_stabilizer_skip" in messages
    assert "source=stop_replay source_seq=0" in messages
    assert "reason=transcribe_error" in messages
    assert "error_class=RuntimeError" in messages
