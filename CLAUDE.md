# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Parakeet STT is a local, low-latency speech-to-text system for Linux/Wayland. It uses NVIDIA NeMo Parakeet for inference and consists of two main components that communicate via WebSocket.

## Architecture

```
parakeet-ptt (Rust)              parakeet-stt-daemon (Python)
├── evdev hotkey (Right Ctrl)    ├── FastAPI WebSocket server
├── WebSocket client       ────► ├── Audio capture (sounddevice)
├── Text injection (wtype)  ◄──── ├── NeMo Parakeet inference
└── State machine                └── Session management
```

**Control flow**: User presses Right Ctrl → client sends `start_session` → daemon captures audio → user releases → client sends `stop_session` → daemon transcribes → returns `final_result` → client injects text via wtype.

## Build Commands

### Daemon (Python, uv-managed)
```bash
cd parakeet-stt-daemon
uv sync --dev                    # CPU-only dependencies
uv sync --dev --extra inference --prerelease allow \
  --index https://download.pytorch.org/whl/nightly/cu130 \
  --index-strategy unsafe-best-match  # With GPU inference

uv run parakeet-stt-daemon --check           # Health check
uv run parakeet-stt-daemon --no-streaming    # Run server (offline mode)
uv run parakeet-stt-daemon --host 127.0.0.1 --port 8765
```

### Client (Rust)
```bash
cd parakeet-ptt
cargo build --release
cargo run --release -- --endpoint ws://127.0.0.1:8765/ws
cargo run -- --help              # Show all options
```

### Linting/Formatting
```bash
# Daemon
cd parakeet-stt-daemon
uv run ruff check .
uv run black --check .
uv run pyright

# Client
cd parakeet-ptt
cargo fmt
cargo clippy --all-targets --all-features -D warnings
cargo test
```

### Helper Script (recommended workflow)
```bash
source scripts/stt-helper.sh     # Source once per shell
stt start                        # Start daemon + client in tmux
stt start --paste                # Use clipboard injection mode
stt stop                         # Stop everything
stt status                       # Check running processes
stt show                         # Attach to tmux session
stt logs [client|daemon|both]    # Tail log files
stt check                        # Run daemon health check
```

Logs: `/tmp/parakeet-daemon.log`, `/tmp/parakeet-ptt.log`

## Key Source Files

### Daemon (`parakeet-stt-daemon/src/parakeet_stt_daemon/`)
- `server.py` - FastAPI app, WebSocket handler, `DaemonServer` class
- `model.py` - NeMo model loading, `ParakeetTranscriber`, `ParakeetStreamingTranscriber`
- `audio.py` - Sounddevice audio capture with ring buffer
- `session.py` - Session state machine and manager
- `messages.py` - Pydantic models for WebSocket protocol
- `config.py` - `ServerSettings` with env var overrides (`PARAKEET_*`)

### Client (`parakeet-ptt/src/`)
- `main.rs` - CLI entry, hotkey loop, WebSocket message handling
- `hotkey.rs` - evdev Right Ctrl detection
- `injector.rs` - `WtypeInjector`, `ClipboardInjector` implementations
- `client.rs` - WebSocket connection wrapper
- `protocol.rs` - Message types matching daemon protocol
- `state.rs` - `PttState` (Idle → Listening → WaitingResult)

## WebSocket Protocol

Messages are JSON with a `type` field. Key message types:
- Client → Server: `start_session`, `stop_session`, `abort_session`
- Server → Client: `session_started`, `final_result`, `error`, `status`

See `docs/SPEC.md` for complete protocol specification.

## Coding Conventions

### Python
- Black + Ruff, 100-char line width
- Type hints everywhere, Pydantic for settings/messages
- Lazy imports in `model.py` to avoid GPU deps when not needed
- Structured logging via `loguru`
- Environment variables prefixed `PARAKEET_`

### Rust
- `cargo fmt` formatting, address all Clippy warnings
- `anyhow::Result` for fallible operations, `thiserror` for typed errors
- `tracing` macros for logs
- Avoid `unwrap`/`expect` in async code paths

## Environment Variables

- `PARAKEET_HOST` / `PARAKEET_PORT` - Daemon bind address (default: 127.0.0.1:8765)
- `PARAKEET_ROOT` - Override repo root for helper script
- `PARAKEET_INJECTION_MODE` - Default injection mode (type/paste)
- `PARAKEET_SILENCE_FLOOR_DB` - Silence trim threshold
- `RUST_LOG` - Rust logging level (default: info)

## Testing

No formal test suite yet. Manual verification:
- `parakeet-stt-daemon/test-run.py` + sample WAV for model smoke test
- `cargo run -- --test-injection` to verify wtype works
- `cargo run -- --demo` for single start/stop cycle

## Clipboard Injector Handoff (2026-02-08)

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
