# Parakeet STT Product Roadmap

Date: 2026-02-08

## Goal

Turn Parakeet STT from "works with logs" into a premium local dictation experience with clear, low-latency user feedback and predictable behavior across terminals and browsers.

## Current Baseline

- Core STT loop is operational and low-latency.
- Injection stack has robust controls (`paste` mode, key backend, failure policy).
- Main UX gap is user feedback: state is visible in logs, not in an obvious UI channel.

## UX Principles

- State must be obvious without opening logs.
- Feedback must be immediate (<100ms perceived response when possible).
- Failure states must be explicit and recoverable.
- Advanced tuning remains available, but defaults stay minimal and reliable.

## P0: Injection Architecture Hardening (Highest Priority)

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

1. Complete P0 injection architecture hardening and lock reliability baselines.
2. Ship Phase 1 cues behind a feature flag, default on for sound cues only.
3. Add Phase 1 tests and update docs.
4. Ship TUI skeleton with existing state only.
5. Extend TUI with injection outcomes.
6. Add incremental dictation error correction loop behind an opt-in switch.
7. Prototype GUI after TUI reaches stable daily use.

## Metrics to Track

- Time-to-confidence after hotkey press (user knows if system is listening).
- Paste success rate per backend and app surface.
- Fallback/copy-only event rate.
- User-reported "had to check logs" frequency.
- Recurring dictation-error correction hit rate (matches applied vs reverted).

## Streaming-Dependent Features (Blocked)

The following UX improvements require streaming mode to be stable and default:

1. "Ready to release" audio cue: Play a sound when transcription settles (no new words for
N ms) while the user is still holding, signaling they can release without truncation.

2. Live transcript preview: Show partial transcription in TUI/GUI overlay as the user speaks.

3. Confidence-based early termination: Detect silence plus high confidence to auto-stop
session without waiting for button release.

Streaming mode exists (`--streaming`) but is not yet default due to architectural differences
in how audio chunks are processed. Stabilizing streaming is a prerequisite for these features.

## Non-Goals (Near-Term)

- Cloud telemetry.
- Non-local speech processing.
- Heavy compositor-specific integrations as default path.
