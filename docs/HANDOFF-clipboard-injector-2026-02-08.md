# Clipboard Injector Handoff (2026-02-08)

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
