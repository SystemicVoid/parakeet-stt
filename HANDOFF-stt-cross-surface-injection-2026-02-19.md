# Handoff: STT Cross-Surface Injection Reliability

Date: 2026-02-19  
Repo: `parakeet-stt`  
Author: Codex + operator session

## 1. Executive Summary

Cross-surface injection was failing because a single static paste chord cannot satisfy both terminal-class and editor/browser-class targets on COSMIC Wayland:
- terminal/TUI surfaces generally require `Ctrl+Shift+V`
- editor/browser/notion-like surfaces generally require `Ctrl+V`

This handoff now reflects **implemented changes** (not only diagnosis). The fix introduces **adaptive routing** in `parakeet-ptt` that chooses the paste chord based on focused-surface metadata, while preserving existing backend reliability controls (`uinput -> ydotool -> wtype`, clipboard readiness barrier, copy-only policy).

## 1.1 Git Commit Map (Atomic Units)

1. `e1113c8` `feat(ptt): add adaptive cross-surface paste routing`
- Core Rust implementation:
  - routing config/CLI
  - focus resolver
  - adaptive injector integration
  - tests and dependency updates

2. `cf59d57` `chore(helper): wire adaptive routing flags through stt start`
- Shell helper defaults + forwarding for all new routing controls.

3. `76c6c5d` `docs(runtime): document adaptive routing defaults and focus baseline`
- Runtime docs updated for operator-facing defaults and COSMIC focus settings baseline.

4. `6af47e6` `docs(handoff): publish full implementation handoff for cross-surface fix`
- Full implementation handoff (this file).

## 2. Problem Context (Pre-Fix)

Observed behavior before implementation:
- `Ctrl+Shift+V` profile worked in terminal-like/browser-notion mixes but failed in local editor/VS Code.
- `Ctrl+V` profile worked in editor/browser, failed in terminal/TUI.
- Daemon/session pipeline was healthy; failures were in app-surface acceptance of synthetic chord semantics.

Root issue:
- `paste_strategy=single` + one static shortcut is under-specified for mixed app classes.

## 3. Product/Architecture Decisions Made

### 3.1 Injection strategy
- **Chosen:** adaptive per-surface routing.
- **Rejected:** static profile toggling (manual friction) and always-chain default (duplicate/focus churn risk).

### 3.2 X11 compatibility layer
- **Chosen:** no X11 injection path.
- **Why:** target stack is Wayland-first; mixed X11 injection would increase ambiguity and not reliably solve native Wayland targets.

### 3.3 Unknown-focus fallback
- **Chosen:** terminal-first (`ctrl-shift-v`) for unknown classification.
- **Why:** terminal/TUI use case is critical in this repo workflow.

### 3.4 Focus metadata source
- **Chosen:** AT-SPI accessibility bus lookup (Wayland-compatible userspace metadata).
- **Why:** available in-session and provides app/window descriptors without adding compositor-specific protocol dependency.

### 3.5 Existing reliability controls
- **Kept as-is:** clipboard choreography, backend ladder, backend failure policy, paste strategy semantics.

## 4. Implemented Work

## 4.1 New routing/config surfaces
Files:
- `parakeet-ptt/src/config.rs`
- `parakeet-ptt/src/main.rs`

Added config/API:
- `PasteRoutingMode::{Static, Adaptive}`
- `ClipboardOptions` additions:
  - `routing_mode`
  - `adaptive_terminal_shortcut`
  - `adaptive_general_shortcut`
  - `adaptive_unknown_shortcut`

Added CLI flags:
- `--paste-routing-mode static|adaptive`
- `--adaptive-terminal-shortcut ctrl-v|ctrl-shift-v|shift-insert`
- `--adaptive-general-shortcut ctrl-v|ctrl-shift-v|shift-insert`
- `--adaptive-unknown-shortcut ctrl-v|ctrl-shift-v|shift-insert`

## 4.2 Adaptive routing engine
File:
- `parakeet-ptt/src/routing.rs`

What it does:
- Classifies focus as `terminal`, `general`, or `unknown` using normalized hints (`app_name`, object name/path/service).
- Produces route decision:
  - primary shortcut
  - optional adaptive fallback shortcut (deduped)
  - reason + class for observability.

Behavior:
- `routing_mode=static` uses legacy `paste_shortcut` behavior.
- `routing_mode=adaptive` selects:
  - terminal-like -> terminal shortcut
  - editor/browser-like -> general shortcut
  - unknown -> unknown shortcut (terminal-first by default)

## 4.3 Focus metadata resolver
File:
- `parakeet-ptt/src/surface_focus.rs`

What it does:
- Resolves AT-SPI bus address via `org.a11y.Bus.GetAddress`.
- Walks AT-SPI accessible roots/children.
- Uses accessibility state bits to detect active/focused objects.
- Emits `FocusSnapshot` for router consumption.

Dependency added:
- `regex` in `parakeet-ptt/Cargo.toml` and `Cargo.lock` for robust parsing of `gdbus` output payloads.

## 4.4 Injector integration
File:
- `parakeet-ptt/src/injector.rs`

What changed:
- `ClipboardInjector` now carries a focus resolver.
- Per injection:
  1. resolve focus snapshot
  2. compute route decision
  3. run shortcut attempts using routed primary + adaptive fallback + explicit fallback according to strategy
- Added route/focus structured logs (`route_class`, `route_primary`, focus app/object metadata, reason).

What did not change:
- Backend sender ladder logic
- Clipboard readiness barrier and post-chord hold
- restore policy and copy-only semantics

## 4.5 Helper integration and defaults
File:
- `scripts/stt-helper.sh`

Added env/defaults and forwarded args:
- `PARAKEET_PASTE_ROUTING_MODE` (default `adaptive`)
- `PARAKEET_ADAPTIVE_TERMINAL_SHORTCUT` (default `ctrl-shift-v`)
- `PARAKEET_ADAPTIVE_GENERAL_SHORTCUT` (default `ctrl-v`)
- `PARAKEET_ADAPTIVE_UNKNOWN_SHORTCUT` (default `ctrl-shift-v`)

Updated:
- `stt start` option parsing and startup summary
- tmux/cargo runner arg forwarding
- `stt diag-injector` forwarding for new routing flags

## 4.6 Documentation updates
Files:
- `README.md`
- `docs/stt-troubleshooting.md`

Updated to reflect:
- adaptive default routing
- new CLI/control surfaces
- COSMIC focus-navigation baseline for best adaptive behavior:
  - focus follows cursor: ON
  - focus follows cursor delay: 0ms
  - cursor follows focus: ON

## 5. Validation Performed

In `parakeet-ptt`:
- `cargo fmt`
- `cargo test`
- `cargo clippy --all-targets --all-features -- -D warnings`

Result:
- tests passing (`24 passed, 0 failed`)
- clippy clean under `-D warnings`

Helper script:
- `bash -n scripts/stt-helper.sh`

## 6. Runtime Behavior After Change

Default routing profile is now adaptive and terminal-safe on unknown focus:
- terminal-like focus -> `ctrl-shift-v`
- general/editor/browser focus -> `ctrl-v`
- unknown focus -> `ctrl-shift-v`

If focus metadata resolution fails:
- injector logs warning and falls back deterministically to unknown route (terminal-first).

## 7. Tradeoffs and Residual Risks

1. AT-SPI dependency
- Pros: available now, no compositor-specific protocol binding.
- Risk: some apps may expose sparse/odd accessibility trees or stale "active" hints.

2. Heuristic classification
- Pros: practical and low-friction now.
- Risk: unknown/new apps may classify as unknown and get terminal-first behavior.

3. No X11 injection path
- Pros: architecture remains Wayland-native and predictable.
- Risk: does not exploit X11-specific automation semantics for Xwayland edge cases.

4. Focus policy sensitivity
- Desktop focus settings can materially affect where synthetic input lands.
- Operator has already set focus delay to `0ms`; keep as operational baseline.

## 8. Suggested Post-Merge Validation Matrix

Run manual matrix with 20 attempts/cell:
- Ghostty prompt
- COSMIC Terminal prompt
- VS Code editor
- COSMIC Text Editor
- Brave field
- Notion field

Across:
- `--paste-key-backend auto`
- `--paste-key-backend uinput`

Record:
- insert success rate
- duplicate insertion rate
- focus loss/churn incidents

## 9. Rollback / Safe Fallback

If adaptive routing regresses for a user:
- temporary static mode:
  - `--paste-routing-mode static --paste-shortcut <desired>`
- or copy-only safety:
  - `--injection-mode copy-only`

## 10. Summary of Outcome

This implementation closes the original cross-surface gap by making shortcut choice context-aware while preserving existing reliability hardening. It deliberately avoids an X11-comp-layer injection branch and keeps behavior observable and recoverable via explicit flags.

## 11. Current State Diagnostics (2026-02-19 PM Follow-up)

This section records post-implementation runtime diagnostics after additional operator repros in COSMIC Terminal/Ghostty.

### 11.1 What is confirmed working

1. Updated helper defaults are now loaded in fresh shells:
- `stt start` summary includes:
  - `Paste routing mode: adaptive`
  - `Adaptive terminal shortcut: ctrl-shift-v`
  - `Adaptive general shortcut: ctrl-v`
  - `Adaptive unknown shortcut: ctrl-shift-v`

2. Updated client binary now exists and exposes adaptive flags:
- `parakeet-ptt/target/release/parakeet-ptt --help` includes:
  - `--paste-routing-mode`
  - `--adaptive-terminal-shortcut`
  - `--adaptive-general-shortcut`
  - `--adaptive-unknown-shortcut`

3. Runtime adaptive mode is active in logs:
- `Using clipboard injector ... paste_routing_mode=Adaptive ...`

### 11.2 Diagnosed startup issues

1. First launch after update hit stale-binary fallback + compile race:
- `stt` fell back to `cargo run --release` (release binary check failed for new flags).
- Log evidence:
  - `Finished 'release' profile [optimized] target(s) in 11.79s`
  - followed by `Running 'target/release/parakeet-ptt ... --paste-routing-mode adaptive ...'`

2. Helper emitted false negative while compile was still in progress:
- `stt` waits up to ~10s for `parakeet-ptt` PID (`20 * 0.5s` loop in helper).
- Compile took longer (~11.79s), so helper printed:
  - `Client did not stay up; recent client log: ...`
- Client then started successfully once compile completed.

3. `can't find pane: 0` is explained by tmux index configuration mismatch:
- Helper targets pane `.0`.
- User tmux config is 1-based:
  - `set -g base-index 1`
  - `setw -g pane-base-index 1`

### 11.3 Diagnosed routing/focus mismatch during terminal repros

During the latest adaptive runs, all captured route decisions resolved to Brave metadata, not terminal/editor targets:

- 9/9 routing snapshots:
  - `focus_app="Brave Browser"`
  - `focus_object="Gemini - Brave"`
  - `focus_active=true`
  - `focus_focused=false`
  - `route_class=General`
  - `route_primary=CtrlV`
  - `route_adaptive_fallback=Some(CtrlShiftV)`

Implication:
- When dictating while expecting terminal behavior, adaptive routing still chose `CtrlV` because focus resolver reported browser/general context.
- This matches observed symptom where terminal received literal `ctrl+v` rather than paste.

### 11.4 Remaining issues (open)

1. Focus snapshot staleness/misattribution under AT-SPI:
- Resolver frequently returns active browser object with `focus_focused=false`.
- Needs further refinement so active dictation target is resolved more reliably for rapid cross-app switching.

2. Helper UX issues:
- PID readiness timeout is too short for a cold `cargo run --release` compile.
- Pane selection should not assume zero-based pane index on user tmux setups.

### 11.5 Next diagnostics to run (before code changes)

1. Capture focused-surface logs while intentionally dictating in:
- COSMIC Terminal
- Ghostty
- VS Code
- COSMIC Editor

2. For each run, record:
- `focus_app`, `focus_object`, `focus_active`, `focus_focused`
- `route_class`, `route_primary`, `route_reason`
- observed insertion behavior in target app

3. Confirm whether misroutes correlate with:
- `focus_focused=false` snapshots
- stale active app metadata from previous surface

## 12. Execution Plan and Progress Ledger (Post-Diagnostics)

This section tracks implementation in atomic units so work can resume after context loss.

### 12.1 Task checklist

- [x] T1 Add checklist/ledger scaffolding and validation template in this handoff.
- [x] T2 Fix helper cold-start false negative during `cargo run --release` compile windows.
- [x] T3 Fix helper tmux pane selection to avoid zero-based index assumptions.
- [ ] T4 Harden adaptive routing when AT-SPI snapshot is low-confidence (`focus_focused=false`).
- [ ] T5 Add AT-SPI `gdbus` timeout bounds to prevent routing stalls.
- [ ] T6 Run validation matrix, update docs, and close out residual risks.

### 12.2 Commit ledger

| Task | Scope | Commit | Status | Notes |
|---|---|---|---|---|
| T1 | `HANDOFF-stt-cross-surface-injection-2026-02-19.md` | `27f14a9` | Done | Added checklist, task IDs, commit ledger, and validation template |
| T2 | `scripts/stt-helper.sh`, handoff | `b18575a` | Done | Added timeout-based readiness loop with compile-aware wait extension |
| T3 | `scripts/stt-helper.sh`, handoff | pending (this commit) | Done | Switched to pane-id based selection instead of `.0` target |
| T4 | `parakeet-ptt/src/routing.rs`, `parakeet-ptt/src/injector.rs`, tests, handoff | pending | Not started | Degrade low-confidence focus to unknown route |
| T5 | `parakeet-ptt/src/surface_focus.rs`, tests, handoff | pending | Not started | Add explicit `gdbus` timeouts |
| T6 | handoff + runtime docs (if needed) | pending | Not started | Record validation outcomes and remaining risks |

## 13. Validation Log Template

Use this to capture evidence during/after T2-T6:

| Date | Surface | Backend | Observed focus metadata | Route decision | Injection outcome | Notes |
|---|---|---|---|---|---|---|
| 2026-02-19 | COSMIC Terminal | auto | (fill) | (fill) | (fill) | |
| 2026-02-19 | Ghostty | auto | (fill) | (fill) | (fill) | |
| 2026-02-19 | VS Code | auto | (fill) | (fill) | (fill) | |
| 2026-02-19 | COSMIC Editor | auto | (fill) | (fill) | (fill) | |
| 2026-02-19 | Brave | auto | (fill) | (fill) | (fill) | |
