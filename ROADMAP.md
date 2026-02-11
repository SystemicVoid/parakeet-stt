# Parakeet STT Product Roadmap

Date: 2026-02-08

## Goal

Turn Parakeet STT from "works with logs" into a premium local dictation experience with clear, low-latency user feedback and predictable behavior across terminals and browsers.

## Current Baseline

- Core STT loop is operational and low-latency.
- Injection stack has robust controls (`paste` strategy/backends/failure policy).
- Main UX gap is user feedback: state is visible in logs, not in an obvious UI channel.

## UX Principles

- State must be obvious without opening logs.
- Feedback must be immediate (<100ms perceived response when possible).
- Failure states must be explicit and recoverable.
- Advanced tuning remains available, but defaults stay minimal and reliable.

## Phase 1: Immediate Feedback Layer (Sound + Notification)

Scope:
- Add short, distinct cues for:
  - ready
  - listening start
  - listening stop
  - transcript injected ✓ (implemented 2026-02-11, but see limitation below)
  - injection fallback/copy-only
  - hard error
- Add optional desktop notification summaries for error/fallback events.

Implementation direction:
- Rust-side event emission from `parakeet-ptt` state transitions.
- Configurable cue backend (`none|sound|notify|sound+notify`).
- Keep defaults lightweight and local-only.

**Known limitation (completion sound):**
Current completion sound plays *after* button release because offline mode only
transcribes once `stop_session` is received. The sound confirms completion but
cannot signal "ready to release" during long utterances. Fixing this requires
streaming mode where partial results arrive while holding, enabling a "settled"
detection (no new words for N ms) to trigger a release-ready cue.

Acceptance:
- User can run one dictation cycle without looking at logs and correctly infer system state.
- No audible spam during normal repeated use.

## Phase 2: Compact TUI Overlay

Scope:
- Add a tiny terminal dashboard mode with:
  - connection status
  - hotkey state (`Idle`, `Listening`, `WaitingResult`)
  - last transcript preview (truncated)
  - injection backend used (`uinput|ydotool|wtype|copy-only`)
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

Acceptance:
- Users can tell whether failure is ASR, clipboard, backend init, or target-app acceptance.
- First-run experience requires minimal tuning for common apps.

## Delivery Strategy (Atomic)

1. Ship Phase 1 cues behind a feature flag, default on for sound cues only.
2. Add Phase 1 tests and update docs.
3. Ship TUI skeleton with existing state only.
4. Extend TUI with injection outcomes.
5. Prototype GUI after TUI reaches stable daily use.

## Metrics to Track

- Time-to-confidence after hotkey press (user knows if system is listening).
- Paste success rate per backend and app surface.
- Fallback/copy-only event rate.
- User-reported "had to check logs" frequency.

## Streaming-Dependent Features (Blocked)

The following UX improvements require streaming mode to be stable and default:

1. **"Ready to release" audio cue** - Play sound when transcription settles (no new
   words for N ms) while user is still holding, signaling they can release without
   truncation. Current offline mode cannot provide this because transcription only
   starts after button release.

2. **Live transcript preview** - Show partial transcription in TUI/GUI overlay as
   user speaks. Requires streaming partial results.

3. **Confidence-based early termination** - Detect silence + high confidence to
   auto-stop session without waiting for button release.

Streaming mode exists (`--streaming`) but is not yet default due to architectural
differences in how audio chunks are processed. Stabilizing streaming is a
prerequisite for these features.

## Non-Goals (Near-Term)

- Cloud telemetry.
- Non-local speech processing.
- Heavy compositor-specific integrations as default path.
