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
