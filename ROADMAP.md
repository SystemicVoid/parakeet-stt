# Parakeet STT Product Roadmap

Date: 2026-02-23

## Goal

Turn Parakeet STT from "works with logs" into a premium local dictation experience with clear, low-latency user feedback and predictable behavior across terminals and browsers.

## Current Baseline

- Core STT loop is operational and low-latency.
- Injection stack has robust controls (`paste` mode, key backend, failure policy).
- Daemon hardening gaps are now explicitly identified (disconnect cleanup invariants, CLI/env precedence, and streaming truthfulness).
- Main UX gap is user feedback: state is visible in logs, not in an obvious UI channel.

## UX Principles

- State must be obvious without opening logs.
- Feedback must be immediate (<100ms perceived response when possible).
- Failure states must be explicit and recoverable.
- Advanced tuning remains available, but defaults stay minimal and reliable.

## P-1: Daemon Hardening Gate (Release Blocker for Streaming UX)

Scope:
- Enforce session lifecycle invariants on websocket disconnect and handler errors.
- Make start-session transactional so partial failures always roll back to idle.
- Fix configuration precedence for booleans (`CLI explicit > ENV > defaults`).
- Make daemon truth visible (`requested` vs `effective` runtime state for device and streaming engine).
- Add daemon test coverage for lifecycle and config regressions.
- Keep helper/runtime defaults aligned with offline-first operation, while retaining an explicit streaming opt-in profile.
- Remove default offline finalize temp-wav roundtrip by transcribing in-memory arrays first, with a guarded temp-wav fallback path.

Implementation direction:
- Add one cleanup path used by disconnect, exception, stop, and abort handling.
- Add rollback guards around post-allocation start-session steps.
- Parse CLI booleans with explicit intent (no implicit override when flags are omitted).
- Enrich `/status` and startup logs with hard runtime truth signals and fallback reasons.
- Add `pytest` plus focused tests for session lifecycle, precedence, and protocol mapping.
- Keep helper default path offline (`stt start`), and use explicit streaming opt-in (`PARAKEET_STREAMING_ENABLED=true`) for targeted validation/soak runs.
- Use in-memory offline transcription as the default finalize path; keep temp-wav fallback only for compatibility failures.

Acceptance:
- Forced disconnect during active listening leaves `sessions_active=0` with no continued audio accumulation.
- Env-only control of streaming/status works when CLI flags are omitted.
- Startup and `/status` clearly report whether streaming is truly active or in offline fallback.
- Daemon lifecycle and precedence tests pass locally in CI-equivalent runs.
- Streaming validation profile can run repeated dictation cycles without leaked session state.

## P0: Injection Architecture Hardening (After P-1)

Scope:
- Keep the consolidated runtime path (`paste` + `copy-only`) reliable before adding new UX features.
- Remove hot-path blocking and hidden fallback ambiguity in injector execution.
- Add clearer per-stage timing and failure observability for routing, clipboard, and chord emission.

Implementation direction:
- Move blocking injection work off async message handling paths.
- Keep route semantics deterministic (primary + adaptive fallback) and fully logged.
- Introduce a dedicated injector worker/channel and richer metrics as the next refactor step.
- Investigate reducing subprocess churn (`wl-paste`/`wl-copy` probing) while preserving correctness.

Acceptance:
- No measurable event-loop stalls attributable to injection calls.
- Failures are attributable to a specific stage (`clipboard_ready`, `route_shortcut`, `backend`).
- Regression matrix remains stable across terminal + browser + editor targets.

Session closeout notes (2026-02-22):
- Cross-surface validation passed in active use (Ghostty, Brave navigation bar, COSMIC Text Editor).
- Recent runtime telemetry remained healthy during repeated dictation cycles:
  - `backend_failure_total=0`
  - `queue_wait_ms=0`
  - event-loop lag windows `p99=2ms` vs target `20ms`
- Known minor issue: startup device scan can emit `failed to open "/dev/input/eventX": Permission denied` while still finding enough readable devices and starting hotkey listeners.
  - Follow-up candidate (low priority): suppress/downgrade per-device permission-denied noise unless no eligible hotkey devices are discovered.

Known cross-surface gap (2026-02-23):
- Zed markdown editor can interpret current paste routing as a markdown preview toggle instead of text insertion.
- Working hypothesis: terminal-style shortcut routing (`Ctrl+Shift+V`) conflicts with Zed markdown keybinding semantics on that surface.
- Follow-up: add an app/surface override for Zed markdown contexts (`Ctrl+V` first, explicit fallback policy) and add Zed to the acceptance matrix for semantic insertion validation.

 STT: compatibility > cleverness

Keep fast paste path (`Ctrl+V` / `Ctrl+Shift+V`), but add reliability defaults:

- keep fast paste path
- add typing fallback as reliability default when paste fails
- instrument failure detection per app/surface (start with Zed + VSCode parity tests)

That turns weird app shortcuts from random breakage into deterministic behavior.

## Phase 1: Immediate Feedback Layer (Sound + Notification)

Scope:
- Add short, distinct cues for:
  - ready
  - listening start
  - listening stop
  - transcript injected
  - injection fallback/copy-only
  - hard error
- Add optional desktop notification summaries for error/fallback events.

Implementation direction:
- Rust-side event emission from `parakeet-ptt` state transitions.
- Configurable cue backend (`none|sound|notify|sound+notify`).
- Keep defaults lightweight and local-only.

Known limitation (completion sound):
Current completion sound plays after button release because offline mode only transcribes once
`stop_session` is received. The sound confirms completion but cannot signal "ready to release"
during long utterances. Fixing this requires streaming mode where partial results arrive while
holding, enabling a "settled" detection (no new words for N ms) to trigger a release-ready cue.

Acceptance:
- User can run one dictation cycle without looking at logs and correctly infer system state.
- No audible spam during normal repeated use.

## Phase 2: Compact TUI Overlay

Scope:
- Add a tiny terminal dashboard mode with:
  - connection status
  - hotkey state (`Idle`, `Listening`, `WaitingResult`)
  - last transcript preview (truncated)
  - injection backend used (`uinput|ydotool|copy-only`)
  - last outcome class (`success_assumed`, `copy_only`, etc.)

Implementation direction:
- New `parakeet-ptt --ui tui` mode (read-only status + key hints).
- Reuse existing tracing/event model; avoid daemon protocol churn in first pass.

Acceptance:
- Single-pane TUI works in tmux and standalone terminal.
- Status updates remain smooth under normal dictation cadence.

## Phase 3: Native Desktop Companion (Optional GUI)

Scope:
- Build a minimal Rust desktop companion:
  - tray icon with live state color
  - push-to-talk status badge
  - recent transcripts list
  - quick toggles for injection mode/backend

Implementation direction:
- Keep GUI optional and separate binary/crate.
- Start with read-only status + controls that map to existing client flags/env.

Acceptance:
- GUI is optional; CLI-only users are unaffected.
- Common operations (start/stop/status/mode switch) can be done from tray UI.

## Phase 4: Reliability UX Hardening

Scope:
- Add explicit "action outcome" surface:
  - "Injected"
  - "Copied only (paste backend unavailable)"
  - "Paste command sent, app may have rejected"
- Add lightweight per-app profile presets (terminal/browser/general).

Implementation direction:
- Keep profile logic local and deterministic.
- Start with opt-in presets that map to existing runtime behavior.

Acceptance:
- Users can tell whether failure is ASR, clipboard, backend init, or target-app acceptance.
- First-run experience requires minimal tuning for common apps.

## Phase 5: Dictation Error Correction Loop (Low Priority)

Scope:
- Add a lightweight, user-curated correction layer for recurring dictation mistakes.
- Keep corrections grounded in observed errors captured during daily use.
- Avoid speculative rewrites or broad automatic "cleanup" that can hallucinate intent.

Implementation direction:
- Start with a local corrections file (jargon/substitution style) and apply deterministic replacements.
- Keep the workflow append-only and incremental: add one correction when a pattern repeats.
- Prefer narrow phrase-level mappings over aggressive regex-style transforms.

Acceptance:
- Users can maintain recurring corrections over time without changing core ASR/injection behavior.
- Correction behavior is predictable, explainable, and easy to disable for troubleshooting.

## Delivery Strategy (Atomic)

1. Complete P-1 daemon hardening gate and lock lifecycle/config invariants with tests.
2. Complete P0 injection architecture hardening and lock reliability baselines.
3. Build and lock a repeatable offline benchmark harness (`bench_audio` WER + timing summaries + regression thresholds) as a hard gate before UX phase work. (done 2026-02-25)
4. Ship Phase 1 cues behind a feature flag, default on for sound cues only.
5. Add Phase 1 tests and update docs.
6. Ship TUI skeleton with existing state only.
7. Extend TUI with injection outcomes.
8. Add incremental dictation error correction loop behind an opt-in switch.
9. Prototype GUI after TUI reaches stable daily use.

## Daemon Hardening Action Board (2026-02-23)

Status legend: `todo` | `in-progress` | `done` | `blocked`

1. `A1` — status: `done` (2026-02-23); owner: `Owner-S1`; branch: `agent/a1-a3-session-hardening`; scope: Disconnect/error cleanup invariant in daemon session lifecycle (`parakeet-stt-daemon/src/parakeet_stt_daemon/server.py`, `parakeet-stt-daemon/src/parakeet_stt_daemon/audio.py`, `parakeet-stt-daemon/tests/test_session_cleanup.py`).
2. `A2` — status: `done` (2026-02-23); owner: `Owner-M1`; branch: `agent/a2-config-precedence`; scope: CLI/env precedence fix for `status_enabled` and `streaming_enabled` (`parakeet-stt-daemon/src/parakeet_stt_daemon/__main__.py`).
3. `A3` — status: `done` (2026-02-23); owner: `Owner-S1`; branch: `agent/a1-a3-session-hardening`; scope: Transactional start-session rollback semantics (`parakeet-stt-daemon/src/parakeet_stt_daemon/server.py`, `parakeet-stt-daemon/src/parakeet_stt_daemon/session.py`) plus disconnect-path session ownership guard.
4. `A4` — status: `in-progress`; owner: `Owner-M1`; branch: `agent/b2-c1-observability`; scope: Runtime truth signals in `/status` and startup logging (`parakeet-stt-daemon/src/parakeet_stt_daemon/server.py`, `parakeet-stt-daemon/src/parakeet_stt_daemon/model.py`, `parakeet-stt-daemon/src/parakeet_stt_daemon/messages.py`).
5. `A5` — status: `done` (2026-02-23, bootstrap); owner: `Owner-M1`; branch: `agent/a2-config-precedence`; scope: Daemon test harness bootstrap + focused config precedence tests (`parakeet-stt-daemon/tests/`, `parakeet-stt-daemon/pyproject.toml`).
6. `A6` — status: `done` (2026-02-23); owner: `Owner-S2`; branch: `agent/b1-streaming-integration`; scope: Streaming engine integration against supported NeMo API with explicit fallback signaling (`parakeet-stt-daemon/src/parakeet_stt_daemon/model.py`).
7. `A7` — status: `done` (2026-02-23); owner: `Owner-S2`; branch: `agent/c2-c3-perf-guardrails`; scope: Helper policy and operator profiles (`scripts/stt-helper.sh`): default helper launch is offline-first (`PARAKEET_STREAMING_ENABLED=false`) with explicit streaming opt-in (`PARAKEET_STREAMING_ENABLED=true`).
8. `A8` — status: `done` (2026-02-25); owner: `Owner-M1`; branch: `agent/a8-offline-bench-harness`; scope: Offline benchmark harness implemented in `check_model.py` with committed bench dataset inputs, per-sample + aggregate WER/infer/finalize metrics, JSON report output, and configurable regression thresholds with non-zero exit on failure.

## Metrics to Track

- Time-to-confidence after hotkey press (user knows if system is listening).
- Paste success rate per backend and app surface.
- Fallback/copy-only event rate.
- User-reported "had to check logs" frequency.
- Recurring dictation-error correction hit rate (matches applied vs reverted).
- Active-session leak count after disconnect/error scenarios.
- Stop-session finalize latency (`p50`/`p95`/`p99`) in offline and streaming profiles.
- Streaming-active truth ratio (`streaming requested` vs `streaming engine active`).

## Streaming-Dependent Features (Blocked)

The following UX improvements require streaming mode to be functionally active, explicitly reported, and validated by the P-1 hardening gate:

1. "Ready to release" audio cue: Play a sound when transcription settles (no new words for
N ms) while the user is still holding, signaling they can release without truncation.

2. Live transcript preview: Show partial transcription in TUI/GUI overlay as the user speaks.

3. Confidence-based early termination: Detect silence plus high confidence to auto-stop
session without waiting for button release.

Current default policy uses offline helper launch (`stt start`,
`PARAKEET_STREAMING_ENABLED=false`) with explicit streaming opt-in
(`PARAKEET_STREAMING_ENABLED=true`) for targeted validation and troubleshooting.

## Non-Goals (Near-Term)

- Cloud telemetry.
- Non-local speech processing.
- Heavy compositor-specific integrations as default path.
