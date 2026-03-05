# Streaming Overlay + Seal-Final Injection: Canonical Plan

_Last updated: 2026-03-03 (live UX findings: audio-reactive waveform, interim rewrite polish, injection-aware finalizing dismiss)_

## Progress Tracker
- [x] Worktree policy in effect (`../parakeet-overlay-dev` on `feature/overlay-phase0-capability-gate`).
- [x] Phase 0: Capability and feasibility gate implemented in `parakeet-ptt`.
- [x] Phase 1: Protocol extension + unknown-message compatibility hardening.
- [x] Phase 2: Daemon emission path behind rollout controls.
- [x] Phase 3: PTT routing + hard injection boundary proofs.
- [x] Phase 4: Overlay process MVP (separate process).
- [x] Phase 4 Slice A: separate overlay binary + deterministic state machine + IPC wiring + failure-isolation tests.
- [x] Phase 4 Slice B: overlay respawn manager + reconnect state replay guarantees.
- [x] Phase 4 Slice C: layer-shell/fallback render integration path + unsupported-backend noop safety.
- [x] Phase 4 Slice D: crash/restart simulation through PTT routing path + reconnect/final-injection proof.
- [x] Phase 4 Slice E: layer-shell text rendering + COSMIC fallback guardrails.
- [x] Phase 5: Config/flags/rollout controls.
- [x] Phase 6: E2E reliability and promotion gates.
- [x] Phase 7: Visual overhaul — dark frosted-glass panel, rounded corners, soft shadow, accent stripe, text shadows, premultiplied alpha, font cascade, 250ms ease-out-cubic fade transitions.
- [x] Phase 8 (P0–P2): Overlay UX polish — bottom-screen default, entrance/exit slide animations, accent cross-fade, animated listening text, finalizing progress bar + success flash.
- [x] Phase 8 (P3 follow-through): active-output tracking (8.4 Tier 1), interim text fade-in (8.6), idle breathing (8.7), adaptive width (8.8).
- [x] Phase 8 (P4 follow-through): audio-level plumbing, real-time waveform rendering, and injection-aware Finalizing dismissal with text carry-through.
- [ ] Phase 8 (remaining deferred): cursor-spawn placement (8.4 Tier 2). See **§ Overlay UX Roadmap**.

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
- 2026-02-28: Added COSMIC fallback guardrail warning when `fallback-window` is selected and updated the overlay runbook to prefer the layer-shell path with fallback marked diagnostic-only.
- 2026-02-28: Fixed Slice E COSMIC regressions by resolving generic/system font fallbacks for `Sans 16`, keeping layer-shell surfaces mapped with transparent hidden frames (instead of null-buffer detach), and failing fast on renderer errors so PTT respawn semantics recover overlay safely.
- 2026-02-28: Fixed daemon live interim gap by emitting deduplicated `interim_text` from streaming drain chunks during active sessions (not only stop-time `ready_chunks`), preserving display-only overlay routing and `final_result` injection boundaries.
- 2026-02-28: Fixed overlay long-utterance visibility by tail-clamping wrapped lines in `parakeet-overlay` so the newest interim words remain visible after `max_lines` is reached (instead of freezing on earliest lines).
- 2026-02-28: Completed Phase 4 closeout failure-isolation hardening with repeated overlay spawn/disconnect fault injection tests proving `final_result` enqueue/injection remains unaffected, including explicit missing-overlay-binary non-fatal coverage.
- 2026-02-28: Added repeatable `just soak-perf` sustained-run sampler for `parakeet-ptt` + `parakeet-overlay` CPU/RSS collection (10+ minute window, deterministic artifact path).
- 2026-02-28: Completed Phase 5 rollout controls by adding `--overlay-enabled <true|false>` with `PARAKEET_OVERLAY_ENABLED` precedence (`CLI > env > default=false`), invalid-env soft-fail disable semantics, and empty-mode-env normalization.
- 2026-02-28: Updated helper/runbook operator surfaces (`scripts/stt-helper.sh`, root `justfile`, `docs/stt-troubleshooting.md`) so overlay remains explicit opt-in while preserving non-fatal fallback behavior.
- 2026-02-28: Completed Phase 6 reliability harness by extending daemon overlay contract tests with explicit quick-utterance, long-dictation, daemon-reconnect, and overlay-crash scenarios while preserving non-fatal final-result behavior.
- 2026-02-28: Added explicit mixed-version decode stream test in `parakeet-ptt` protocol coverage to prove unknown daemon message types remain non-fatal when interleaved with known variants.
- 2026-02-28: Added Phase 6 operator gates (`just phase6-contract`, `just phase6-promotion`) to enforce repeated clean reliability runs and emit deterministic promotion artifacts.
- 2026-02-28: Simplified `just` operator surface by adding root-level overlay wrappers (`just start`, `just stop`, `just status`, `just phase6-contract`, `just phase6-promotion`) and collapsing redundant overlay start recipes into a single `start mode="<auto|layer-shell|fallback-window|disabled>"` (default `layer-shell`).
- 2026-02-28: Completed Phase 7 visual overhaul in `parakeet-overlay.rs` — replaced flat phase-colored rectangle with dark frosted-glass panel (rounded corners, soft shadow, accent stripe, text shadows, premultiplied alpha compositing, preferred font cascade, 250ms ease-out-cubic fade transitions). All rendering remains pure software pixel manipulation, no new dependencies.
- 2026-02-28: Documented Phase 8 overlay UX roadmap covering entrance/exit slide animations, animated ellipsis with rotating flavor text, bottom-screen default, cursor-aware multi-monitor placement (3-tier progressive enhancement), finalizing progress hints, interim text character-level fade-in, idle breathing, and adaptive width.
- 2026-03-01: Phase 8.3 — changed default anchor from `TopCenter` to `BottomCenter` with 32px vertical margin (was 24px). Users look at center-to-bottom of screen; top anchor forced eye-jump.
- 2026-03-01: Phase 8.1 (spatial) — added entrance slide (7px over 300ms ease-out-cubic) and exit slide (5px over 250ms ease-in-cubic) with anchor-aware direction. Shadow region (8px) absorbs the max 7px offset.
- 2026-03-01: Phase 8.1 (color) — added 150ms linear RGB cross-fade on accent stripe during visible phase transitions (Listening↔Interim↔Finalizing). Hidden→visible transitions handled by entrance fade.
- 2026-03-01: Phase 8.2 — added 12 rotating listening phrases (3s rotation cycle, 200ms cross-fade) and staggered ellipsis dot animation (1200ms cycle, 200ms per-dot delay). Starting phrase seeded from wall-clock time.
- 2026-03-01: Phase 8.5 — added 2px indeterminate progress bar during Finalizing (1500ms sweep, 30% segment width with soft edges) and 200ms green success flash on Finalizing→Hidden transition.
- 2026-03-01: Phase 8.4 Tier 1 — added active-output tracking from Wayland focus metadata through overlay spawn (`--output-name`) so layer-shell targets the focused monitor.
- 2026-03-01: Phase 8.6/8.7/8.8 — landed interim character fade-in, subtle listening-phase accent breathing, and adaptive panel width animation for short/long utterances.
- 2026-03-01: Startup race finding — `OverlayProcessManager` could spawn before focus cache output readiness and lock monitor targeting for process lifetime; decision was to defer output-targeted spawn until output hint/readiness is known and replay latest overlay state after hint-triggered spawn.
- 2026-03-01: Startup targeting hardening follow-up — added a one-shot output-name watchdog fallback in `OverlayProcessManager`: if focused output remains unavailable past a short timeout, spawn once without `--output-name` and log a warning to avoid permanent overlay invisibility.
- 2026-03-01: Visual artifact finding (deferred) — after Finalizing→Hidden on adaptive-width utterances, residual frame slices can remain on-screen (ghosted previous widths). Logged from live run screenshot; renderer cleanup fix is deferred.
- 2026-03-02: Ghosted slices fix — tracking previous committed width and damaging the width union on shrink paths ensures stale pixels are cleared. Added shrink-damage unit tests to overlay binary.
- 2026-03-03: Added end-to-end audio level event plumbing (`AudioLevel`) across protocol routing and overlay process boundaries, then consumed it in the renderer to drive a real-time waveform/VU treatment.
- 2026-03-03: Polished interim rewrite motion by stabilizing staged suffix animations and full-string replacement transitions to avoid visible jitter during rapid partial-result updates.
- 2026-03-03: Finalizing now carries forward last recognized interim text, accepts `injection_complete` as a one-shot dismiss signal, and falls back to a shorter 600ms auto-hide timeout when completion signals are absent.

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
- 2026-02-28 (Phase 2/4 live interim fix): `cd parakeet-stt-daemon && uv run pytest tests/test_session_cleanup.py tests/test_streaming_truth.py tests/test_overlay_event_stream.py tests/test_messages.py` passed (40 tests), including new `test_live_interim_chunk_emission_dedupes_repeated_text`.
- 2026-02-28 (Phase 4 slice E long-utterance tail clamp): `cd parakeet-ptt && cargo fmt && cargo test` passed (lib: 8 tests, overlay binary unit target: 11 tests, `src/main.rs`: 45 tests), including new `text_layout_keeps_recent_tail_lines_when_clamped` and `text_layout_truncates_single_long_word_to_width`.
- 2026-02-28 (Phase 4 closeout + Phase 5 controls): `cd parakeet-ptt && cargo fmt && cargo test && cargo clippy --all-targets -- -D warnings` passed (lib: 8 tests, overlay binary unit target: 11 tests, `src/main.rs`: 56 tests), including new repeated overlay-failure isolation and explicit missing-binary non-fatal tests.
- 2026-02-28 (helper contract): `bash -n scripts/stt-helper.sh` and `source scripts/stt-helper.sh && stt help start` passed with `--overlay-enabled` surfaced from `start_option_rows`.
- 2026-02-28 (Phase 4 sustained runtime soak): `just soak-perf 600 1` generated artifact `/tmp/parakeet-overlay-soak-20260228-221536.tsv` with 591 samples each for `parakeet-ptt` and `parakeet-overlay`; summarized CPU/RSS were `parakeet-ptt` avg/p95/max CPU `0.12/0.20/0.60%`, RSS `14.8/14.8/14.8 MiB`; `parakeet-overlay` avg/p95/max CPU `0.00/0.00/0.10%`, RSS `31.9/31.9/31.9 MiB`.
- 2026-02-28 (Phase 6 contract coverage): `cd parakeet-stt-daemon && uv run pytest tests/test_overlay_event_stream.py` passed (12 tests), including explicit quick-utterance, long-dictation, daemon-reconnect, and overlay-crash scenario contracts.
- 2026-02-28 (Phase 6 mixed-version + isolation checks): `cd parakeet-ptt && cargo fmt && cargo test && cargo clippy --all-targets -- -D warnings` passed (lib: 8 tests, overlay binary unit target: 11 tests, `src/main.rs`: 57 tests), including new `decode_server_message_mixed_version_stream_tolerates_unknown_between_known_messages`.
- 2026-02-28 (Phase 6 promotion gate): `just phase6-promotion 3` passed (three consecutive clean `phase6-contract` runs + stream/seal regression gate) and generated artifact `/tmp/parakeet-overlay-phase6-gate-20260228-224327.log`; stream-seal vs offline deltas were weighted WER `+0.000514`, strict command exact match `+0.020000`, normalized command exact match `+0.020000`, intent+slot match `+0.020000`, critical token recall `+0.007782`, punctuation F1 `+0.018368`, terminal punctuation accuracy `+0.000000`, and warm finalize P95 `+2.828343 ms`.

- 2026-03-01 (Phase 8 commits 1–5): `cd parakeet-ptt && cargo fmt && cargo test && cargo clippy --all-targets -- -D warnings` passed (overlay binary unit target: 32 tests), including `default_cli_anchor_is_bottom_center`, `ease_in_cubic_boundaries`, `slide_offset_entrance_ends_at_zero`, `slide_offset_exit_starts_at_zero`, `entrance_duration_longer_than_exit`, `accent_transition_interpolates_at_midpoint`, `accent_transition_completes_at_duration`, `no_accent_transition_from_hidden`, `phrase_advances_after_interval`, `dot_opacities_stagger_correctly`, `dots_reset_after_cycle`, `crossfade_active_during_rotation_window`, `progress_segment_wraps_at_duration`, `success_flash_active_during_window`, `success_flash_triggers_on_finalizing_exit`.
- 2026-03-01 (Phase 8 follow-through + output-targeting race fix): `cd parakeet-ptt && cargo fmt && cargo test` passed (lib: 9 tests, overlay binary unit target: 47 tests, `src/main.rs`: 59 tests), including `output_targeted_manager_waits_for_hint_and_replays_latest_state`, `focus_snapshot_includes_output_name`, `char_fadein_zero_at_start`, `breathing_modulates_alpha_at_quarter_cycle`, and adaptive-width bounds tests.
- 2026-03-01 (post-review lint gate): `cd parakeet-ptt && cargo clippy --all-targets -- -D warnings` passed after startup race-fix follow-up cleanup.
- 2026-03-01 (output watchdog fallback): `cd parakeet-ptt && cargo fmt && cargo test && cargo clippy --all-targets -- -D warnings` passed (`src/main.rs`: 61 tests), including new `overlay_process::tests::output_watchdog_spawns_once_without_output_targeting`.
- 2026-03-02 (ghosted slices fix): `cd parakeet-ptt && cargo fmt && cargo test && cargo clippy --all-targets -- -D warnings` passed (overlay binary unit target: 51 tests), including new shrink-damage width tests proving stale pixels are cleared on adaptive width shrink. Fix tracks previous committed width and damages the width union on shrink paths.
- 2026-03-03 (Phase 8 audio/waveform + injection-aware finalizing): `cd parakeet-ptt && cargo test -p parakeet-ptt && prek run --all-files && prek run --stage pre-push --all-files` passed, including new coverage for `overlay_ipc::tests::injection_complete_serialization_roundtrip`, `overlay_state::tests::injection_complete_hides_matching_finalizing_session`, and `overlay_process::tests::manager_replay_ignores_injection_complete_as_latest_state`.

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
- [x] Remaining for full Phase 4 completion:
  - [x] crash/restart simulation with reconnect semantics validated end-to-end.
  - [x] CPU/memory budget validation under sustained 10-minute runtime soak.
  - [x] richer text shaping/typography beyond phase-colored state surfaces (completed in Phase 7 visual overhaul).

### Verification Loop
1. [x] Validate state machine headlessly.
2. [x] Validate transitions from fake event streams.
3. [x] Validate rendering integration for key states (render-intent mapping, backend selection safety, and state-phase color routing unit coverage).
4. [x] Run overlay crash/restart simulation.
5. [x] Validate CPU/memory budget under 10+ minute dictation run.

### Essential Tests
- Overlay state-machine tests:
  - [x] transition invariants.
  - [x] stale sequence drop behavior.
  - [x] auto-hide timer behavior.
- PTT integration tests:
  - [x] overlay disconnect has zero impact on final injection path.
  - [x] overlay reconnect consumes current valid state only.

### Gate To Proceed
- [x] Overlay process may fail arbitrarily without affecting capture/transcription/final injection.

## Phase 5: Config, Feature Flags, and Rollout Controls
### Implementation Tasks
- [x] Add `PARAKEET_OVERLAY_ENABLED` and CLI equivalent.
- [x] Add mode control (`auto`, `layer-shell`, `fallback`, `off`) if needed.
- [x] Keep overlay disabled by default initially.
- [x] Ensure startup logs include effective overlay mode and fallback reason.

### Verification Loop
1. [x] Verify defaults preserve baseline behavior bit-for-bit.
2. [x] Validate env/CLI precedence.
3. [x] Validate invalid config handling.
4. [x] Validate soft-fail when overlay binary missing.

### Essential Tests
- Rust config precedence tests in `parakeet-ptt/src/config.rs`.
- Startup mode tests:
  - [x] enabled+available.
  - [x] enabled+missing binary.
  - [x] disabled.

### Gate To Proceed
- [x] Overlay remains opt-in and cannot break baseline runtime when disabled.

## Phase 6: End-To-End Reliability And Promotion Gates
### Implementation Tasks
- [x] Add E2E runner scenarios:
  - [x] quick utterance.
  - [x] long dictation.
  - [x] abort mid-session.
  - [x] daemon reconnect.
  - [x] overlay crash mid-session.
  - [x] mixed-version protocol compatibility.
- [x] Add acceptance thresholds for latency and final injection reliability.

### Verification Loop
1. [x] Run E2E on every protocol-affecting PR (`just phase6-contract`).
2. [x] Capture artifacts/logs for failures (`phase6-promotion` writes deterministic `/tmp/parakeet-overlay-phase6-gate-*.log`).
3. [x] Require repeated clean runs before promotion (`phase6-promotion runs>=3`).
4. [x] Re-run stream+seal quality checks to ensure no WER/latency regression (`just eval compare` within promotion gate).

### Essential Tests
- Extended daemon overlay contract suite:
  - `parakeet-stt-daemon/tests/test_overlay_event_stream.py` (Phase 6 scenario tests).
- Extended PTT mixed-version protocol suite:
  - `parakeet-ptt/src/protocol.rs` (`decode_server_message_mixed_version_stream_tolerates_unknown_between_known_messages`).
- Added promotion-gate harness:
  - Root `justfile` (`phase6-contract`, `phase6-promotion`, updated `runbook`).

### Gate To Proceed
- [x] Promotion requires repeated clean runs and zero regressions to final-result correctness.

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

## Phase 7: Visual Overhaul (Completed)

Transformed the overlay from a flat phase-colored rectangle into a polished dark-glass panel. All rendering remains pure software pixel manipulation — no new dependencies.

### What Shipped
- **Design system**: dark near-black `(22,22,26)` background at ~90% opacity, subtle `(58,58,68)` 1px border, 12px corner radius, 8px soft shadow with quadratic falloff.
- **Accent stripe**: 3px pill-shaped vertical indicator on the left interior edge, colored per phase (blue=listening, mint=interim, amber=finalizing). Replaces full-background color coding.
- **Typography**: preferred font cascade `Inter → Cantarell → Noto Sans → generic Sans` (only when user hasn't overridden `--font`); default 18px with 1.45 line height; 1px text shadow for legibility.
- **Premultiplied alpha**: `argb_pixel_premul()` and premultiplied-over-premultiplied compositing for correct Wayland ARGB8888 rendering (eliminates fringing on shadows and AA edges).
- **Geometry primitives**: `Rect`, `ContentArea`, `blend_pixel`, `rounded_rect_coverage` (AA at corners), `fill_rounded_rect`, `stroke_rounded_rect`, `distance_to_rounded_rect`.
- **Shadow**: surface dimensions extended by `2 * shadow_radius`; layer-shell margins compensated so content visual position is unchanged.
- **Fade transitions**: 250ms ease-out-cubic for visibility transitions. `FadeState` tracks direction/progress; tick handler re-renders while fading.
- **Tests**: 16 unit tests including `fill_rounded_rect_corner_coverage`, `fade_progress_interpolation`, `ease_out_cubic_boundaries`, `surface_dimensions_include_shadow`.

### Files Modified
- `parakeet-ptt/src/bin/parakeet-overlay.rs` — all rendering code.

---

## Phase 8: Overlay UX Polish (P0–P3 Complete, Tier 2 Deferred)

### What Shipped
- **8.3 — BottomCenter default**: anchor changed from `TopCenter` to `BottomCenter`, vertical margin bumped from 24→32px to clear taskbar/dock areas.
- **8.1 — Entrance/exit slide**: 7px slide-in over 300ms (ease-out-cubic), 5px slide-out over 250ms (ease-in-cubic). Direction is anchor-aware (bottom anchors slide down, top anchors slide up). Shadow region (8px) absorbs the max 7px offset.
- **8.1 — Accent cross-fade**: 150ms linear RGB interpolation on accent stripe when transitioning between visible phases. Entrance fade handles Hidden→visible.
- **8.2 — Animated listening text**: 12 rotating flavor phrases on a 3s cycle with 200ms cross-fade. Staggered 3-dot ellipsis animation (1200ms cycle, 200ms per-dot delay). Starting phrase seeded from wall-clock time.
- **8.5 — Progress bar + success flash**: 2px indeterminate sweep bar during Finalizing (1500ms cycle, 30% segment, soft edges). 200ms green success flash on Finalizing→Hidden exit overrides accent stripe.
- **8.4 Tier 1 — Active-output tracking**: wired focused-output names through the Wayland focus cache and process spawn path so layer-shell overlay startup targets the active monitor.
- **8.6 — Interim character fade-in**: shared-prefix diffing + 100ms alpha ramp for newly appended interim text to reduce full-string flicker.
- **8.7 — Listening breathing**: subtle ±5% accent alpha oscillation during Listening only (paused during accent transitions and non-listening phases).
- **8.8 — Adaptive width**: measured-text width targets with ~200ms easing, min/max clamps, and interim growth bias to avoid jarring panel shrink while streaming.
- **Startup targeting hardening**: fixed output-targeting startup race by deferring output-targeted spawn until output hint/readiness, replaying latest non-hint state after hint-triggered spawn, and adding a one-shot watchdog fallback spawn (without `--output-name`) to avoid permanent invisibility when focus output never resolves.
- **Tests**: overlay binary now at 47 unit tests; `src/main.rs` now at 61 tests, including output-targeted spawn deferral/replay coverage and watchdog fallback coverage.
- **Files modified**: renderer + routing/focus/process plumbing (`parakeet-ptt/src/bin/parakeet-overlay.rs`, `src/main.rs`, `src/overlay_process.rs`, `src/surface_focus.rs`, `src/overlay_ipc.rs`, `src/overlay_state.rs`).

### Findings and Decisions (2026-03-01)
- **Finding**: eager overlay spawn could run before focus cache output readiness, leading to untargeted startup and monitor lock-in for the child lifetime.
- **Decision**: require output readiness for targeted startup, emit output hints before interim events, and replay latest overlay state after hint-driven spawn.
- **Decision (hardening)**: if output targeting remains unresolved past watchdog timeout, allow exactly one fallback spawn without `--output-name` and emit a warning, prioritizing visibility over perfect monitor targeting.
- **Known issue (FIXED 2026-03-02)**: Finalizing→Hidden can leave ghosted width slices from prior adaptive-width frames. Root cause was incomplete damage-region cleanup on shrink — fixed by tracking previous committed width and damaging the width union on shrink paths. See commit `51cf52b`.
- **Deferred**: cursor-spawn placement (8.4 Tier 2) remains intentionally deferred until Tier 1 stability is observed under longer multi-monitor soak runs.

### Findings and Decisions (2026-03-02)
- **Finding (monitor targeting)**: live observation shows overlay still appears on wrong monitor most of the time despite Tier 1 output tracking implementation. The focus cache may be returning stale/unavailable output names, or the timing between focus cache readiness and overlay spawn is still misaligned. Root cause investigation needed — not yet clear if this is code behavior or intended design.
- **Finding (listening text cycling)**: the 12 rotating listening phrases (8.2) are not cycling as implemented. The overlay shows only a single static phrase instead of cycling through the phrases every 3 seconds. This appears to be a renderer or state tick bug rather than a state machine issue. **Key observation**: this bug only occurs on the first time the overlay renders after PTT daemon start. After completing a transcription cycle (overlay appears, text flows, gets hidden), subsequent overlay spawns work correctly and phrases cycle as expected. This suggests an initialization issue in the first render cycle.
- **Finding (finalizing UX)**: the "Finalizing..." text at session end shows static text instead of an animation. The current implementation displays stock "Finalizing..." text during the finalizing phase, but the original design intent (8.5) was for an indeterminate progress bar + success flash. The progress bar exists but is too thin (2px) and subtle to be noticeable — only a faint hint at the bottom edge. Consider thickening the progress bar and/or replacing the static final text with a brief animation (e.g., pulsing or shrinking) to improve UX at the critical final-to-hidden transition.

### Design Direction
The overlay should feel like it belongs on a premium desktop — invisible when idle, delightful when active, and always spatially intuitive. Every interaction should feel like the system is alive and responsive, not just flipping visibility flags.

---

## Overlay UX Roadmap

### 8.1 — Entrance & Exit Micro-Animations

**Problem**: The current 250ms opacity fade is functional but flat. Modern system UIs (macOS notifications, GNOME toast, COSMIC panel hints) combine opacity with spatial motion to create a sense of physical presence.

**Design**:
- **Entrance**: slide-up 6–8px + opacity fade-in over 300ms with ease-out-cubic. The panel should feel like it's gently rising into view from just below its resting position.
- **Exit**: slide-down 4–6px + opacity fade-out over 250ms with ease-in-cubic. Slightly faster than entrance — departures should feel snappy, not lingering.
- **Implementation**: animate a `y_offset` alongside `fade_alpha` in `FadeState`. Apply the offset to `content.y` before rendering. Layer-shell margins or surface position don't need to change — the offset is purely within the allocated surface buffer.
- **Phase transitions** (listening → interim → finalizing): subtle accent stripe color cross-fade (interpolate RGB over ~150ms) instead of hard cut. No spatial motion for phase changes — only entrance/exit get motion.

### 8.2 — Dynamic Listening Text (Personality)

**Problem**: "Listening..." is static and generic. The overlay sits there unchanged for the entire listening phase, which can last several seconds. It feels dead.

**Design** — two layers of life:

1. **Animated ellipsis**: cycle dots with staggered opacity. Render three dots where each dot fades in sequentially (dot 1 at 0ms, dot 2 at 200ms, dot 3 at 400ms), then the whole set resets at 1200ms. This gives a gentle "breathing" pulse without being distracting. Implemented as a time-based alpha modulation on each dot glyph — no layout changes needed.

2. **Rotating flavor text**: replace static "Listening" with randomly selected warm phrases that rotate every ~3 seconds with a quick cross-fade. Examples (inspired by Claude Code's loading messages, but adapted for voice context):
   - "Listening closely..."
   - "All ears..."
   - "Go ahead, I'm here..."
   - "Ready when you are..."
   - "Hearing you out..."
   - "Speak your mind..."
   - "Catching every word..."
   - "Tuned in..."
   - "Say the word..."
   - "Standing by..."
   - "On it..."
   - "Ears perked..."

   **Implementation**: maintain a `&[&str]` pool in the overlay binary. On entering `Listening` phase, pick a random starting index (seeded from `Instant::now()` to avoid needing `rand`). Advance index every ~3s. Cross-fade between old and new text by rendering outgoing text at decreasing alpha and incoming text at increasing alpha over ~200ms. The `OverlayVisibility::Listening` render intent currently emits a fixed `"Listening..."` headline from `overlay_state.rs` — the flavor rotation should live **in the overlay renderer**, not the state machine, since it's purely cosmetic and shouldn't affect IPC or state transitions.

### 8.3 — Bottom-of-Screen Default Position

**Problem**: the current default anchor is `TopCenter`. Users are typically looking at the center-to-bottom of their screen (text editors, terminals, chat windows). A top-anchored overlay forces the eye to jump to the top of the display, breaking flow.

**Design**:
- Change `--anchor` default from `TopCenter` to `BottomCenter`.
- Adjust default `--margin-y` from 24 to 32 (slightly more breathing room from the bottom edge — taskbars, docks, and COSMIC panel live here).
- This is a one-line default change in the CLI parser. Existing users who pass `--anchor` explicitly are unaffected.

### 8.4 — Cursor-Spawn Placement (Focus Proximity)

**Problem**: on multi-monitor setups, the overlay always appears on the same output regardless of where the user is working. The user has to visually scan across screens to find the overlay.

**Design** — progressive enhancement in two tiers:

1. **Tier 1 — Active-output tracking** (low effort): use the Wayland `wl_output` that the focused surface is on. Layer-shell `get_layer_surface()` accepts an optional `output` parameter. When `parakeet-ptt` knows which output the focused window is on (already partially available from the toplevel cache), pass that output reference to the overlay via IPC. The overlay re-targets its layer surface to that output. This gets the overlay on the right monitor without any pointer tracking.

2. **Tier 2 — Cursor-spawn positioning** (medium effort, target tier): snapshot the pointer position at utterance start via `wl_pointer` and spawn the overlay anchored to the quadrant/region of the output where the cursor was at that moment. The overlay stays put for the duration of the utterance — no continuous tracking, no following, just "appear where I'm looking." Requires a one-shot `wl_pointer` position capture when entering Listening phase. The overlay process already has a Wayland connection.

**Recommended path**: implement Tier 1 first (active output), then Tier 2 (cursor-spawn). Continuous cursor-following (Tier 3 in earlier drafts) is overkill and risks being distracting — removed from the roadmap.

### 8.5 — Finalizing Phase Polish

**Problem**: the "Finalizing..." phase currently shows static text with an amber accent stripe. This is the moment the user is most anxious — they just stopped talking and want to know their text landed.

**Design**:
- **Progress hint**: show a subtle horizontal progress bar along the bottom edge of the content area (1–2px tall, accent-colored). Even if we don't have real progress data, a smooth indeterminate animation (a short bright segment sweeping left-to-right over ~1.5s) signals that work is happening.
- **Success flash**: on transition from `Finalizing` to `Hidden` (which means `final_result` was received and injection happened), flash the accent stripe green for ~200ms before starting the fade-out. This gives a satisfying "done" signal without text changes.

### 8.6 — Interim Text Streaming Feel (done)

**Problem**: interim text currently appears as full string replacements — the entire headline swaps on each `interim_text` event. This looks like flickering rather than streaming.

**Design**:
- **Character-level fade-in**: when new interim text arrives, diff against the previous text. Characters that are unchanged render at full alpha. New characters at the end fade in over ~100ms. This creates a typewriter-like streaming feel.
- **Implementation**: track `previous_headline: String` in the render path. On each frame, compare current vs previous. For characters beyond the shared prefix, apply a time-based alpha ramp. Reset the timer on each new `interim_text` event.

### 8.7 — Subtle Idle Breathing (Listening Phase)

**Problem**: during the listening phase, the overlay is static apart from the proposed dot animation. A completely still panel can feel frozen.

**Design**:
- Very subtle accent stripe brightness oscillation: ±5% alpha over a 3-second sine cycle. Just enough to suggest "alive" without being distracting.
- Only active during `Listening` phase. Pauses during `Interim` (text is the focus) and `Finalizing` (progress bar takes over).

### 8.8 — Adaptive Width (hidden behind flag - to be retired soon)

**Problem**: the overlay is always `max_width` wide regardless of text length. Short messages like "Tuned in..." sit in an unnecessarily wide panel.

**Design**:
- Measure actual text width after layout. Set surface width to `max(text_width + padding, min_width)` clamped to `max_width`.
- Animate width changes over ~200ms with ease-out to prevent jarring size jumps during interim text streaming.
- This requires recreating the `wl_shm_pool`/`wl_buffer` on size change, or pre-allocating at `max_width` and only damage/commit the active region. The latter is simpler and wastes minimal memory.

---

## 2026-03-03 QA Latency Matrix (Main vs Overlay Branch)

Full-corpus benchmark run completed across four comparison points (`main` vs `feature/overlay-phase0-capability-gate`, each in `offline` and `stream-seal`) using `check_model.py --bench-offline --bench-manifest bench_audio/personal --bench-append-legacy --bench-tier all`.

### Summary Metrics (108 samples, 2 runs, median-of-runs aggregate)

| Branch | Runtime | avg_wer | weighted_wer | infer_p95_ms | warm_finalize_p95_ms |
| --- | --- | ---: | ---: | ---: | ---: |
| `feature/overlay-phase0-capability-gate` (`d255423`) | `offline` | `0.031434` | `0.039909` | `56.443` | `56.796` |
| `feature/overlay-phase0-capability-gate` (`d255423`) | `stream-seal` | `0.030659` | `0.039239` | `58.163` | `57.165` |
| `main` (`48958ae`) | `offline` | `0.030552` | `0.039147` | `56.505` | `56.714` |
| `main` (`48958ae`) | `stream-seal` | `0.030835` | `0.039391` | `59.054` | `58.032` |

### Key Comparison Notes

- Overlay branch `stream-seal` remained within noise-level deltas vs `main` and showed slightly lower p95 latency than `main` `stream-seal` on this run (`warm_finalize_p95 -0.866 ms`, `infer_p95 -0.891 ms`).
- No regression gate failures occurred in any of the four runs.
- First-sample cold-start outlier (`cmd_001`) remained a warmup artifact and is excluded from warm metrics (`warmup_samples=1`).

### JSON Result Artifacts

- [Overlay branch / offline](file:///tmp/parakeet-qa-latency-20260303-184336/feature_overlay-phase0-capability-gate-d255423-offline.json)
- [Overlay branch / stream-seal](file:///tmp/parakeet-qa-latency-20260303-184336/feature_overlay-phase0-capability-gate-d255423-stream-seal.json)
- [Main branch / offline](file:///tmp/parakeet-qa-latency-20260303-184336/main-48958ae-offline.json)
- [Main branch / stream-seal](file:///tmp/parakeet-qa-latency-20260303-184336/main-48958ae-stream-seal.json)

### Helper Runtime Truth Snapshots

- [Overlay helper status (offline)](file:///tmp/parakeet-qa-latency-20260303-184336/feature_overlay-phase0-capability-gate-offline-helper-status.json)
- [Overlay helper status (streaming)](file:///tmp/parakeet-qa-latency-20260303-184336/feature_overlay-phase0-capability-gate-streaming-helper-status.json)
- [Main helper status (offline)](file:///tmp/parakeet-qa-latency-20260303-184336/main-offline-helper-status.json)
- [Main helper status (streaming)](file:///tmp/parakeet-qa-latency-20260303-184336/main-streaming-helper-status.json)
