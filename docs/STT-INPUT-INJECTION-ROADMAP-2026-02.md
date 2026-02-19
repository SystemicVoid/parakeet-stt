# STT Input Injection Roadmap (uinput-first)

Date: 2026-02-08  
Repository: `parakeet-stt`

## 1. Purpose

This document synthesizes all current AI-generated research with the actual codebase state and runtime evidence, then defines a decision-complete roadmap for the next implementation cycle.

Primary objective:
- Keep the current working STT flow available for daily use.
- Build a more reliable injection stack on an isolated branch/worktree.

## 2. Current System Baseline (What Is Already Implemented)

The following are already present in `parakeet-ptt` and helper tooling:

1. Strategy-driven paste choreography in `parakeet-ptt/src/injector.rs`.
- `PasteStrategy`: `single`, `on-error`, `always-chain`.
- Shortcut chain support with inter-chord delay.

2. Clipboard readiness barrier before paste chord.
- `wl-copy` write followed by `wl-paste` readback polling.
- Timeout-based continue behavior with structured outcome logging.

3. Ownership and race mitigations.
- Optional foreground ownership during paste (`--paste-copy-foreground`).
- Post-chord hold (`--paste-post-chord-hold-ms`).
- Restore policy (`never` or `delayed`).

4. Multiple key backend options for chord injection.
- `wtype`, `ydotool`, `uinput`, `auto` backend selection surface.
- `auto` runtime ladder: `uinput -> ydotool -> wtype`.

5. Explicit backend failure policy.
- `--paste-backend-failure-policy copy-only|error`.
- Default is `copy-only` to preserve transcript delivery when key backend init fails.

6. Operational fallback mode.
- `--injection-mode copy-only` for deterministic clipboard-only behavior.

7. Diagnostics and helper wiring.
- `stt` helper forwards advanced paste flags.
- `stt start` rejects unknown options to prevent silent misconfiguration.
- `stt diag-injector` runs reproducible test-injection combinations.

## 3. Known Remaining Problem

Logs show process-level injection success (`success_assumed`), but UI-level insertion remains inconsistent in some app/surface combinations (notably Ghostty/COSMIC + some browser fields).  
Current gap: process-success telemetry is stronger than app-level semantic-success telemetry.

## 4. Research Synthesis (Critically Filtered)

## High-confidence findings (aligned with repo evidence)

1. `virtual-keyboard-v1` via `wtype` is inconsistent across compositor/app combinations for this use case.
2. Clipboard-readiness wait is necessary to reduce stale paste races.
3. Chord-only success is insufficient as proof of UI insertion success.
4. A kernel-level backend (`uinput`) is a pragmatic near-term reliability path.

## Medium-confidence findings (plausible, not fully validated here)

1. `uinput` can improve modifier ordering reliability on problematic targets.
2. Per-app shortcut profiles outperform one global chained strategy.
3. Focus churn from aggressive chained shortcuts can cause semantic no-op in some fields.

## Low-confidence or deferred claims

1. Anti-cheat-driven spoofing/jitter is useful only for specific environments and is not default product behavior.
2. IME/AT-SPI/libei architecture is likely long-term direction but not a near-term delivery blocker.

## 5. Strategic Decision

Chosen path: **uinput-first**.

Rationale:
1. It targets immediate reliability without waiting for compositor-native protocol maturity.
2. It complements, rather than invalidates, current mitigations already in tree.
3. It can be isolated behind backend selection so rollback is low-risk.

## 6. Branch and Worktree Isolation

To keep the current runtime available while building the next version:

1. Keep daily usage on existing tree/branch (`main`).
2. Implement roadmap work on:
- Branch: `feat/uinput-first-injector`
- Worktree path: `../parakeet-stt-uinput`

Rules:
1. No runtime-default flips on `main` during this effort.
2. Merge only after acceptance matrix passes.
3. Maintain atomic commits by logical change.

## 7. Target Architecture (Near-Term)

Primary goal: add a first-class direct `uinput` backend in Rust, while keeping current backends available.

Planned backend hierarchy:
1. `uinput` (new, primary candidate for reliability track)
2. `ydotool` (existing uinput-backed subprocess fallback)
3. `wtype` (existing protocol path fallback)
4. `copy-only` (operational safety fallback)

Planned injection flow:
1. Copy transcript to clipboard (`wl-copy`).
2. Readiness barrier (`wl-paste` hash/value match with timeout).
3. Emit paste chord via selected backend.
4. Optional post-chord hold.
5. Optional restore policy handling.
6. Outcome classification and telemetry emission.

## 8. Implementation Phases

## Phase 0: Baseline Lock + Instrumentation Tightening (Completed)

Scope:
1. Freeze baseline behavior in worktree with no default changes.
2. Extend logging fields to distinguish:
- command-level success
- clipboard-state convergence
- fallback activation
- backend chosen

Acceptance:
1. Existing tests pass.
2. New logs make backend behavior and fallback path unambiguous.

## Phase 1: Direct `uinput` Backend MVP (Completed)

Scope:
1. Add `uinput` backend implementation in Rust (no external daemon dependency).
2. Extend CLI/config enums to include `uinput`.
3. Add backend-specific timing controls with safe defaults (dwell/jitter optional, conservative defaults).

Acceptance:
1. Build/test/format checks pass.
2. `--test-injection` works with `--paste-key-backend uinput`.
3. Fail-fast with clear message when `/dev/uinput` permission is missing.

## Phase 2: Reliability Ladder and Fallback Semantics (In Progress)

Scope:
1. Define deterministic fallback order (configurable).
2. Allow fallback on both hard errors and explicit timeout policies.
3. Keep fallback behavior observable in logs/metrics.

Implemented in this phase:
1. Runtime backend ladder for `auto`: `uinput -> ydotool -> wtype`.
2. Explicit backend failure policy:
- `copy-only` preserves transcript via clipboard-only path.
- `error` surfaces explicit injector failure.
3. Unit test coverage for chain fallback and error-policy path.

Remaining in this phase:
1. Add app-level semantic success evidence (not only command/clipboard success).
2. Run and record repeated multi-app acceptance matrix results.

Acceptance:
1. Unit tests cover attempt ordering and fallback transitions.
2. Manual runs confirm predictable behavior across backends.

## Phase 3: Operations and System Integration

Scope:
1. Add udev rule template and setup/verification docs.
2. Extend helper diagnostics for `uinput` capability checks.
3. Keep current helper defaults stable until promotion gate.

Acceptance:
1. Non-root user with proper group can run `uinput` backend.
2. Helper outputs clear remediation when permissions are absent.

## Phase 4: Acceptance Matrix + Default Decision

Matrix (must pass in repeated runs):
1. Ghostty prompt.
2. COSMIC Terminal prompt.
3. Brave address bar and text field.
4. GTK/native text entry.
5. `copy-only` non-regression.

Decision gate:
1. Promote backend default only if matrix pass rate exceeds current default and no severe regressions are found.
2. Otherwise ship as opt-in backend and continue hardening.

Historical default policy at roadmap authoring time (2026-02-08):
1. Keep `--paste-key-backend wtype`.
2. Keep `--paste-backend-failure-policy copy-only`.

Current defaults on `main` have since changed; see top-level `README.md` for active runtime behavior.

Acceptance matrix recording template:

| Surface | Backend | Attempts | Successes | Failure class | Notes |
|---------|---------|----------|-----------|---------------|-------|
| Ghostty prompt | wtype |  |  |  |  |
| Ghostty prompt | uinput |  |  |  |  |
| COSMIC Terminal prompt | wtype |  |  |  |  |
| COSMIC Terminal prompt | uinput |  |  |  |  |
| Brave address bar | wtype |  |  |  |  |
| Brave address bar | uinput |  |  |  |  |
| GTK/native text entry | wtype |  |  |  |  |
| GTK/native text entry | uinput |  |  |  |  |
| Clipboard-only regression check | copy-only |  |  |  |  |

## Phase 5: Deferred R&D Track (Non-blocking)

Scope:
1. IME/AT-SPI/libei feasibility prototype.
2. Evaluate if compositor-native path can become long-term primary.

This phase does not block the `uinput-first` reliability deliverable.

## 9. Security and Risk Register

1. `uinput` permissions risk.
- Risk: broader input capabilities than protocol-scoped injection.
- Mitigation: least-privilege group, explicit docs, no root process, clear install boundary.

2. Focus/target ambiguity.
- Risk: synthetic keys land in wrong focused surface.
- Mitigation: short post-trigger delay options, app-profile tuning, conservative chaining defaults.

3. Configuration complexity.
- Risk: too many knobs reduce operability.
- Mitigation: documented baseline presets + helper command profiles.

4. False confidence from process-level success.
- Risk: command success without UI insertion.
- Mitigation: acceptance matrix and app-level verification before default switch.

## 10. Test and Verification Plan

Automated:
1. `cargo fmt`
2. `cargo clippy --all-targets --all-features -- -D warnings`
3. `cargo test`

Behavioral:
1. `stt diag-injector` extended to include `uinput` capability checks and matrix runs.
2. Repeated multi-app insertion matrix runs with backend-specific logs.
3. Permission-denied and missing-device error-path checks.

## 11. Definition of Done

1. Roadmap branch contains a working `uinput` backend selectable via CLI/helper.
2. Operational docs for permissions/setup are complete.
3. Acceptance matrix results are recorded and reproducible.
4. Merge recommendation includes explicit go/no-go default decision.

## 12. Cross-References

1. `docs/HANDOFF-clipboard-injector-2026-02-08.md`
2. `docs/SYSTEM-SPECS-and-FULL-STACK-REFERENCES-2026-02-08.md`
3. `docs/stt-troubleshooting.md`
