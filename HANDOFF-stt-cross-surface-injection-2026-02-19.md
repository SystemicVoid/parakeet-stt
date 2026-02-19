# Handoff: STT Cross-Surface Injection Reliability

Date: 2026-02-19  
Repo: `parakeet-stt`  
Author: Codex + operator session

## 1. Executive Summary

Cross-surface injection was failing because a single static paste chord cannot satisfy both terminal-class and editor/browser-class targets on COSMIC Wayland:
- terminal/TUI surfaces generally require `Ctrl+Shift+V`
- editor/browser/notion-like surfaces generally require `Ctrl+V`

This handoff now reflects **implemented changes** (not only diagnosis). The fix introduces **adaptive routing** in `parakeet-ptt` that chooses the paste chord based on focused-surface metadata, while preserving existing backend reliability controls (`uinput -> ydotool -> wtype`, clipboard readiness barrier, copy-only policy).

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
