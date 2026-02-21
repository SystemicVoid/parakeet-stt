# Parakeet STT

Parakeet STT is a local, low-latency speech-to-text stack for Linux/Wayland.
It has two runtime components:

- `parakeet-stt-daemon` (Python/FastAPI): captures audio and runs NeMo Parakeet ASR.
- `parakeet-ptt` (Rust): global hotkey client, daemon WebSocket client, and text injector.

## Current State (Feb 2026)

Since `21d8f74` and follow-up commits, the injection path is now reliability-first:

- Runtime injection surface is now `paste` or `copy-only` (legacy `type` mode removed).
- Default routing mode is adaptive, selecting shortcut by focused surface class.
- Default backend is `auto` with runtime ladder `uinput → ydotool`.
- Backend failures default to `copy-only` so transcript delivery is preserved in clipboard.
- Clipboard readiness barrier and post-chord ownership timing controls are implemented.
- `stt diag-injector` reports capability prechecks and runs reproducible injection tests.

This keeps the system usable now while uinput behavior is hardened across app surfaces.

## Prerequisites

- Python 3.11+ (`uv`)
- Rust toolchain (`cargo`)
- Linux (Wayland/X11 compatible input stack)
- NVIDIA GPU optional for lower latency; CPU path works for development/testing

## Quick Start

1. Install daemon dependencies:
```bash
cd parakeet-stt-daemon
uv sync --dev
```
Optional CUDA/nightly inference extras:
```bash
uv sync --dev --extra inference --prerelease allow \
  --index https://download.pytorch.org/whl/nightly/cu130 \
  --index-strategy unsafe-best-match
```

2. Start via helper (recommended):
```bash
source scripts/stt-helper.sh
stt start
```

3. Inspect runtime:
```bash
stt status
stt show
stt logs both
```

4. Stop:
```bash
stt stop
```

Manual two-terminal start is still supported:
```bash
# Terminal A
cd parakeet-stt-daemon
uv run parakeet-stt-daemon --no-streaming

# Terminal B
cd parakeet-ptt
cargo run --release -- --endpoint ws://127.0.0.1:8765/ws
```

## Helper Defaults (`stt start`)

Default profile:

- `--injection-mode paste`
- `--paste-key-backend auto` (ladder: uinput → ydotool)
- `--paste-backend-failure-policy copy-only`
- `--uinput-dwell-ms 18`
- Adaptive routing: Terminal → Ctrl+Shift+V, General → Ctrl+V, Unknown → Ctrl+Shift+V
- Wayland focus cache: 30s stale threshold, 500ms transition grace
- Clipboard: foreground wl-copy, 700ms post-chord hold, `text/plain;charset=utf-8`

Helper readiness timing:

- `PARAKEET_CLIENT_READY_TIMEOUT_SECONDS` controls client readiness wait (default `30`)
- helper extends readiness wait when `cargo run --release` compile activity is detected

COSMIC focus-navigation baseline for best adaptive behavior:
- `Focus follows cursor = ON`
- `Focus follows cursor delay = 0ms`
- `Cursor follows focus = ON`

Primary helper commands:

- `stt start|restart|stop|status`
- `stt show` (attach tmux)
- `stt logs [client|daemon|both]`
- `stt check` (daemon health)
- `stt diag-injector` (injection diagnostics)
- `stt help` and `stt help start` (full helper + start flag reference)

`stt start` flag parsing/help/runtime args are driven by a single metadata table in
`scripts/stt-helper.sh` (`start_option_rows`).

## Testing and Validation

Client checks:
```bash
cd parakeet-ptt
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Daemon checks:
```bash
cd parakeet-stt-daemon
uv run ruff check .
uv run black --check .
uv run pyright
uv run parakeet-stt-daemon --check
```

Manual injector validation:
```bash
stt diag-injector
```

## Docs Map

- Protocol contract: `docs/SPEC.md`
- Troubleshooting (canonical operator source): `docs/stt-troubleshooting.md`
- Historical injector handoff archive (non-canonical): `docs/HANDOFF-clipboard-injector-2026-02-08.md`
- Injection implementation roadmap: `docs/STT-INPUT-INJECTION-ROADMAP-2026-02.md`
- UX roadmap (new): `ROADMAP.md`
