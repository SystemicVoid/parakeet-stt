# Streaming Overlay + Seal-Final Injection: Canonical Plan

_Last updated: 2026-02-28_

## Objective
Implement a modern Rust overlay that displays session feedback (and interim text when available) during push-to-talk, while preserving the hard safety guarantee that only `final_result` triggers text injection.

## Non-Negotiable Constraints
- Final text injection remains triggered only by `final_result`.
- Overlay is display-only and never owns paste/injection responsibilities.
- Overlay failures must not interrupt capture, transcription, or final injection.
- Protocol changes must remain backward-compatible during rollout.

## Canonical Local Truth (As Of This Revision)
- Daemon server message types currently include only `session_started`, `final_result`, `error`, and `status`.
- Daemon stop flow emits final output at session end; no realtime overlay event emission path exists yet.
- Daemon Pydantic message models currently use strict `extra="forbid"` behavior.
- Rust client injection boundary is already clean: only `ServerMessage::FinalResult` enqueues injection work.
- Rust protocol decode currently uses tagged enum deserialization; unknown message types are decode errors unless explicitly handled.

## Final Strategy Decisions

### 1) Overlay Stack And Runtime
- Primary backend: Wayland layer-shell (`zwlr_layer_shell_v1`) via Rust Wayland stack.
- Fallback backend: regular Wayland toplevel window (`xdg_toplevel`) with best-effort behavior.
- Runtime model: separate overlay process launched and monitored by `parakeet-ptt`.

### 2) Why This Stack
- Layer-shell is the only path that can provide protocol-level guarantees for overlay z-order/input semantics when compositor support exists.
- Fallback window mode is required for unsupported compositors and remains best-effort by design.
- Separate process preserves crash isolation so overlay failures cannot take down hotkey handling or injection pipeline.

### 3) Protocol Compatibility Requirement (Hard Gate)
- Before daemon emits new overlay message types, Rust client decode path must tolerate unknown `type` values and ignore them gracefully.
- Compatibility behavior must avoid warning spam during mixed-version operation.

## Oracle Confirmation Summary
External architecture sanity-check confirms this plan is the correct direction:
- Layer-shell primary plus fallback window mode is best practice on Linux Wayland.
- Separate process is strongly preferred for reliability and fault containment.
- Unknown-message tolerance must be explicit in the Rust protocol decode path before introducing new daemon events.
- A dedicated worktree + feature branch flow is strongly recommended to keep the current `stt` daily-driver workflow stable during risky overlay development.

## Worktree And Branch Policy (Required For Overlay Work)

### Goal
Keep local offline `stt` usage stable while overlay implementation is in-progress.

### Workflow
1. Keep `main` as stable daily-driver branch.
2. Create a dedicated worktree for overlay development.
3. Implement and test overlay phases only in the overlay worktree/branch.
4. Merge phase-by-phase only after each gate passes.

### Suggested Commands
```bash
git worktree add ../parakeet-overlay-dev feature/overlay-phase0-capability-gate
```

## Delivery Phases And Gates

## Phase 0: Capability And Feasibility Gate (New First Phase)
### Implementation Tasks
- Add a startup capability probe for overlay mode selection:
  - `layer_shell` when `zwlr_layer_shell_v1` is available.
  - `fallback_window` when layer-shell is unavailable but regular window rendering works.
  - `disabled` when neither mode initializes.
- Emit explicit startup log describing effective overlay mode and reason.
- Ensure overlay probe failures are soft-fail and do not affect existing PTT flow.

### Verification Loop
1. Validate deterministic mode selection on Pop!_OS/COSMIC.
2. Validate deterministic fallback behavior on unsupported compositor sessions.
3. Validate that failed probe does not alter start/stop/final injection behavior.

### Gate To Proceed
- Overlay mode classification is deterministic and non-fatal under all tested sessions.

## Phase 1: Protocol Extension And Compatibility Hardening
### Implementation Tasks
- Add new server message variants for overlay-safe realtime updates:
  - `interim_state`
  - `interim_text`
  - `session_ended`
- Include `session_id` on all new variants.
- Add monotonic per-session `seq` on interim variants.
- Preserve existing variants unchanged:
  - `session_started`
  - `final_result`
  - `error`
  - `status`
- Update both sides:
  - `parakeet-stt-daemon/src/parakeet_stt_daemon/messages.py`
  - `parakeet-ptt/src/protocol.rs`
- Add explicit unknown-message tolerance path in `parakeet-ptt` websocket decode flow.

### Verification Loop
1. Add schema and decode changes before enabling emission.
2. Run focused serialization/deserialization tests.
3. Verify unknown message types are ignored safely.
4. Verify old client against new daemon behavior (no crashes, bounded logs).
5. Verify new client against old daemon behavior.

### Essential Tests
- Daemon message model tests:
  - `interim_text` required fields and validation.
  - `interim_state` enum/state validation.
  - `seq` monotonic constraints and non-negative validation.
- Rust protocol tests:
  - decode each new variant.
  - round-trip all known variants.
  - unknown `type` handling path remains non-fatal.

### Gate To Proceed
- Mixed-version protocol matrix passes with no functional regressions.

## Phase 2: Daemon Emission Path (State-First, Then Text)
### Implementation Tasks
- Introduce emission behind feature flag/environment control, default off.
- Emit `interim_state` transitions first (`listening`, `processing`, etc.).
- Emit `interim_text` only when validated incremental source is available.
- Emit `session_ended` on normal completion and abort/error paths.
- Keep `final_result` generation path unchanged.
- Add structured counters for emitted and dropped overlay events.

### Verification Loop
1. Validate state-only emission path.
2. Validate optional interim-text path.
3. Validate no cross-session event leakage.
4. Validate stop/abort race behavior.
5. Validate exactly one `final_result` per session.

### Essential Tests
- Extend `parakeet-stt-daemon/tests/test_streaming_truth.py`:
  - in-order interim event behavior.
  - state-only fallback behavior.
- Add `parakeet-stt-daemon/tests/test_overlay_event_stream.py`:
  - no events after `session_ended`.
  - no cross-session leakage.
  - final_result exactly once.
- Extend `parakeet-stt-daemon/tests/test_session_cleanup.py`:
  - abort paths emit terminal event and clean state.

### Gate To Proceed
- Emission path passes all invariant tests without changing final-result behavior.

## Phase 3: PTT Routing + Injection Hard Boundary
### Implementation Tasks
- Extend `handle_server_message` in `parakeet-ptt/src/main.rs`:
  - route `interim_state`, `interim_text`, and `session_ended` to overlay sink only.
  - keep enqueue logic only in `FinalResult` arm.
- Add explicit routing counters/metrics proving interim messages never reach injector queue.
- Add overlay sink adapter interface for deterministic tests.
- Add stale/out-of-order `seq` dropping logic in overlay path.

### Verification Loop
1. Add exhaustive match coverage.
2. Run existing queue/injector tests for regressions.
3. Add adversarial mixed-stream tests.
4. Validate queue counters change only on final results.
5. Validate state reset semantics remain unchanged.

### Essential Tests
- Extend `parakeet-ptt/src/main.rs` tests:
  - interim/state/ended events do not enqueue injection jobs.
  - final result still enqueues exactly once.
  - mixed stream enqueues exactly once.
  - out-of-order interim seq is dropped on overlay path only.

### Gate To Proceed
- Injection boundary remains mathematically unchanged: enqueue count only tracks `final_result`.

## Phase 4: Overlay Process MVP (Separate Process)
### Implementation Tasks
- Add overlay binary target under `parakeet-ptt` workspace.
- Implement deterministic overlay state machine first:
  - `hidden`, `listening`, `interim`, `finalizing`.
- Event intake from PTT via local IPC (newline-delimited JSON over child process stdio or UDS).
- Backend selection:
  - primary: layer-shell.
  - fallback: regular best-effort window.
- Behavior requirements:
  - hidden when idle.
  - auto-hide after final/session end timeout.
  - no injection, no clipboard, no keyboard ownership.
- Add configuration fields (opacity/font/anchor/margins/max width/lines) with conservative defaults.

### Verification Loop
1. Validate state machine headlessly.
2. Validate transitions from fake event streams.
3. Validate rendering integration for key states.
4. Run overlay crash/restart simulation.
5. Validate CPU/memory budget under 10+ minute dictation run.

### Essential Tests
- Overlay state-machine tests:
  - transition invariants.
  - stale sequence drop behavior.
  - auto-hide timer behavior.
- PTT integration tests:
  - overlay disconnect has zero impact on final injection path.
  - overlay reconnect consumes current valid state only.

### Gate To Proceed
- Overlay process may fail arbitrarily without affecting capture/transcription/final injection.

## Phase 5: Config, Feature Flags, and Rollout Controls
### Implementation Tasks
- Add `PARAKEET_OVERLAY_ENABLED` and CLI equivalent.
- Add mode control (`auto`, `layer-shell`, `fallback`, `off`) if needed.
- Keep overlay disabled by default initially.
- Ensure startup logs include effective overlay mode and fallback reason.

### Verification Loop
1. Verify defaults preserve baseline behavior bit-for-bit.
2. Validate env/CLI precedence.
3. Validate invalid config handling.
4. Validate soft-fail when overlay binary missing.

### Essential Tests
- Rust config precedence tests in `parakeet-ptt/src/config.rs`.
- Startup mode tests:
  - enabled+available.
  - enabled+missing binary.
  - disabled.

### Gate To Proceed
- Overlay remains opt-in and cannot break baseline runtime when disabled.

## Phase 6: End-To-End Reliability And Promotion Gates
### Implementation Tasks
- Add E2E runner scenarios:
  - quick utterance.
  - long dictation.
  - abort mid-session.
  - daemon reconnect.
  - overlay crash mid-session.
  - mixed-version protocol compatibility.
- Add acceptance thresholds for latency and final injection reliability.

### Verification Loop
1. Run E2E on every protocol-affecting PR.
2. Capture artifacts/logs for failures.
3. Require repeated clean runs before promotion.
4. Re-run stream+seal quality checks to ensure no WER/latency regression.

### Essential Tests
- Add integration harness file(s), e.g.:
  - `parakeet-stt-daemon/tests/test_overlay_e2e_contract.py`
- Extend benchmark harness assertions:
  - `parakeet-stt-daemon/tests/test_offline_benchmark_harness.py`

### Gate To Proceed
- Promotion requires repeated clean runs and zero regressions to final-result correctness.

## External Reference Notes (Advisory, Not Source Of Truth)
- A GTK/layer-shell Whisper overlay reference implementation validates practical tactics:
  - layer-shell initialization and overlay layer usage.
  - keyboard mode disabled.
  - explicit click-through input region.
  - visibility/show-hide workarounds for compositor quirks.
- These patterns are informative only and must be adapted to this repo’s Rust architecture and constraints.

## Verification Commands
- Rust unit/integration:
  - `cd parakeet-ptt && cargo test`
- Python daemon tests:
  - `cd parakeet-stt-daemon && uv run pytest`
- Targeted daemon pass:
  - `cd parakeet-stt-daemon && uv run pytest tests/test_streaming_truth.py tests/test_session_cleanup.py`
- Protocol/message targeted pass:
  - `cd parakeet-stt-daemon && uv run pytest -k "message or overlay_event_stream"`
  - `cd parakeet-ptt && cargo test protocol`
- Repo quality gates before merge:
  - `prek run --all-files`
  - `prek run --stage pre-push --all-files`

## Definition Of Done
- Overlay shows session feedback with stable UX in supported environments.
- `final_result` remains the sole source of paste/injection.
- Interim-event flood/race conditions do not affect injection correctness.
- Overlay process failures are fully isolated from STT and injection path.
- Mixed-version compatibility is verified.
- Feature-flag rollout path is validated and documented in PR notes.

## Suggested PR Slicing (Atomic)
1. Phase 0 capability gate + logging.
2. Phase 1 protocol and unknown-message tolerance.
3. Phase 2 daemon emission behind flag + tests.
4. Phase 3 PTT routing guards + boundary tests.
5. Phase 4 overlay binary MVP + state-machine tests.
6. Phase 5 config/flag wiring + precedence tests.
7. Phase 6 E2E/reliability harness + rollout defaults.
