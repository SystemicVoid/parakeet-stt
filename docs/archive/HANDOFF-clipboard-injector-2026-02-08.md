# Clipboard Injector Handoff (2026-02-08)

## Status of this document

This file is preserved as a historical investigation archive.

- Do not use this file as operational guidance for current flags/defaults.
- Canonical runtime/troubleshooting guidance lives in `docs/stt-troubleshooting.md`.
- Canonical helper flag/default wiring lives in `scripts/stt-helper.sh` (`start_option_rows`).

Still-useful content in this archive:
- Root-cause analysis patterns for paste/injection race conditions.
- Logging and verification strategies that can be reused for future regressions.
- Tradeoff analysis around backend fallback behavior and copy-only safety nets.

## Canonical roadmap

For implementation strategy and phased execution, start with:

- `docs/archive/STT-INPUT-INJECTION-ROADMAP-2026-02.md`

This handoff remains the detailed investigation log and runtime evidence archive.

## Current truth snapshot (2026-02-08, post-uinput MVP hardening)

This section is the canonical quick state for current branch behavior.

### What is now implemented

1. Direct `uinput` paste backend is available (`--paste-key-backend uinput`).
2. `auto` backend now uses runtime fallback ladder:
   - `uinput -> ydotool -> wtype` per shortcut attempt.
3. Paste backend failure policy is explicit:
   - `--paste-backend-failure-policy copy-only|error` (default `copy-only`).
4. Backend init failures in paste mode no longer degrade to noop.
   - `copy-only` policy preserves transcript delivery via clipboard write.
   - `error` policy returns explicit injector failure.
5. Helper lifecycle hardening:
   - daemon reuse/restart decisions now use PID file + socket readiness, not name-only `pgrep`.
6. Helper strictness and diagnostics:
   - `stt start` now fails on unknown options.
   - `stt diag-injector` now reports capability checks (`wtype`, `ydotool`, `/dev/uinput` writeability).

### Verification in latest pass

- `cargo fmt` (client) passed.
- `cargo test` (client) passed: 7 tests.
- `bash -n scripts/stt-helper.sh` passed.
- `cargo clippy --all-targets --all-features -- -D warnings` still fails on pre-existing `clippy::enum_variant_names` in `parakeet-ptt/src/protocol.rs` (unchanged by injector migration work).

### Current operational default recommendation

Use the canonical guidance in `docs/stt-troubleshooting.md`.

Quick baseline (kept in sync with current helper/client surface):

```bash
stt start --paste \
  --paste-key-backend auto \
  --paste-backend-failure-policy copy-only
```

## Update (systematic implementation pass)

This pass implemented the planned robustness changes across Rust injector + helper.

### New behavior now in tree

1. Structured per-injection tracing in `parakeet-ptt/src/injector.rs`.
   - Adds monotonic `trace_id` and outcome classification:
     - `success_assumed`
     - `clipboard_not_ready`
     - `chord_failed`
     - `no_effect_suspected`
     - `copy_only`
   - Logs clipboard fingerprints/lengths and step timings, without dumping full transcript text.

2. Paste strategy engine.
   - New strategy control:
     - `single`
     - `on-error`
     - `always-chain`
   - Shortcut attempts now support semantic chaining with configurable inter-chord delay.
   - Chain can include primary shortcut, configured fallback, and `Ctrl+V` as a final rescue attempt.

3. Post-chord ownership hold and improved race handling.
   - New configurable hold after shortcut(s): `--paste-post-chord-hold-ms`.
   - Foreground `wl-copy` ownership remains alive through chord and hold before transfer/restore.

4. Wayland clipboard controls.
   - Seat-aware clipboard IO (`--paste-seat`).
   - Optional PRIMARY selection mirroring (`--paste-write-primary`).

5. Alternate key backend + copy-only mode.
   - Paste key backend now configurable:
     - `wtype`
     - `ydotool`
     - `uinput`
     - `auto` (runtime chain: `uinput -> ydotool -> wtype`)
   - New injection mode: `copy-only` (writes clipboard, skips key chord).

6. Helper propagation in `scripts/stt-helper.sh`.
   - `stt start` and `stt tmux` now forward all new flags/env defaults.
   - Added `stt diag-injector` matrix command for repeatable `--test-injection` runs.
   - Startup output now prints active strategy/backend/hold settings.

### New CLI surfaces (Rust client)

- `--injection-mode type|paste|copy-only`
- `--ydotool <path>`
- `--paste-strategy single|on-error|always-chain`
- `--paste-chain-delay-ms <ms>`
- `--paste-post-chord-hold-ms <ms>`
- `--paste-key-backend wtype|ydotool|uinput|auto`
- `--paste-backend-failure-policy copy-only|error`
- `--paste-seat <seat>`
- `--paste-write-primary true|false`
- fallback now accepts `ctrl-shift-v` too

### Verification in this pass

- `cargo fmt` (client) passed.
- `cargo test` (client) passed.
- `bash -n scripts/stt-helper.sh` passed.
- `cargo run --release -- --test-injection --injection-mode copy-only` passed with debug trace output.
- `cargo run --release -- --test-injection --injection-mode paste ... --paste-strategy always-chain ...` passed with expected multi-step logs.
- `cargo clippy --all-targets --all-features -- -D warnings` still fails only on pre-existing `clippy::enum_variant_names` in `parakeet-ptt/src/protocol.rs` (unchanged in this pass).

### Operational recommendation right now

Start with:

```bash
stt start --paste \
  --paste-shortcut ctrl-shift-v \
  --paste-shortcut-fallback shift-insert \
  --paste-strategy always-chain \
  --paste-chain-delay-ms 45 \
  --paste-post-chord-hold-ms 700 \
  --paste-restore-policy never \
  --paste-copy-foreground true
```

If key chord acceptance remains inconsistent in a target app:

```bash
stt start --copy-only
```

## Update (deeper pass after user repro: stale first paste, then empty)

### Latest user-reported behavior

- In paste mode, first attempt pasted current/old clipboard content.
- Subsequent attempts pasted nothing.
- Core symptom persisted after prior injector hardening commits.

### New evidence gathered in this pass

1. End-to-end STT still succeeds up to injection dispatch.
   - `/tmp/parakeet-ptt.log` repeatedly shows:
     - `final result received ...`
     - `parakeet_ptt::injector: injecting via clipboard ...`
   - No injector errors/warnings are emitted at `info` level during these runs.

2. Direct injector test flow succeeds in-process with debug logs.
   - Command:
     - `cd parakeet-ptt && RUST_LOG=debug cargo run --release -- --test-injection --injection-mode paste --paste-shortcut ctrl-shift-v --paste-restore-policy never --paste-copy-foreground true --paste-mime-type 'text/plain;charset=utf-8'`
   - Observed flow in logs (all successful):
     - capture existing clipboard
     - `wl-copy --foreground` write
     - readiness check matches expected text in ~12ms
     - `wtype` paste chord exits `0`
     - foreground -> background ownership transfer succeeds
   - Post-run check:
     - `wl-paste --no-newline` returned `Parakeet Test`
     - resident process present: `wl-copy --type text/plain;charset=utf-8`

3. Nu/tmux runtime context verified.
   - Client process env includes:
     - `WAYLAND_DISPLAY=wayland-1`
     - `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus`
     - `DISPLAY=:0`
   - Nu wrapper in `~/.config/forge/nushell/.config/nushell/config.nu` runs:
     - `bash -lc "source .../scripts/stt-helper.sh && stt ..."`
   - Wrapper uses raw string-join argument interpolation (no shell escaping), which is fragile for special chars in args.

4. Helper lifecycle/process detection issues discovered (adjacent reliability risk).
   - `stt restart` can hit:
     - "Daemon already running" (based on `pgrep`) followed by
     - "Waiting for socket... not ready"
   - `stt status` can report daemon not running even after launch attempts.
   - This is likely because daemon matching relies on `[p]arakeet-stt-daemon`, but uv/Python process command lines do not always include that token.
   - This issue is orthogonal to clipboard paste semantics, but it confounds debugging and restart loops.

### Updated hypothesis ranking (after new logs)

1. Target-surface acceptance/timing mismatch for synthetic paste chord (35%)
   - Injection path is invoked and succeeds internally, but focused app behavior diverges.
   - Ghostty virtual-keyboard issues are confirmed upstream for `wtype` typing; paste chord handling may still be inconsistent across apps.

2. Clipboard ownership timing still races app read path (30%)
   - Even with readiness checks, ownership transfer immediately after chord may be too early for some apps.
   - A compositor/app may read clipboard slightly later than expected.

3. Nu/tmux environment/session semantics causing intermittent path differences (15%)
   - tmux environment model and shell-bridging can create stale/inconsistent runtime context across restarts.
   - Not the primary signal right now, but plausible as an intermittent amplifier.

4. Shortcut mismatch by target app/profile (10%)
   - `Ctrl+Shift+V` is terminal-centric; some surfaces expect `Ctrl+V` or `Shift+Insert`.
   - Current fallback only triggers on command failure, not on semantic no-op.

5. Helper daemon detection/restart bug masking real injector tests (10%)
   - If daemon is down or flapping, user perception of "paste failure" can blur with "no transcription event occurred."

### Lateral solution strategies (beyond current implementation)

1. Make paste success criteria explicit and stateful in injector.
   - Add optional `post_chord_hold_ms` (keep foreground source alive N ms after chord).
   - Defer foreground->background transfer until hold elapses.
   - Keep last foreground `wl-copy` child alive between injections (replace on next injection), not per-injection teardown.

2. Treat fallback shortcuts as semantic fallback, not process-exit fallback only.
   - Add optional chained mode:
     - `Ctrl+Shift+V` then `Shift+Insert` then `Ctrl+V` with short delays.
   - Run regardless of primary exit status when enabled.

3. Add configurable seat pinning for wl-clipboard.
   - Expose `--paste-seat` and pass `wl-copy --seat <seat>`, `wl-paste --seat <seat>`.
   - Reduces ambiguity in multi-seat/multi-device setups.

4. Add alternate key injection backend for Ghostty/COSMIC path.
   - Integrate optional `ydotool` backend (uinput-based, not Wayland virtual keyboard protocol).
   - Use as opt-in fallback for environments where `wtype` virtual-keyboard path is unreliable.
   - Upstream reference: <https://github.com/ReimuNotMoe/ydotool>

5. Add "copy-only" degradation mode.
   - On injection event: write clipboard and notify user in logs.
   - Skip synthetic key chord entirely.
   - Useful as deterministic fallback while compositor/app-specific paste injection is tuned.

6. Harden helper daemon health logic (debuggability prerequisite).
   - Replace name-based daemon checks with PID file + socket/status probe.
   - Treat "process exists but socket dead" as unhealthy and restart automatically.
   - This should be fixed independently to prevent false debugging signals.

### Immediate high-signal test matrix to run next

Run all with `RUST_LOG=parakeet_ptt=info,parakeet_ptt::injector=debug` and capture `/tmp/parakeet-ptt.log`:

1. `--paste-shortcut ctrl-shift-v --paste-shortcut-fallback shift-insert --paste-restore-policy never`
2. Same + injected `post_chord_hold_ms` (once implemented): `500`, `1000`
3. `--paste-shortcut shift-insert --paste-shortcut-fallback ctrl-v`
4. `--paste-shortcut ctrl-v --paste-shortcut-fallback shift-insert`
5. Cross-app matrix:
   - Ghostty terminal prompt
   - COSMIC Terminal prompt
   - Brave address bar
   - Native GTK text entry (e.g., settings/search box)

### Key references

- Ghostty discussion tracking `wtype`/virtual keyboard corruption:
  - <https://github.com/ghostty-org/ghostty/discussions/10558>
- `wl-copy`/`wl-paste` options (`--foreground`, `--paste-once`, `--seat`, `--type`):
  - local man/help output (`wl-copy --help`, `wl-paste --help`)
- tmux environment model (`update-environment`, global/session env merge):
  - local `man tmux` ("GLOBAL AND SESSION ENVIRONMENT")

## Update (implemented)

The paste injector path has now been reworked and instrumented in-tree:

- Reliability-first default: `--paste-restore-policy never` (no auto-restore race by default).
- Deterministic clipboard ownership path:
  - `wl-copy --type <mime>` is always used.
  - Optional foreground ownership (`--paste-copy-foreground true`) is held through paste.
  - Ownership can be transferred to background mode after paste to avoid leaked processes.
- Readiness wait before paste chord:
  - Injector now waits briefly for clipboard readback to match requested text before firing `wtype`.
- Optional fallback paste chord:
  - `--paste-shortcut-fallback <none|ctrl-v|shift-insert>` is only attempted when primary chord fails.
- End-to-end helper propagation:
  - `scripts/stt-helper.sh` now forwards all paste-related flags/env vars and validates release-binary support.

Validated locally:

- `cargo test` passes.
- `bash -n scripts/stt-helper.sh` passes.
- `cargo run --release -- --test-injection ...` works with new flags.
- Clipboard semantics are verified:
  - `restore-policy never` leaves transcript in clipboard.
  - `restore-policy delayed` restores original clipboard.

Relevant commits (same day, ordered):

- `de70dff` chore(ptt): add clipboard injector step diagnostics
- `52984ee` fix(ptt): add explicit paste restore policy
- `47e9dbe` fix(ptt): harden wl-copy ownership for paste mode
- `49131b5` fix(ptt): add optional fallback paste chord
- `e005a56` fix(helper): forward full paste injector controls
- `e3903c4` fix(ptt): wait for clipboard readiness before paste

## Historical context (pre-fix investigation)

### Current status
- End-to-end STT pipeline is healthy: hotkey start/stop, daemon transcription, and `final_result` delivery all work.
- Paste injection still behaves incorrectly in user-facing apps:
  - Ghostty and COSMIC Terminal: no visible paste on release.
  - Brave address bar: paste action is triggered, but pastes previous clipboard content instead of transcription.
- This indicates injection is running, but clipboard payload and/or timing/chord behavior is still wrong at the UI boundary.

### Most recent user repro (from shell)
```bash
stt start
>>> Starting Parakeet STT (detached tmux)...
   - Injection mode: paste
   - Paste shortcut: ctrl-shift-v
   - Paste restore delay (ms): 250
   - Launching daemon...
   - Waiting for socket... OK
   - Dictation ready (tmux session: parakeet-stt).
```
Observed by user:
- Hold/release Right Ctrl to dictate: no terminal paste.
- Manual paste afterward still gives old clipboard.
- Brave pastes old clipboard automatically when releasing Right Ctrl.

### What changed in this session
- `parakeet-ptt/src/config.rs`
  - Added `PasteShortcut` enum (`CtrlV`, `CtrlShiftV`, `ShiftInsert`).
  - Added `paste_restore_delay_ms` to config.
  - Introduced `InjectionConfig` to avoid constructor argument explosion.
- `parakeet-ptt/src/main.rs`
  - Added CLI flags:
    - `--paste-shortcut` (default `ctrl-shift-v`)
    - `--paste-restore-delay-ms` (default `250`)
  - Wired these into injector construction and startup logs.
- `parakeet-ptt/src/injector.rs`
  - Removed paste fallback to type injection.
  - Added configurable paste chord sequences via `wtype`:
    - `CtrlV`: `-M ctrl -k v -m ctrl`
    - `CtrlShiftV`: `-M ctrl -M shift -k v -m shift -m ctrl`
    - `ShiftInsert`: `-M shift -k Insert -m shift`
  - Clipboard roundtrip check now warns and continues (informational), instead of abort/fallback.
  - Clipboard restore remains enabled, delay configurable.
  - Attempt to pipe `wl-copy` stderr + `wait_with_output` was tried and reverted because `wl-copy` forking can hang the pipe read.
- `scripts/stt-helper.sh`
  - Added defaults:
    - `PARAKEET_PASTE_SHORTCUT` (default `ctrl-shift-v`)
    - `PARAKEET_PASTE_RESTORE_DELAY_MS` (default `250`)
  - Added start args:
    - `--paste-shortcut <ctrl-v|ctrl-shift-v|shift-insert>`
    - `--paste-restore-delay-ms <ms>`
  - Propagates new flags to client command in both `start` and `tmux` paths.
  - Forces `cargo run --release` if release binary lacks new paste flags.

### Verification run in this session
- `bash -n scripts/stt-helper.sh` passed.
- `cargo test` passed.
- `cargo run -- --help` shows:
  - `--injection-mode`
  - `--paste-shortcut`
  - `--paste-restore-delay-ms`
- Injector self-tests succeeded:
  - `cargo run --release -- --test-injection --injection-mode paste`
  - `... --paste-shortcut ctrl-v`
  - `... --paste-shortcut shift-insert`
- `stt start --paste` / `stt stop` smoke runs pass.
- `cargo clippy --all-targets --all-features -- -D warnings` still fails on pre-existing `clippy::enum_variant_names` in `parakeet-ptt/src/protocol.rs` (unrelated to injector path).

### Runtime evidence from logs
- `/tmp/parakeet-ptt.log` (latest session around `2026-02-08T16:21:42Z`):
  - Client runs with `paste_shortcut=CtrlShiftV restore_delay_ms=250`.
  - Multiple successful STT cycles:
    - `start_session sent`
    - `final result received`
    - `injecting via clipboard ... shortcut=CtrlShiftV ...`
  - No injector warnings/errors during those cycles.
- `/tmp/parakeet-daemon.log`:
  - Matching session IDs complete with normal latency and no server errors.

### Ranked hypotheses (probability, current confidence)
1. Clipboard restore race (45%)
   - Restore after 250ms may happen before target app reads clipboard for paste.
   - Matches symptom: Brave triggers paste but receives old clipboard.
2. App-specific paste chord mismatch/handling (20%)
   - Terminals often expect `Ctrl+Shift+V` or `Shift+Insert`, but behavior differs by app/settings.
   - Could explain "works in one app, not another."
3. Synthetic key acceptance difference across apps (15%)
   - Some apps may treat virtual-keyboard-originated key chords differently.
   - Ghostty already has known virtual-keyboard issues for typed text.
4. Clipboard manager interference (10%)
   - External clipboard manager may preserve/restore selection aggressively.
   - Could mask clipboard writes during fast set->paste->restore windows.
5. `wl-copy` availability timing (5%)
   - `wl-copy` background serving may not be ready by the time paste key is sent.
   - Roundtrip warning would usually catch this when strict, but now mismatch is non-fatal.
6. Session/display/seat mismatch edge case (5%)
   - tmux environment inheritance can be tricky; less likely here because process env includes valid Wayland vars and sessions run.

### Why this is still unresolved
- Current logs only record injector start (`injecting via clipboard`), not per-step outcomes:
  - clipboard before/after lengths/previews
  - chord execution status with timing
  - restore start/finish timing
- Without per-step logs, we cannot distinguish race vs chord-vs-app behavior from evidence alone.

### Recommended next debugging strategy (ordered)
1. Add high-signal injector debug logs (step timestamps)
   - Log:
     - original clipboard capture status/length
     - post-`wl-copy` roundtrip status/length
     - paste chord command + exit status
     - restore start/end + delay used
   - Keep logs at `debug` to avoid user noise.
2. Add a "no restore" mode and test first
   - Add flag/env: `--paste-restore-delay-ms -1` or `--no-restore`.
   - If transcription starts pasting correctly, race is confirmed.
3. Increase restore delay for confirmation
   - Try `PARAKEET_PASTE_RESTORE_DELAY_MS=1500` then `3000`.
   - If this fixes Brave/terminal behavior, prioritize delayed/optional restore.
4. Add optional multi-chord paste strategy
   - Configurable fallback sequence:
     - `ctrl-shift-v` then `shift-insert` after short sleep.
   - Keep disabled by default; use only for targeted app incompatibility.
5. Explicit seat/type options for clipboard tools
   - Consider `wl-copy --seat seat0 --type text/plain;charset=utf-8`.
   - Useful in multi-seat or MIME edge cases.
6. Consider `wl-copy --foreground` experiment
   - Could reduce race by ensuring data source is alive and deterministic through paste.
   - Needs careful process lifecycle handling.

### Missing information to collect next
- App-specific behavior matrix under identical runs:
  - Ghostty, COSMIC Terminal, Brave URL bar, plain text editor.
- Whether paste works with restore disabled.
- Whether paste works with long restore delay (`1500ms+`).
- Whether manual one-liner works from same tmux-managed client environment:
  - `wl-copy 'ZZZ' && wtype -M ctrl -M shift -k v -m shift -m ctrl`
- Whether a clipboard manager is running and rewriting clipboard.
- Exact terminal keybinds in use (customized vs defaults).

### External docs/issues to consult while debugging
- Ghostty discussion (current state: unanswered, needs-confirmation):
  - <https://github.com/ghostty-org/ghostty/discussions/10558>
- `wtype` man page (virtual-keyboard protocol, modifier semantics):
  - <https://manpages.ubuntu.com/manpages/jammy/man1/wtype.1.html>
- `wl-clipboard` man page (`--foreground`, `--paste-once`, seat/type gotchas):
  - <https://manpages.ubuntu.com/manpages/jammy/man1/wl-copy.1.html>
- tmux environment behavior (`update-environment`, server/session env model):
  - <https://manpages.ubuntu.com/manpages/focal/man1/tmux.1.html>

## Update (latest runtime evidence: 2026-02-08 18:19-18:30 UTC)

### User command matrix that was tested

```bash
stt start --paste \
  --paste-shortcut ctrl-shift-v \
  --paste-shortcut-fallback shift-insert \
  --paste-strategy always-chain \
  --paste-chain-delay-ms 45 \
  --paste-post-chord-hold-ms 700 \
  --paste-restore-policy never \
  --paste-copy-foreground true

stt start --copy-only

stt start
```

### Latest user-visible behavior

- `copy-only` starts and processes STT normally.
- In paste mode, injection is triggered but transcript does not get inserted in-place.
- Progress vs earlier state: key-chord injection now fires consistently (paste action observable).
- Web input surfaces show a focus side effect: after trigger, input often needs a click to regain text entry focus.
- That focus side effect is not seen in terminal prompts or COSMIC Text Editor.

### What logs now prove (and what they still do not prove)

1. STT pipeline is healthy end-to-end.
   - `/tmp/parakeet-daemon.log`: start/stop/final_result flows are clean during 18:20-18:30.
   - `/tmp/parakeet-ptt.log`: matching `final result received` lines for each session.

2. Clipboard injector executes all intended steps at process level.
   - Example timeline from `/tmp/parakeet-ptt.log`:
     - `2026-02-08T18:20:08.558630Z` `starting clipboard injection` (`trace_id=1`)
     - `paste shortcut executed` for `CtrlShiftV`, `ShiftInsert`, `CtrlV`
     - `clipboard injection flow finished ... outcome="success_assumed"`
   - Same pattern repeats for `trace_id` 1..4 in the 18:19-18:30 window.

3. No warnings/errors are emitted in this latest window.
   - No `WARN`/`ERROR` from injector during those injections.
   - Outcomes observed in current instrumentation:
     - `success_assumed`: 8
     - `copy_only`: 2

4. Gap: current success criterion remains process-level, not UI-level.
   - `success_assumed` means command execution looked good.
   - It does not prove focused app inserted transcript text.

### Updated interpretation

- This is no longer primarily a "did injector run?" problem; it clearly runs.
- Current blocker is likely at the app/compositor interaction boundary:
  - synthetic chord acceptance in specific surfaces
  - focus churn caused by chained shortcuts
  - semantic mismatch between process success and UI insertion success
- `always-chain` likely increases side effects in web inputs by sending extra chords after the first one.

### Updated hypothesis ranking

1. Chord chaining side effects on focus/target surface (35%)
   - `always-chain` sends 2-3 shortcuts every time.
   - This can cause unintended UI state transitions in browser fields.

2. Surface-specific key acceptance of virtual keyboard events (25%)
   - Process-level success does not guarantee app-level acceptance.
   - Ghostty already has known virtual-keyboard problems for `wtype`.

3. Clipboard ownership/read timing vs app paste-read timing (20%)
   - `copy_foreground=true` + hold helped reliability, but timing could still diverge by app.

4. Nu wrapper argument shelling/path semantics causing subtle mode mismatch (10%)
   - Current `nu` bridge builds a single shell string via `str join ' '`.
   - This is fragile if arguments ever include shell-sensitive content.

5. Seat/session context mismatch edge case (10%)
   - Client env is stable (`WAYLAND_DISPLAY=wayland-1`, DBus set), but `XDG_SESSION_TYPE=tty`.
   - Worth validating with explicit seat pinning (`--paste-seat`) in experiments.

### Out-of-the-box strategies to investigate next

1. Replace `always-chain` with per-surface profiles.
   - Browser/text fields: `single` + `ctrl-v`.
   - Terminals: `single` + `ctrl-shift-v` or `shift-insert`.
   - Only chain when explicitly debugging.

2. Add a UI-fidelity backend path (independent of `wtype`).
   - Test `--paste-key-backend ydotool` systematically.
   - Evaluate libei + portal-based input injection as a longer-term backend.

3. Promote copy-first workflow as a deterministic fallback.
   - Keep `copy-only` as "known good" path.
   - Optionally add explicit user feedback (notification or log marker) when clipboard updated.

4. Stop doing semantic fallback by default in one pass.
   - A/B run `single` mode first to remove confounding side effects.
   - Add stateful retries only after single-mode failure is confirmed.

5. Harden the nu wrapper invocation.
   - Use argv-style external invocation in nu instead of joining into one shell string.
   - Eliminates shell-escaping ambiguity from command assembly.

### Additional context discovered (possible confounders)

- Package timeline indicates multiple moving parts around this period:
  - Ghostty 1.2.3 installed on 2026-01-25 (`apt-get install ... ghostty_1.2.3-0.ppa1...`).
  - COSMIC components and compositor were upgraded repeatedly, including 2026-02-07.
- So "breakage after nu migration" may be real, but co-timed app/compositor upgrades remain plausible contributors.
