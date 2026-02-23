# Handoff: STT Cross-Surface Injection Reliability

Date: 2026-02-19
Repo: `parakeet-stt`
Author: Codex + operator session

## Archive Status (2026-02-23)

- Incident is closed.
- This handoff is archived in `docs/` for historical implementation context.
- Use `README.md`, `docs/SPEC.md`, and `docs/stt-troubleshooting.md` for current runtime guidance.

## Status Update (2026-02-21)

- Active runtime no longer includes `wtype`/`type` injection paths.
- Current backend ladder is `uinput -> ydotool` with `copy-only` failure policy.
- Keep this handoff for historical investigation context; use `README.md`, `docs/SPEC.md`, and `docs/stt-troubleshooting.md` for current operator truth.

## 1. Executive Summary

Cross-surface injection was failing because a single static paste chord cannot satisfy both terminal-class and editor/browser-class targets on COSMIC Wayland:
- terminal/TUI surfaces generally require `Ctrl+Shift+V`
- editor/browser/notion-like surfaces generally require `Ctrl+V`

This handoff now reflects **implemented changes** (not only diagnosis). The fix introduces **adaptive routing** in `parakeet-ptt` that chooses the paste chord based on focused-surface metadata, while preserving existing backend reliability controls (`uinput -> ydotool`, clipboard readiness barrier, copy-only policy).

Current reality check (2026-02-20):
- terminal/browser paths are still materially better than editor paths.
- VS Code editor and COSMIC Text Editor remain unreliable in latest operator tests.
- current failures are no longer explained by daemon/ASR/session health; they remain focus-resolution/routing-path failures.

## 1.2 Current Status Override (2026-02-20)

Historical note at that point in time:
- Adaptive routing and Wayland focus cache infrastructure are implemented.
- Cross-surface correctness is still unresolved for editor targets in real desktop use.
- Latest operator observation: Chromium continues to accept injection in cases where VS Code and COSMIC Text Editor do not.

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
- [x] T4 Harden adaptive routing when AT-SPI snapshot is low-confidence (`focus_focused=false`).
- [x] T5 Add AT-SPI `gdbus` timeout bounds to prevent routing stalls.
- [x] T6 Run validation matrix, update docs, and close out residual risks.

### 12.2 Commit ledger

| Task | Scope | Commit | Status | Notes |
|---|---|---|---|---|
| T1 | `docs/HANDOFF-stt-cross-surface-injection-2026-02-19.md` | `27f14a9` | Done | Added checklist, task IDs, commit ledger, and validation template |
| T2 | `scripts/stt-helper.sh`, handoff | `b18575a` | Done | Added timeout-based readiness loop with compile-aware wait extension |
| T3 | `scripts/stt-helper.sh`, handoff | `9d9c5b1` | Done | Switched to pane-id based selection instead of `.0` target |
| T4 | `parakeet-ptt/src/routing.rs`, `parakeet-ptt/src/injector.rs`, tests, handoff | `9359f59` | Done | Route degrades to unknown when `focus_focused=false`; added confidence logging/tests |
| T5 | `parakeet-ptt/src/surface_focus.rs`, tests, handoff | `442565b` | Done | Added bounded `gdbus` timeouts (`--timeout 2`) for AT-SPI lookup calls |
| T6 | handoff + runtime docs | `fed209d` | Done | Recorded validation outcomes, doc updates, and remaining manual matrix |

### 12.3 Validation summary (post-fix)

Automated checks run:
- `bash -n scripts/stt-helper.sh` -> pass
- `cd parakeet-ptt && cargo test` -> pass (`25 passed, 0 failed`)
- `cd parakeet-ptt && cargo clippy --all-targets --all-features -- -D warnings` -> pass
- `source scripts/stt-helper.sh && stt diag-injector` -> pass (capability checks + 3 test cases)

Runtime observation from `diag-injector` after `cargo build --release`:
- When AT-SPI returns `focus_focused=false`, routing now emits:
  - `route_class=Unknown`
  - `route_primary=CtrlShiftV`
  - `route_low_confidence=true`
  - `route_reason=\"adaptive low-confidence focus snapshot (focused=false)\"`

Operational note:
- `diag-injector` prefers `target/release/parakeet-ptt` when present.
- If the release binary is stale, diagnostics may reflect old behavior until rebuilt (`cargo build --release`).

## 13. Validation Log Template

Use this to capture evidence during/after T2-T6:

| Date | Surface | Backend | Observed focus metadata | Route decision | Injection outcome | Notes |
|---|---|---|---|---|---|---|
| 2026-02-19 | COSMIC Terminal | auto | (fill) | (fill) | (fill) | |
| 2026-02-19 | Ghostty | auto | `focus_app=\"Unnamed\"`, `focus_focused=false` | `Unknown -> CtrlShiftV` (`route_low_confidence=true`) | `success_assumed` | from `stt diag-injector` |
| 2026-02-19 | VS Code | auto | (fill) | (fill) | (fill) | |
| 2026-02-19 | COSMIC Editor | auto | (fill) | (fill) | (fill) | |
| 2026-02-19 | Brave | auto | `focus_app=\"Brave Browser\"`, `focus_focused=false` | `Unknown -> CtrlShiftV` (`route_low_confidence=true`) | `success_assumed` | from `stt diag-injector` |

## 14. Remaining Manual Matrix

Still required (interactive operator validation):
- 20-attempt matrix per surface for:
  - COSMIC Terminal, Ghostty, VS Code, COSMIC Editor, Brave, Notion
- Backends:
  - `--paste-key-backend auto`
  - `--paste-key-backend uinput`
- Record:
  - insertion success rate
  - duplicate insertion rate
  - focus churn incidents

## 15. Session Continuation Handoff (2026-02-19 15:02 UTC)

This section captures the exact state at the point of operator-requested agent restart.

### 15.1 What changed during this diagnostic slice

1. User confirmed cross-surface behavior:
- Works in terminal/browser surfaces (Ghostty, Notion, Brave bar).
- Still fails in editor surfaces (COSMIC Text Editor, VS Code editor panel).

2. Additional outage introduced during agent-driven restart:
- Symptom: no completion sound and no text injection anywhere.
- Client log showed repeated:
  - `Connection to daemon failed: ... Connection refused (os error 111)`
- Root cause:
  - `stt restart` was executed from a non-interactive one-shot shell.
  - Helper `start` launches daemon as a background process of that shell.
  - Shell exits after command completes; daemon receives SIGHUP and dies.
  - Client remains alive in tmux and continues retry loop.

3. Recovery performed:
- Daemon relaunched inside tmux window so it survives shell exit:
  - tmux session: `parakeet-stt`
  - window: `daemon-fix`
  - device override used: `PARAKEET_DEVICE=cpu`
- Confirmed at handoff time:
  - listener up on `127.0.0.1:8765`
  - daemon pid `131885`
  - client reconnected at `2026-02-19T15:02:15Z` (`Connected to daemon`)

### 15.2 Critical operational note for next agent

Do **not** run `stt start/restart` from a disposable one-shot shell command runner unless daemon is disowned/daemonized independently.

Use persistent tmux-managed runtime for both:
- Codex agent shell
- STT runtime shell/session

### 15.3 Recommended restart layout (next instance)

1. Create/attach two tmux sessions:
- `tmux new -As codex`
- `tmux new -As stt-runtime`

2. In `stt-runtime`, run helper interactively (not one-shot):
- `cd ~/Documents/Engineering/parakeet-stt`
- `source scripts/stt-helper.sh`
- `PARAKEET_DEVICE=cpu stt start`

3. Validate runtime before testing:
- `ss -ltnp | rg 8765`
- `tail -n 40 /tmp/parakeet-ptt.log`
- `tail -n 40 /tmp/parakeet-daemon.log`

### 15.4 Immediate next diagnostic task after restart

Resume the high-signal loop:
1. Keep live filter on injector lines only (`starting clipboard injection`, `resolved focused surface`, `clipboard injection flow finished`).
2. Re-run surface sweep:
- COSMIC Text Editor
- VS Code editor panel
- Notion/Brave bar
- COSMIC Terminal / Ghostty
3. Compare route/focus fields and confirm whether editor failures remain purely due low-confidence `Unknown -> CtrlShiftV` behavior or require resolver patch.

## 16. Resolver Deep-Scan Patch (2026-02-19 15:20 UTC)

### 16.1 New root-cause evidence

Additional raw AT-SPI probing showed the current resolver was too shallow:
- Existing resolver scanned only app roots + one child level.
- Real `focused=true` nodes can be much deeper:
  - Brave focused descendant found at `/org/a11y/atspi/accessible/29` (depth 8)
  - VS Code focused descendant found at `/org/a11y/atspi/accessible/530` (depth 29)
- This mismatch explains repeated `focus_focused=false` snapshots and `Unknown -> CtrlShiftV` routing when editor surfaces were expected to use `CtrlV`.

### 16.2 Implemented code changes

File:
- `parakeet-ptt/src/surface_focus.rs`

Changes:
1. Added active-first candidate ranking:
- ranking now prioritizes `active` over `focused-only` (`active+focused > active-only > focused-only`).

2. Added bounded focused-descendant probe:
- new `find_focused_descendant()` runs a depth-first scan for `focused=true`.
- scan is bounded (`MAX_FOCUS_SCAN_NODES=1024`) to avoid unbounded traversal.
- deep scan is only attempted for apps that are currently active in the shallow pass.

3. Added deterministic tie-break:
- for equal-ranked focused candidates, prefer deeper descendants.

4. Added unit tests:
- active-vs-focused ranking behavior
- focused tie-break behavior

### 16.3 Automated validation

Ran in `parakeet-ptt`:
- `cargo fmt` -> pass
- `cargo test` -> pass (`27 passed, 0 failed`)
- `cargo clippy --all-targets --all-features -- -D warnings` -> pass
- `cargo build --release` -> pass
- `source scripts/stt-helper.sh && stt diag-injector` -> pass

Key runtime evidence from `stt diag-injector` after release rebuild:
- Ghostty snapshot now resolved as focused terminal descendant:
  - `focus_object_path="/com/mitchellh/ghostty/..."`
  - `focus_active=true`
  - `focus_focused=true`
  - `route_class=Terminal`
  - `route_primary=CtrlShiftV`
- Brave snapshot now resolved as focused browser descendant:
  - `focus_object_path="/org/a11y/atspi/accessible/29"`
  - `focus_active=true`
  - `focus_focused=true`
  - `route_class=General`
  - `route_primary=CtrlV`

### 16.4 Runtime validation still required

Pending interactive matrix after rebuilding release binary:
- rebuild: `cd parakeet-ptt && cargo build --release`
- run surface sweep in live session:
  - COSMIC Text Editor
  - VS Code editor panel
  - Notion/Brave bar
  - COSMIC Terminal / Ghostty
- capture whether resolver now reports focused descendants on editor targets and routes to `CtrlV` as expected.

## 17. Latency Regression Follow-up (Fixed, 2026-02-19 15:45 UTC)

### 17.1 Root cause recap

The multi-second post-beep delay was in pre-chord focus resolution (`start` -> `resolved focused surface`), not in paste hold/chord completion.

Primary contributors:
- resolver deep-scanned every app that reported `active=true`
- per-call `gdbus` timeout was `2s`, allowing timeout accumulation across scans
- resolution happened synchronously in injection path

### 17.2 Fix implemented

Code paths updated:
- `parakeet-ptt/src/surface_focus.rs`
- `parakeet-ptt/src/injector.rs`
- `parakeet-ptt/src/config.rs`
- `parakeet-ptt/src/main.rs`
- `scripts/stt-helper.sh`

Behavior changes:
1. Added resolver controls:
   - `--focus-resolve-budget-ms` (default `450`)
   - `--focus-deep-scan-max-apps` (default `1`)
2. Deep scan now runs for at most one ranked active app (default), instead of fan-out across all active apps.
3. Added timeout-aware resolver stats:
   - `focus_resolve_ms`
   - `focus_resolve_timed_out`
   - `focus_resolve_gdbus_calls`
   - `focus_resolve_deep_scan_apps`
   - `focus_resolve_deep_scan_nodes`
4. Reduced `MAX_FOCUS_SCAN_NODES` from `1024` to `256`.
5. Reduced per-call `gdbus` timeout from `2s` to `1s`.
6. Helper/start paths now forward and print new focus resolver controls.

### 17.3 Validation (this session)

Build/test gates:
- `cargo fmt`
- `cargo clippy --all-targets --all-features -- -D warnings`
- `cargo test` (`28 passed`)
- `bash -n scripts/stt-helper.sh`

Runtime checks:
1. `stt restart` confirms active defaults:
   - `Focus resolve budget (ms): 450`
   - `Focus deep-scan max apps: 1`
2. `stt diag-injector` now logs resolver stage metrics (example terminal case):
   - `focus_resolve_ms=305`
   - `focus_resolve_timed_out=false`
3. 30-run tight loop with `--test-injection` (adaptive paste profile):
   - `min=299ms`, `p50=306.5ms`, `p95=313ms`, `max=315ms`
   - `timeouts=0`
   - no multi-second resolver spikes observed

### 17.4 Remaining caveat

The exact user repro sequence (VS Code -> terminal/Ghostty -> COSMIC Text Editor -> browser) still needs an interactive operator pass in a live desktop focus flow to fully close the loop, but the prior 3s+ resolver outlier class was not reproduced in automated high-frequency validation.

## 18. COSMIC Focus-Signal Pivot Prep (2026-02-19 21:45 UTC)

This section documents follow-up research and runtime evidence for a likely architecture pivot:
- from: per-injection synchronous AT-SPI tree traversal
- to: event-driven Wayland toplevel activation cache (with AT-SPI fallback)

No behavior changes were implemented in this section; this is handoff context only.

### 18.1 High-confidence findings (local runtime + protocol files)

1. COSMIC exposes relevant Wayland globals in this live session.

Evidence command:
```bash
WAYLAND_DEBUG=1 wlr-randr 2>&1 | sed -n '1,120p'
```

Observed globals include:
- `zcosmic_toplevel_info_v1` (v3)
- `ext_foreign_toplevel_list_v1` (v1)
- `zcosmic_toplevel_manager_v1` (v4)
- `zwp_text_input_manager_v3` (v1)
- `zwp_input_method_manager_v2` (v1)
- `cosmic_a11y_manager_v1` (v2)

2. `ext_foreign_toplevel_list_v1` provides identity metadata, not activation state.

Reference:
- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/wayland-protocols-0.32.10/protocols/staging/ext-foreign-toplevel-list/ext-foreign-toplevel-list-v1.xml`
- line refs:
  - `interface name="ext_foreign_toplevel_list_v1"`: line `54`
  - `interface name="ext_foreign_toplevel_handle_v1"`: line `123`
  - `event name="closed"`: line `147`
  - `event name="done"`: line `158`
  - `event name="title"`: line `173`
  - `event name="app_id"`: line `183`
  - `event name="identifier"`: line `193`
- no activated/focused state in this protocol

3. COSMIC toplevel extension adds explicit activation state.

Reference:
- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cosmic-protocols-0.2.0/unstable/cosmic-toplevel-info-unstable-v1.xml`
- line refs:
  - `interface name="zcosmic_toplevel_info_v1"`: line `30`
  - `request name="get_cosmic_toplevel"`: line `81`
  - `event name="done"` (manager): line `94`
  - `interface name="zcosmic_toplevel_handle_v1"`: line `106`
  - enum entry `activated` (`value="2"`): line `215`
  - `event name="state"`: line `220`
- `zcosmic_toplevel_info_v1.done` plus toplevel-handle `state` gives atomic activation snapshots after batch completion

4. COSMIC a11y protocol is not a focus-stream API.

Reference:
- `~/.cargo/registry/src/index.crates.io-1949cf8c6b5b557f/cosmic-protocols-0.2.0/unstable/cosmic-a11y-unstable-v1.xml`
- line refs:
  - `interface name="cosmic_a11y_manager_v1"`: line `30`
  - `event name="magnifier"`: line `35`
  - `request name="set_magnifier"`: line `45`
  - `event name="screen_filter2"`: line `103`
- exposes toggles like magnifier/screen-filter
- no per-target focus event or text-target ownership stream

### 18.2 Why current resolver path is still the wrong long-term primary

Code path:
- `parakeet-ptt/src/injector.rs:914` calls `resolve_with_limits(...)` synchronously during injection.
- `parakeet-ptt/src/injector.rs:936` derives route immediately via `decide_route(...)`.
- `parakeet-ptt/src/routing.rs:52` + `parakeet-ptt/src/routing.rs:67` enforce low-confidence unknown fallback on `focused=false`.

Recent quantified behavior from `/tmp/parakeet-ptt.log` (last 47 resolved snapshots):
- `focused_false=36`
- `route_class=Unknown` count `27`
- `focus_resolve_ms`: min `293`, median `392`, max `451`

Concrete mismatch examples:
- `2026-02-19T21:23:13Z` preview=`Testing in cosmic terminal.` -> resolved `focus_app=\"Brave Browser\"`
- `2026-02-19T21:23:25Z` preview=`Testing in VS Code.` -> resolved `focus_app=\"Brave Browser\"`

Interpretation:
- Even after resolver latency controls, wrong/stale target selection remains a major UX driver.

### 18.3 DeepWiki codemap references (useful, not locally source-verified)

DeepWiki reported likely compositor internals (treat as leads until validated against upstream source):
- `mod.rs:178` `ActiveFocus::get(seat)`
- `mod.rs:399` `ActiveFocus::set(seat, target.cloned())`
- `mod.rs:400` `keyboard.set_focus(state, target.cloned(), serial)`
- `toplevel_info.rs:529` pushes `States::Activated`
- `toplevel_info.rs:625` emits `handle.send_done()`
- `target.rs:707` maps keyboard focus target to `WaylandFocus::wl_surface(...)`

These references strongly align with the external protocol evidence above, but should be verified in actual `cosmic-comp` source before implementation.

### 18.4 Proposed next architecture (design intent, not implemented)

Primary signal:
- Wayland event-driven activation tracking using:
  - `ext_foreign_toplevel_list_v1` (handles + title/app_id/identifier lifecycle)
  - `zcosmic_toplevel_info_v1` (activated state)

Client-side cache:
- Maintain active toplevel snapshot in-memory:
  - `identifier`, `app_id`, `title`, `activated`, `last_state_ts`
- Use cache for immediate adaptive routing decision at injection time.

Fallback hierarchy:
1. fresh activated toplevel from Wayland cache
2. existing AT-SPI resolver (`resolve_with_limits`) when cache unavailable/stale/ambiguous
3. unknown routing fallback behavior as today

Expected benefit:
- remove synchronous AT-SPI scan from hot path in most injections
- reduce stale cross-surface snapshot risk during rapid app switching

### 18.5 Explicit uncertainties (do not silently assume)

1. Activated toplevel may not always equal text-input target.
- Layer-shells/popups/URL bars/IME surfaces can diverge from window-level activation.

2. Transition windows may momentarily report no activated handle.
- Need robust debounce/staleness policy in client cache.

3. Protocol availability is compositor policy.
- Available on this host now, but must handle absence/version downgrade safely.

4. Cross-surface correctness still needs live matrix validation.
- Especially: COSMIC applet editor, COSMIC text editor, VS Code editor pane, Ghostty, Brave/Notion.

### 18.6 Next-agent execution starter checklist

Read first:
- `docs/HANDOFF-stt-cross-surface-injection-2026-02-19.md` (sections 17 and 18)
- `parakeet-ptt/src/injector.rs`
- `parakeet-ptt/src/routing.rs`
- `parakeet-ptt/src/surface_focus.rs`

Reconfirm host protocol surface:
```bash
WAYLAND_DEBUG=1 wlr-randr 2>&1 | rg 'zcosmic_toplevel_info_v1|ext_foreign_toplevel_list_v1|cosmic_a11y_manager_v1'
```

Research before coding:
- validate DeepWiki compositor references directly in upstream `cosmic-comp`
- verify event ordering guarantees around toplevel `state` + `done`
- define cache freshness and fallback criteria with explicit thresholds

Do not remove AT-SPI path initially:
- first implementation should be additive (feature-gated or dual-path),
- then validated against live matrix before default switch.

## 19. Deep Research Distill (Archived From Scratch Notes)

This section captures durable conclusions from the ad-hoc deep research notes and replaces those scratch files.

### 19.1 Protocol truth table

1. `ext_foreign_toplevel_list_v1`:
- Provides toplevel identity/lifecycle metadata (`title`, `app_id`, `identifier`, `done`, `closed`).
- Does **not** carry activated/focused state.

2. `zcosmic_toplevel_info_v1` + `zcosmic_toplevel_handle_v1`:
- `state` carries activation (`activated=2`).
- manager `done` is an atomicity boundary for batched updates.

3. `zwp_text_input_v3` / `zwp_input_method_v2`:
- Most authoritative text-target signal in principle (`enter/leave` + `enable/disable`).
- Not broadly consumable by ordinary third-party clients without operating as IME/input-method role.

4. COSMIC a11y manager protocol:
- Not a per-target focus stream.

### 19.2 Practical architecture conclusions

1. Keep event-driven cache as primary for latency and hot-path safety.
2. Keep AT-SPI as required fallback path while Wayland activation semantics are still imperfect in edge transitions.
3. Avoid synchronous full-tree scans on injection path; prefer event cache + bounded verification.
4. Treat terminal and editor targets as separate policy classes; unknown classification remains high-risk for editor insertion.

### 19.3 Known ecosystem caveats impacting this project

1. Chromium/Electron accessibility/focus signaling can be conditional and inconsistent by launch/runtime context.
2. Ghostty and some terminal-class apps may expose limited accessibility fidelity.
3. Layer-shell and transient focus transitions can create short windows where active toplevel != effective text target.

## 20. Latest Operator Runtime State (2026-02-20)

Observed by operator after latest patches and `hybrid` run:
- COSMIC Terminal/Ghostty: still mostly working.
- Chromium: still working (including scenarios accepting `Ctrl+Shift+V`).
- VS Code editor panel: still broken.
- COSMIC Text Editor: still broken.

Implication:
- Existing route/focus improvements reduced some stale-cache classes but did not close editor-target failures.

Immediate next diagnostic priority:
1. Capture per-injection route lines for failing editor attempts (`focus_source_selected`, `route_class`, `route_primary`, `focus_*`).
2. Determine whether editor failures are:
- still route-choice mismatch (wrong shortcut for effective target), or
- key-delivery acceptance mismatch (shortcut delivered but app rejects synthetic path).

## 21. Root-Cause Closure and Compaction Plan (2026-02-20)

This section captures the validated closure path after the latest operator run and the agreed API compaction strategy.

### 21.1 Evidence from latest run (`/tmp/parakeet-ptt.log.clean`, session start `2026-02-20T13:42:27Z`)

| Surface | Focus resolution | Route decision | Status |
| --- | --- | --- | --- |
| Ghostty | `focus_source_selected="wayland_cache"`, `focus_focused=true` | `route_class=Terminal`, `route_primary=CtrlShiftV` | OK |
| VS Code editor pane | `focus_source_selected="wayland_cache"`, `focus_focused=true` | `route_class=General`, `route_primary=CtrlV` | OK |
| Brave input field | `focus_source_selected="wayland_cache"`, `focus_focused=true` | `route_class=General`, `route_primary=CtrlV` | OK |
| COSMIC Text Editor | `focus_app="com.system76.CosmicEdit"`, `focus_focused=true` | `route_class=Unknown`, `route_primary=CtrlShiftV` | Failing |

### 21.2 Root-cause statement

Remaining failure is a routing classification gap for COSMIC Text Editor identifiers (`com.system76.CosmicEdit` / `cosmicedit` / title forms), not resolver freshness or Wayland cache staleness.

### 21.3 Selected defaults and policy decisions

1. Unknown-route policy remains terminal-first (`Ctrl+Shift+V`).
2. Compatibility strategy is deprecate-now, remove-next-release.
3. Wayland cache defaults are set to:
   - stale threshold: `30000ms`
   - transition grace: `500ms`
4. Primary operator profile is Wayland-first for focus metadata.

### 21.4 Execution checklist

1. Routing fix:
- [x] Add COSMIC Edit hints (`com.system76.cosmicedit`, `cosmicedit`, `cosmic text editor`) to General classification.
- [x] Normalize punctuation separators (`.`, `-`, `_`, `/`) before hint matching.
- [x] Preserve unknown terminal-first behavior.
- [x] Emit explicit route reason for COSMIC Edit matches.

2. Default behavior updates:
- [x] Update Wayland stale default to `30000`.
- [x] Update Wayland transition-grace default to `500`.
- [x] Keep fallback safety behavior intact.

3. Option-surface compaction (Phase 1):
- [x] Keep deprecated flags/env vars parse-compatible but runtime-ignored (robust defaults pinned).
- [x] Move deprecated options out of primary help into compatibility help.
- [x] Emit warnings when deprecated compatibility knobs are explicitly used.
- [x] Keep stable operator knobs in primary help/docs.

### 21.5 Acceptance matrix template (post-change validation)

For each dictation event capture:
- `focus_source_selected`
- `focus_wayland_cache_age_ms`
- `focus_wayland_fallback_reason`
- `focus_app`
- `focus_object`
- `focus_focused`
- `route_class`
- `route_primary`
- `route_adaptive_fallback`
- `route_reason`

Expected matrix:
1. Ghostty -> `Terminal`, `CtrlShiftV`
2. VS Code -> `General`, `CtrlV`
3. COSMIC Text Editor -> `General`, `CtrlV`
4. Brave -> `General`, `CtrlV`
