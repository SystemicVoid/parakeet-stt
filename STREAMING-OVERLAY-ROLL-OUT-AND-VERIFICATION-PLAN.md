# Streaming Overlay + Seal-Final Injection: Execution Plan

## Objective
Implement a modern, minimal Rust overlay that displays live session feedback (and interim text when available) during push-to-talk, while preserving the existing safety guarantee that only Seal final results are injected/pasted.

## Non-Negotiable Constraints
- Final text injection remains triggered only by `final_result`.
- Overlay is display-only and must never have injection responsibilities.
- Overlay failures must not interrupt capture, transcription, or final injection.
- Protocol changes must stay backward-compatible during rollout.

## Current Baseline (Code Anchors)
- Daemon protocol models: `parakeet-stt-daemon/src/parakeet_stt_daemon/messages.py`
- Daemon session/server flow: `parakeet-stt-daemon/src/parakeet_stt_daemon/session.py`, `parakeet-stt-daemon/src/parakeet_stt_daemon/server.py`
- PTT protocol decode path: `parakeet-ptt/src/protocol.rs`
- PTT server message handling/injection queue: `parakeet-ptt/src/main.rs`
- Existing injector reliability tests: `parakeet-ptt/src/main.rs` (`#[cfg(test)]` section)

## Delivery Phases

## Phase 1: Protocol Extension for Overlay Events
### Implementation Tasks
- Add new server message variants for overlay-safe realtime updates:
  - `interim_text`
  - `interim_state`
  - `session_ended`
- Include `session_id` on all new variants.
- Add a per-session monotonic `seq` to interim variants so clients can drop stale/out-of-order updates.
- Keep existing variants unchanged:
  - `session_started`
  - `final_result`
  - `error`
  - `status`
- Update both sides:
  - `parakeet-stt-daemon/src/parakeet_stt_daemon/messages.py`
  - `parakeet-ptt/src/protocol.rs`

### Robust Verification Loop
1. Add/adjust schema first in daemon and Rust protocol model.
2. Run focused serialization/deserialization tests immediately.
3. Re-run with unknown/new fields to ensure tolerant behavior where expected.
4. Confirm old clients still parse messages when daemon does not emit new types.
5. Confirm new clients tolerate daemon versions that do not emit new types.

### Essential Tests to Add
- Daemon unit tests (new file or existing protocol tests):
  - `interim_text` validates required fields and rejects extras.
  - `interim_state` validates enumerated states.
  - `seq` must be non-negative and present.
- Rust protocol tests in `parakeet-ptt/src/protocol.rs`:
  - decode each new variant from JSON.
  - round-trip serialize/deserialize for all variants.
  - verify unknown message type behavior is explicit and logged.

## Phase 2: Daemon Emission Path (Stream + Seal Preserved)
### Implementation Tasks
- In session processing loop, emit `interim_text` only when partial stream is healthy.
- If partial stream unavailable/fails preflight, emit `interim_state` transitions instead (e.g. `listening`, `processing`, `partial_unavailable`).
- Emit `session_ended` for both normal completion and abort/error paths.
- Keep existing finalize behavior and `final_result` generation untouched.
- Add lightweight event counters in structured logs for interim emissions and drop reasons.

### Robust Verification Loop
1. Introduce emission behind feature gate/env (default off initially).
2. Validate happy path with synthetic audio chunks and partials enabled.
3. Force partial preflight failure and verify graceful status-only fallback.
4. Verify stop/abort races do not emit cross-session events (session_id guard).
5. Confirm only one final result emitted per session.

### Essential Tests to Add
- `parakeet-stt-daemon/tests/test_streaming_truth.py` (extend):
  - interim events emitted in-order for a session.
  - fallback path emits state updates but no interim text.
- New daemon test file: `parakeet-stt-daemon/tests/test_overlay_event_stream.py`:
  - no events after `session_ended`.
  - no cross-session leakage under back-to-back sessions.
  - `final_result` still emitted exactly once.
- Extend `parakeet-stt-daemon/tests/test_session_cleanup.py`:
  - aborted session always emits terminal state/ended and cleans buffers.

## Phase 3: PTT Routing + Injection Hard Boundary
### Implementation Tasks
- Extend `handle_server_message` in `parakeet-ptt/src/main.rs`:
  - route `interim_text` and `interim_state` to overlay channel only.
  - continue queuing injection jobs only for `FinalResult`.
- Add explicit guard rails (match arms + metrics) to prove interim messages never hit injector queue.
- Add a small adapter interface for overlay sink to keep tests deterministic.

### Robust Verification Loop
1. Add routing changes with exhaustive match coverage.
2. Run existing injection queue tests to detect regressions.
3. Add adversarial tests: flood interim events while final arrives.
4. Confirm queue depth/counters only change on final results.
5. Verify state reset behavior unchanged on final/error.

### Essential Tests to Add
- Extend tests in `parakeet-ptt/src/main.rs`:
  - interim message does not enqueue injection job.
  - final result does enqueue injection job.
  - mixed stream (`interim*`, `status`, `final`) enqueues exactly once.
  - out-of-order interim seq is dropped by overlay path and does not affect injection.

## Phase 4: Rust Overlay Process (MVP)
### Implementation Tasks
- Add a new binary crate or bin target under `parakeet-ptt` workspace for overlay.
- MVP UX:
  - always-on-top translucent window.
  - hidden when idle.
  - states: listening, interim text, finalizing, hidden.
  - auto-hide timeout after final/session end.
- Implement event intake from PTT over local IPC (preferred) or in-process channel if same binary.
- Include configuration surface for opacity, font size, max width/lines, anchor, and margins.

### Robust Verification Loop
1. Build a deterministic overlay state machine first (headless-testable).
2. Validate transitions with fake event streams before UI binding.
3. Add rendering integration tests for key states.
4. Run crash/restart simulation: overlay kill should not break PTT session.
5. Validate CPU/memory envelope during 10+ minute dictation simulation.

### Essential Tests to Add
- New tests in overlay crate:
  - state machine transition tests.
  - stale sequence drop tests.
  - auto-hide timer behavior tests.
- PTT integration test:
  - overlay disconnected => no impact on final_result injection path.
  - overlay reconnect during active session consumes latest valid state.

## Phase 5: Config, Feature Flags, and Backward Compatibility
### Implementation Tasks
- Add `PARAKEET_OVERLAY_ENABLED` + CLI flag equivalent.
- Add overlay config fields with conservative defaults.
- Keep disabled-by-default initially.
- Ensure startup logs clearly describe whether overlay is enabled and attached.

### Robust Verification Loop
1. Verify defaults preserve existing behavior bit-for-bit.
2. Validate env var, CLI, and config precedence.
3. Validate invalid config values fail fast with actionable errors.
4. Validate operation when overlay binary missing (soft-fail if enabled).

### Essential Tests to Add
- Config precedence tests:
  - add/extend in `parakeet-stt-daemon/tests/test_cli_precedence.py` (if daemon-side flags involved).
  - add Rust config tests in `parakeet-ptt/src/config.rs`.
- Startup mode tests:
  - overlay enabled+available.
  - overlay enabled+missing binary.
  - overlay disabled.

## Phase 6: End-to-End Reliability and Regression Suite
### Implementation Tasks
- Add scripted E2E scenario runner for:
  - quick utterance.
  - long dictation.
  - abort mid-session.
  - daemon reconnect.
  - overlay process crash mid-session.
- Add acceptance criteria thresholds for latency and final injection success.

### Robust Verification Loop
1. Run E2E suite on every protocol-affecting PR.
2. Capture logs/artifacts for failed runs.
3. Gate rollout on repeated clean runs (e.g. 10 consecutive).
4. Re-run existing stream+seal quality checks to ensure WER/latency not regressed.

### Essential Tests to Add
- New integration harness test file(s):
  - `parakeet-stt-daemon/tests/test_overlay_e2e_contract.py` (or equivalent location for cross-process tests).
- Extend existing benchmark harness checks:
  - `parakeet-stt-daemon/tests/test_offline_benchmark_harness.py` with no-regression assertions for final output path.

## Verification Commands (Per Phase)
- Rust unit/integration:
  - `cd parakeet-ptt && cargo test`
- Python daemon tests:
  - `cd parakeet-stt-daemon && uv run pytest`
- Targeted daemon tests during iteration:
  - `cd parakeet-stt-daemon && uv run pytest tests/test_streaming_truth.py tests/test_session_cleanup.py`
- Protocol/message targeted pass:
  - `cd parakeet-stt-daemon && uv run pytest -k "message or overlay_event_stream"`
  - `cd parakeet-ptt && cargo test protocol`
- Repo quality gates before merge:
  - `prek run --all-files`
  - `prek run --stage pre-push --all-files`

## Definition of Done
- Overlay shows session feedback (and interim text when available) with stable UX.
- Seal final result remains the sole source for paste/injection.
- Interim-event flood/race conditions do not affect final injection correctness.
- Overlay process failures are fully isolated from STT and injection path.
- Full test suite additions above are implemented and passing.
- Feature flag rollout path is validated and documented in release notes/PR description.

## Suggested PR Slicing (Atomic)
1. Protocol message additions + schema tests.
2. Daemon emission + fallback behavior tests.
3. PTT routing guards + injection boundary tests.
4. Overlay crate MVP + state-machine tests.
5. Config/flag wiring + precedence tests.
6. E2E/reliability harness + rollout defaults.
