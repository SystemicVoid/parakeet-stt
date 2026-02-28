# Streaming Overlay + Seal-Final Injection: Canonical Plan

_Last updated: 2026-02-28_

## Progress Tracker
- [x] Worktree policy in effect (`../parakeet-overlay-dev` on `feature/overlay-phase0-capability-gate`).
- [x] Phase 0: Capability and feasibility gate implemented in `parakeet-ptt`.
- [x] Phase 1: Protocol extension + unknown-message compatibility hardening.
- [x] Phase 2: Daemon emission path behind rollout controls.
- [x] Phase 3: PTT routing + hard injection boundary proofs.
- [ ] Phase 4: Overlay process MVP (separate process).
- [x] Phase 4 Slice A: separate overlay binary + deterministic state machine + IPC wiring + failure-isolation tests.
- [x] Phase 4 Slice B: overlay respawn manager + reconnect state replay guarantees.
- [x] Phase 4 Slice C: layer-shell/fallback render integration path + unsupported-backend noop safety.
- [x] Phase 4 Slice D: crash/restart simulation through PTT routing path + reconnect/final-injection proof.
- [x] Phase 4 Slice E: layer-shell text rendering + COSMIC fallback guardrails.
- [ ] Phase 5: Config/flags/rollout controls.
- [ ] Phase 6: E2E reliability and promotion gates.

## Implementation Log
- 2026-02-28: Established dedicated overlay worktree/branch from latest `origin/main`.
- 2026-02-28: Completed Phase 0 classification and startup logging in `parakeet-ptt` with soft-fail behavior.
- 2026-02-28: Added deterministic classification tests for `layer_shell`, `fallback_window`, and `disabled` modes.
- 2026-02-28: Confirmed no injection semantic change (`final_result` remains sole enqueue trigger).
- 2026-02-28: Phase 1 (PTT side) added protocol variants and unknown-`type` tolerance in websocket decode paths.
- 2026-02-28: Phase 1 (daemon side) added message schemas for `interim_state`, `interim_text`, and `session_ended` with strict validation.
- 2026-02-28: Added daemon message-model tests for required fields, enum validation, and non-negative `seq`.
- 2026-02-28: Added Rust protocol round-trip test coverage for all currently known server message variants.
- 2026-02-28: Completed mixed-version compatibility matrix checks across `main` and `feature/overlay-phase0-capability-gate`.
- 2026-02-28: Verified old-client + new-daemon safety gate by confirming Phase 1 daemon changes are schema/test only (no runtime emission path changes).
- 2026-02-28: Ran stream-seal eval compare in overlay worktree with personal corpus; results stayed within baseline acceptance thresholds.
- 2026-02-28: Phase 2 started in daemon: state-first overlay emission path added behind `PARAKEET_OVERLAY_EVENTS_ENABLED` (default off).
- 2026-02-28: Added structured overlay counters (`overlay_events_emitted`, `overlay_events_dropped`) and surfaced them through status/runtime logs.
- 2026-02-28: Added overlay event stream invariants test suite covering ordering, per-session seq reset, abort terminal event, and non-fatal overlay send failures.
- 2026-02-28: Completed Phase 2 optional `interim_text` emission from validated incremental runtime chunks, still gated by `PARAKEET_OVERLAY_EVENTS_ENABLED`.
- 2026-02-28: Added daemon safeguards so incremental-source failures degrade silently and never block `final_result` or `session_ended`.
- 2026-02-28: Completed Phase 3 in `parakeet-ptt` by routing `interim_state`, `interim_text`, and `session_ended` into a dedicated overlay sink adapter path with per-route counters.
- 2026-02-28: Added overlay-path stale/out-of-order `seq` dropping and session-mismatch drops without changing state reset behavior or final-result enqueue semantics.
- 2026-02-28: Added Phase 3 boundary tests proving queue enqueue counters move only on `final_result`, including mixed-stream and adversarial stale-sequence coverage.
- 2026-02-28: Started Phase 4 with a new `parakeet-overlay` process target in `parakeet-ptt` (`src/bin/parakeet-overlay.rs`) and explicit `[[bin]]` entries in `Cargo.toml`.
- 2026-02-28: Added shared overlay IPC model (`src/overlay_ipc.rs`) and deterministic overlay state machine (`src/overlay_state.rs`) covering `hidden`, `listening`, `interim`, and `finalizing`.
- 2026-02-28: Wired runtime overlay process spawning from `parakeet-ptt` via NDJSON child-stdio IPC (`src/overlay_process.rs` + `src/main.rs`) with soft-fail fallback to noop sink.
- 2026-02-28: Added Phase 4 isolation proof that overlay channel disconnect cannot block `final_result` injection enqueue/worker completion (`overlay_disconnect_does_not_block_final_result_injection`).
- 2026-02-28: Added `OverlayProcessManager` in `parakeet-ptt` to auto-respawn overlay child processes after disconnect with bounded retry backoff and non-fatal failure behavior.
- 2026-02-28: Added reconnect replay contract to send only the latest valid overlay state after process recovery (no backlog replay), preserving final-result injection boundary isolation.
- 2026-02-28: Implemented real overlay renderer initialization in `parakeet-overlay` for both layer-shell (`zwlr_layer_shell_v1`) and fallback window (`xdg_toplevel`) paths with shared-memory surface commits.
- 2026-02-28: Added deterministic render-intent mapping (`OverlayVisibility -> OverlayRenderIntent`) and backend-selection tests proving unsupported/probe-failure paths degrade to noop safely.
- 2026-02-28: Added crash/restart simulation in `parakeet-ptt/src/main.rs` proving overlay child disconnect + respawn replays only current interim state while final-result enqueue/injection behavior remains unchanged.
- 2026-02-28: Added overlay text rendering path in `parakeet-overlay` (font descriptor parsing, system-font resolution via `fontdb`, glyph rasterization via `fontdue`, and bounded line-layout clamping).
- 2026-02-28: Added COSMIC fallback guardrail warning when `fallback-window` is selected and updated `justfile.overlay-dev` runbook to prefer `start-layer-shell` with fallback marked diagnostic-only.
- 2026-02-28: Fixed Slice E COSMIC regressions by resolving generic/system font fallbacks for `Sans 16`, keeping layer-shell surfaces mapped with transparent hidden frames (instead of null-buffer detach), and failing fast on renderer errors so PTT respawn semantics recover overlay safely.

## Verification Ledger
- 2026-02-28 (Phase 1 matrix): `cd parakeet-ptt && cargo test protocol` passed on overlay branch (6 protocol tests) and on `main` baseline (1 protocol test).
- 2026-02-28 (Phase 1 matrix): `cd parakeet-stt-daemon && uv run pytest tests/test_messages.py tests/test_streaming_truth.py tests/test_session_cleanup.py` passed on overlay branch (32 tests).
- 2026-02-28 (Phase 1 matrix): `cd parakeet-stt-daemon && uv run pytest tests/test_streaming_truth.py tests/test_session_cleanup.py` passed on `main` baseline (28 tests).
- 2026-02-28 (eval regression): `just eval compare` passed with personal corpus in overlay worktree.
- 2026-02-28 (eval regression): offline vs stream-seal deltas were WER `+0.000569`, strict command exact match `-0.010000`, critical token recall `+0.001297`, and warm finalize P95 `+1.769628 ms`.
- 2026-02-28 (Phase 2 state-first): `cd parakeet-stt-daemon && uv run pytest tests/test_messages.py tests/test_overlay_event_stream.py tests/test_streaming_truth.py tests/test_session_cleanup.py tests/test_cli_precedence.py` passed (42 tests).
- 2026-02-28 (Phase 2 eval regression): `just eval compare` passed; stream-seal vs offline deltas were WER `-0.002778`, strict command exact match `+0.020000`, critical token recall `+0.006485`, and warm finalize P95 `+0.697694 ms`.
- 2026-02-28 (Phase 2 interim-text): `cd parakeet-stt-daemon && uv run pytest tests/test_messages.py tests/test_overlay_event_stream.py tests/test_streaming_truth.py tests/test_session_cleanup.py tests/test_cli_precedence.py` passed (44 tests).
- 2026-02-28 (Phase 2 interim-text eval regression): `just eval compare` passed; stream-seal vs offline deltas were weighted WER `-0.000765`, strict command exact match `+0.000000`, normalized command exact match `+0.000000`, intent+slot match `+0.000000`, critical token recall `+0.002594`, punctuation F1 `+0.016172`, terminal punctuation accuracy `+0.000000`, and warm finalize P95 `-1.903497 ms`.
- 2026-02-28 (Phase 3 routing/boundary): `cd parakeet-ptt && cargo test` passed (39 tests), including new overlay-routing boundary tests in `src/main.rs`.
- 2026-02-28 (Phase 4 slice A): `cd parakeet-ptt && cargo fmt` passed.
- 2026-02-28 (Phase 4 slice A): `cd parakeet-ptt && cargo test` passed (lib: 5 tests, `src/main.rs`: 40 tests, overlay binary unit target: 0 tests), including new `overlay_disconnect_does_not_block_final_result_injection`.
- 2026-02-28 (Phase 4 slice B): `cd parakeet-ptt && cargo fmt` passed.
- 2026-02-28 (Phase 4 slice B): `cd parakeet-ptt && cargo test` passed (lib: 5 tests, `src/main.rs`: 42 tests, overlay binary unit target: 0 tests), including `overlay_process::tests::manager_replays_latest_message_after_disconnect` and `overlay_process::tests::manager_reconnect_sends_only_current_state`.
- 2026-02-28 (Phase 4 slice C): `cd parakeet-ptt && cargo fmt` passed.
- 2026-02-28 (Phase 4 slice C): `cd parakeet-ptt && cargo test` passed (lib: 8 tests, `src/main.rs`: 42 tests, overlay binary unit target: 5 tests), including backend-selection noop safety and render-phase color mapping assertions.
- 2026-02-28 (Phase 4 slice D): `cd parakeet-ptt && cargo fmt` passed.
- 2026-02-28 (Phase 4 slice D): `cd parakeet-ptt && cargo test` passed (lib: 8 tests, `src/main.rs`: 43 tests, overlay binary unit target: 5 tests), including `overlay_crash_restart_replays_current_state_and_preserves_final_injection`.
- 2026-02-28 (Phase 4 slice E): `cd parakeet-ptt && cargo fmt` passed.
- 2026-02-28 (Phase 4 slice E): `cd parakeet-ptt && cargo test` passed (lib: 8 tests, overlay binary unit target: 9 tests, `src/main.rs`: 45 tests), including font parse/layout/render mapping assertions and existing overlay crash/restart boundary proofs.
- 2026-02-28 (Phase 4 slice E hotfix): `cd parakeet-ptt && cargo fmt && cargo test && cargo clippy --all-targets -- -D warnings` passed (overlay binary unit target now 10 tests with generic-family parsing coverage).
- 2026-02-28 (Phase 4 slice E hotfix): scripted `parakeet-overlay --backend layer-shell --auto-hide-ms 400` NDJSON two-session replay validated no post-hide backend flush failure and confirmed runtime font fallback no longer disables text rendering (`using_requested_generic_family:sans-serif`).

## Objective
Implement a modern Rust overlay that displays session feedback (and interim text when available) during push-to-talk, while preserving the hard safety guarantee that only `final_result` triggers text injection.

## Non-Negotiable Constraints
- Final text injection remains triggered only by `final_result`.
- Overlay is display-only and never owns paste/injection responsibilities.
- Overlay failures must not interrupt capture, transcription, or final injection.
- Protocol changes must remain backward-compatible during rollout.

## Canonical Local Truth (As Of This Revision)
- Daemon server message schema includes `interim_state`, `interim_text`, and `session_ended` in addition to baseline variants.
- Daemon now emits state-first overlay events (`interim_state`, `interim_text`, `session_ended`) only when `PARAKEET_OVERLAY_EVENTS_ENABLED=true`; default runtime behavior remains unchanged.
- `interim_text` is optional and emitted only when incremental runtime chunk transcription yields validated, non-empty text deltas.
- Daemon Pydantic message models currently use strict `extra="forbid"` behavior.
- Rust client injection boundary is hardened with routing proofs: only `ServerMessage::FinalResult` enqueues injection work.
- Rust client routes overlay-only variants (`interim_state`, `interim_text`, `session_ended`) through a dedicated sink adapter with per-session sequence filtering.
- Rust protocol decode now tolerates unknown server `type` values and ignores them safely.

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
- [x] Add a startup capability probe for overlay mode selection:
  - `layer_shell` when `zwlr_layer_shell_v1` is available.
  - `fallback_window` when layer-shell is unavailable but regular window rendering works.
  - `disabled` when neither mode initializes.
- [x] Emit explicit startup log describing effective overlay mode and reason.
- [x] Ensure overlay probe failures are soft-fail and do not affect existing PTT flow.

### Verification Loop
1. [x] Validate deterministic mode selection on Pop!_OS/COSMIC via protocol-signal classifier tests.
2. [x] Validate deterministic fallback behavior on unsupported compositor sessions via classifier tests.
3. [x] Validate that failed probe does not alter start/stop/final injection behavior (`cargo test`, `cargo clippy`, `prek` pre-push suite).

### Gate To Proceed
- [x] Overlay mode classification is deterministic and non-fatal under all tested sessions.

## Phase 1: Protocol Extension And Compatibility Hardening
### Implementation Tasks
- [x] Add new server message variants for overlay-safe realtime updates (PTT protocol model):
  - `interim_state`
  - `interim_text`
  - `session_ended`
- [x] Include `session_id` on all new variants.
- [x] Add monotonic per-session `seq` on interim variants.
- Preserve existing variants unchanged:
  - `session_started`
  - `final_result`
  - `error`
  - `status`
- Update both sides:
  - [x] `parakeet-stt-daemon/src/parakeet_stt_daemon/messages.py`
  - [x] `parakeet-ptt/src/protocol.rs`
- [x] Add explicit unknown-message tolerance path in `parakeet-ptt` websocket decode flow.

### Verification Loop
1. [x] Add schema and decode changes before enabling emission (PTT side complete).
2. [x] Run focused serialization/deserialization tests (PTT protocol tests).
3. [x] Verify unknown message types are ignored safely (PTT decode tests + websocket integration path).
4. [x] Verify old client against new daemon behavior (no crashes, bounded logs) via schema-only daemon change surface and baseline runtime checks.
5. [x] Verify new client against old daemon behavior.

### Essential Tests
- Daemon message model tests:
  - [x] `interim_text` required fields and validation.
  - [x] `interim_state` enum/state validation.
  - [x] non-negative `seq` validation (per-session monotonic enforcement remains a Phase 2+ runtime invariant).
- Rust protocol tests:
  - [x] decode new variants.
  - [x] round-trip all known variants.
  - [x] unknown `type` handling path remains non-fatal.

### Gate To Proceed
- [x] Mixed-version protocol matrix passes with no functional regressions.

## Phase 2: Daemon Emission Path (State-First, Then Text)
### Implementation Tasks
- [x] Introduce emission behind feature flag/environment control, default off (`PARAKEET_OVERLAY_EVENTS_ENABLED`).
- [x] Emit `interim_state` transitions first (`listening`, `processing`, `finalizing`).
- [x] Emit `interim_text` only when validated incremental source is available.
- [x] Emit `session_ended` on normal completion and abort/error paths.
- [x] Keep `final_result` generation path unchanged.
- [x] Add structured counters for emitted and dropped overlay events.

### Verification Loop
1. [x] Validate state-only emission path.
2. [x] Validate optional interim-text path.
3. [x] Validate no cross-session event leakage.
4. [x] Validate stop/abort race behavior.
5. [x] Validate exactly one `final_result` per session.

### Essential Tests
- Extend `parakeet-stt-daemon/tests/test_streaming_truth.py`:
  - in-order interim event behavior.
  - state-only fallback behavior.
- Add `parakeet-stt-daemon/tests/test_overlay_event_stream.py`:
  - no events after `session_ended`.
  - no cross-session leakage.
  - final_result exactly once.
  - interim text emitted only for validated incremental updates.
  - incremental text source failures remain non-fatal.
- Extend `parakeet-stt-daemon/tests/test_session_cleanup.py`:
  - abort paths emit terminal event and clean state.

### Gate To Proceed
- [x] Emission path passes all invariant tests without changing final-result behavior.

## Phase 3: PTT Routing + Injection Hard Boundary
### Implementation Tasks
- [x] Extend `handle_server_message` in `parakeet-ptt/src/main.rs`:
  - route `interim_state`, `interim_text`, and `session_ended` to overlay sink only.
  - keep enqueue logic only in `FinalResult` arm.
- [x] Add explicit routing counters/metrics proving interim messages never reach injector queue.
- [x] Add overlay sink adapter interface for deterministic tests.
- [x] Add stale/out-of-order `seq` dropping logic in overlay path.

### Verification Loop
1. Add exhaustive match coverage.
2. [x] Run existing queue/injector tests for regressions.
3. [x] Add adversarial mixed-stream tests.
4. [x] Validate queue counters change only on final results.
5. [x] Validate state reset semantics remain unchanged.

### Essential Tests
- Extend `parakeet-ptt/src/main.rs` tests:
  - [x] interim/state/ended events do not enqueue injection jobs.
  - [x] final result still enqueues exactly once.
  - [x] mixed stream enqueues exactly once.
  - [x] out-of-order interim seq is dropped on overlay path only.

### Gate To Proceed
- [x] Injection boundary remains mathematically unchanged: enqueue count only tracks `final_result`.

## Phase 4: Overlay Process MVP (Separate Process)
### Implementation Tasks
- [x] Add overlay binary target under `parakeet-ptt` workspace.
- [x] Implement deterministic overlay state machine first:
  - [x] `hidden`
  - [x] `listening`
  - [x] `interim`
  - [x] `finalizing`
- [x] Event intake from PTT via local IPC (newline-delimited JSON over child process stdio).
- [x] Backend selection + render integration:
  - [x] primary: layer-shell Wayland surface path with shared-memory buffer commits.
  - [x] fallback: regular best-effort `xdg_toplevel` path with shared-memory buffer commits.
  - [x] unsupported/probe-failure handling degrades to explicit noop backend.
- [x] Overlay process resilience and reconnect semantics:
  - [x] auto-respawn manager for child disconnects.
  - [x] replay only current valid overlay state after reconnect.
- [x] Behavior requirements:
  - [x] hidden when idle.
  - [x] auto-hide after final/session end timeout.
  - [x] no injection, no clipboard, no keyboard ownership.
- [x] Add configuration fields (opacity/font/anchor/margins/max width/lines) with conservative defaults in overlay CLI process.
- [ ] Remaining for full Phase 4 completion:
  - [ ] crash/restart simulation with reconnect semantics validated end-to-end.
  - [ ] CPU/memory budget validation under sustained dictation run.
  - [ ] richer text shaping/typography beyond phase-colored state surfaces.

### Verification Loop
1. [x] Validate state machine headlessly.
2. [x] Validate transitions from fake event streams.
3. [x] Validate rendering integration for key states (render-intent mapping, backend selection safety, and state-phase color routing unit coverage).
4. [x] Run overlay crash/restart simulation.
5. [ ] Validate CPU/memory budget under 10+ minute dictation run.

### Essential Tests
- Overlay state-machine tests:
  - [x] transition invariants.
  - [x] stale sequence drop behavior.
  - [x] auto-hide timer behavior.
- PTT integration tests:
  - [x] overlay disconnect has zero impact on final injection path.
  - [x] overlay reconnect consumes current valid state only.

### Gate To Proceed
- [ ] Overlay process may fail arbitrarily without affecting capture/transcription/final injection.

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
