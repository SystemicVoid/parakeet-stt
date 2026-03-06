# Architectural Review Grounding And Implementation Handoff

Archived on 2026-03-06 after the review action items were resolved.

## Status

Implemented on 2026-03-06:

- `P0` landed in `5ad4ee6` (`fix(daemon): finalize from canonical session audio`).
- `P1` landed in `a79dd94` (`fix(daemon): serialize websocket output`).
- `P1` landed in `bb071db` (`fix(injector): bound blocking execution and classify timeouts`).
- `P2` landed in `cb70590` (`feat(daemon): harden VAD runtime truth`).
- Repo guidance for daemon Python command context / hook behavior landed in `aa5e408` (`docs(repo): note daemon python command context`).

Remaining highest-ROI follow-up:

- `P3` helper/finalization diagnostics cleanup

## Grounded Conclusions

| Topic | Verdict | Current state |
| --- | --- | --- |
| Final transcript source of truth | Fixed | Finalization now uses canonical `audio_samples` from `AudioInput.stop_session_with_streaming()`. `_active_stream` is no longer authoritative for stop-time correctness. |
| Daemon websocket ordering | Fixed | Outbound websocket writes now go through a per-connection send lock. The daemon, not the client, now owns message order. |
| Late overlay after stop/final | Fixed in daemon | Overlay sessions now transition through explicit `active -> terminal -> ended` states. Late `interim_*` and `audio_level` emissions are dropped once final emission begins. |
| Client ghost overlay diagnosis | Partly corrected | The likely cause was concurrent daemon sends plus permissive terminal handling, not TCP packet reordering on one websocket. |
| Rust injector hang risk | Fixed | Blocking command paths now have bounded execution time, timed-out subprocesses are killed/reaped, and the worker classifies execution timeout separately from backend failure. |
| VAD production readiness | Fixed | `onnxruntime` is now a daemon dependency, VAD loads/warmups at startup when enabled, `/status` exposes `vad_enabled` / `vad_active` / `vad_fallback_reason`, and trim work is off the event loop. |

## What Landed

### P0: Canonical final transcript path

Why it mattered:

- Stop-time correctness depended on a lossy mirror fed by the drain task.
- The daemon already had the full session audio; using anything else for finalization was unnecessary risk.

What changed:

- `DaemonServer._handle_stop()` now treats the captured session buffer as the only finalization source of truth.
- `DaemonServer._finalise_transcription()` now transcribes canonical `audio_samples`.
- `_active_stream` still supports streaming/interim behavior, but no longer controls final correctness.

Regression coverage:

- `parakeet-stt-daemon/tests/test_offline_in_memory_transcription.py::test_server_finalize_uses_canonical_audio_even_when_stream_session_exists`

### P1: Serialized websocket output and terminal overlay gating

Why it mattered:

- The daemon could write to one websocket from both the stop/finalize path and the background drain task.
- That made overlay ordering scheduler-dependent and allowed stale late-interim emissions to race with final/session-end events.

What changed:

- All daemon websocket writes now pass through a per-websocket async send lock.
- Overlay emission tracks per-session terminal state in the daemon.
- Once final emission starts, later `interim_state`, `interim_text`, and `audio_level` messages for that session are dropped.
- `session_ended` remains the terminal overlay event.

Regression coverage:

- `parakeet-stt-daemon/tests/test_overlay_event_stream.py::test_late_live_interim_is_dropped_once_final_send_begins`

### P1: Rust injector timeout and worker recovery

Why it mattered:

- Queue timeout only covered enqueue pressure; it did not protect the worker once a blocking injector backend started running.
- A hung `ydotool` / `wl-copy` / `wl-paste` path could stall later injection jobs until client restart.

What changed:

- Command-based injector paths now run with explicit bounded execution time.
- Timed-out subprocesses are killed and reaped explicitly.
- Worker reports now distinguish `backend_failure`, `execution_timeout`, and `worker_task_failed`.
- A timed-out job no longer prevents later jobs from succeeding.

Regression coverage:

- `parakeet-ptt/src/injector.rs::tests::timed_out_ydotool_backend_fails_fast_and_chain_recovers`
- `parakeet-ptt/src/main.rs::tests::injector_worker_recovers_after_execution_timeout`

### P2: VAD runtime truth and startup readiness

Why it mattered:

- Opt-in VAD existed, but operators could not tell whether it was actually usable at runtime.
- First-use VAD load and trim work were still happening on the stop/finalize path.

What changed:

- Added `onnxruntime` to daemon runtime dependencies so `load_silero_vad(onnx=True)` works in the default environment.
- Daemon startup and `--check` now preload/warm VAD when `PARAKEET_VAD_ENABLED=true`.
- `/status` and runtime logs now expose `vad_enabled`, `vad_active`, and `vad_fallback_reason`.
- `_finalise_transcription()` now offloads tail trimming to the executor before offline decode.

Regression coverage:

- `parakeet-stt-daemon/tests/test_streaming_truth.py::test_status_vad_enabled_pending_load_is_explicit`
- `parakeet-stt-daemon/tests/test_streaming_truth.py::test_status_vad_enabled_and_loaded_is_active`
- `parakeet-stt-daemon/tests/test_streaming_truth.py::test_prepare_vad_marks_missing_dependency_explicitly`
- `parakeet-stt-daemon/tests/test_offline_in_memory_transcription.py::test_server_finalize_offloads_tail_trim_off_event_loop`

## Verification

Focused runs completed after the fixes:

- `cd parakeet-stt-daemon && uv run pytest tests/test_offline_in_memory_transcription.py tests/test_overlay_event_stream.py tests/test_session_cleanup.py tests/test_streaming_truth.py`
  - Result: `49 passed`
- `cd parakeet-stt-daemon && uv run pytest tests/test_streaming_truth.py tests/test_offline_in_memory_transcription.py`
  - Result: `24 passed`
- `cd parakeet-stt-daemon && uv run ruff check src/parakeet_stt_daemon/server.py tests/test_overlay_event_stream.py`
  - Result: passed
- `cd parakeet-stt-daemon && uv run ruff check src/parakeet_stt_daemon/server.py src/parakeet_stt_daemon/messages.py src/parakeet_stt_daemon/__main__.py tests/test_streaming_truth.py tests/test_offline_in_memory_transcription.py`
  - Result: passed
- `cd parakeet-stt-daemon && uv run python - <<'PY' ... load_silero_vad(onnx=True) ... PY`
  - Result: loads successfully (`OnnxWrapper`)
- `cd parakeet-ptt && cargo test`
  - Result: `155 passed`

## Remaining Work, Ranked By ROI

### 1. P3: Clarify helper/runtime diagnostics

Why next:

- Streaming helper truth is now accurate, but the naming still invites a wrong mental model during incident review.

Implementation direction:

- Clarify that `stream_helper_active` is about helper availability, not final transcript ownership.
- Add a separate explicit finalization-path signal such as `finalization_mode=offline_seal`.
- Update status/log wording so operators can distinguish:
  - live interim source/helper state
  - final transcript source/path
  - VAD trim state

## Tracker

- [x] `P0` Canonical final transcript path uses `audio_samples`, not `_active_stream`
- [x] `P0` Add regression test proving finalization ignores the streaming mirror and uses canonical audio
- [x] `P1` Add per-connection websocket send serialization
- [x] `P1` Add terminal overlay gating so no interim events appear after final/session end
- [x] `P1` Add Rust injector execution timeout and worker recovery
- [x] `P2` Decide VAD support policy
- [x] `P2` Add missing runtime dependency and startup warm/load path
- [x] `P2` Offload trim work off the event loop
- [ ] `P3` Clarify `stream_helper_active` and finalization-path diagnostics

## Minimal Validation Checklist For Remaining Work

Python daemon:

- add coverage for any new finalization-path status field or log wording
- verify `/status` and startup logs clearly separate helper state from final transcript ownership
