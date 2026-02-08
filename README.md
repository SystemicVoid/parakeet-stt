# Parakeet STT

Parakeet STT is a local, low-latency speech-to-text system designed for Linux/Wayland environments. It consists of a Python daemon handling audio processing and inference (using NVIDIA NeMo) and a Rust client for push-to-talk interaction.

## Components

- **[parakeet-stt-daemon](./parakeet-stt-daemon)**: The core server that captures audio (optional) and performs speech recognition. It exposes a WebSocket API.
- **[parakeet-ptt](./parakeet-ptt)**: A lightweight Rust client that listens for global hotkeys (via `evdev`) and streams audio/control commands to the daemon.
- **[docs](./docs)**: Design documents and protocol specifications.

## Prerequisites

- **Python 3.11+** (managed via `uv`)
- **Rust** (managed via `cargo`)
- **NVIDIA GPU** (Recommended for low-latency inference)
- **Linux** (Client uses `evdev` and `nix` for input handling)

## Quick Start

1) Install daemon dependencies (CPU-friendly default)
```bash
cd parakeet-stt-daemon
uv sync --dev
```
   - GPU inference (optional): `uv sync --dev --extra inference --prerelease allow --index https://download.pytorch.org/whl/nightly/cu130 --index-strategy unsafe-best-match`

2) Run with the tmux-based helper (recommended)
```bash
source scripts/stt-helper.sh   # one time per shell
stt start                      # starts daemon + client, detaches tmux
stt start --paste              # paste-mode injection (recommended for Ghostty)
```
   - Attach to the panes: `stt show` (top: client via `tee`; bottom: live daemon/client logs).
   - Stop everything: `stt stop`. Status: `stt status`. Logs: `stt logs [client|daemon|both]`.

   The helper:
   - Starts the daemon with `--no-streaming`, binds to `PARAKEET_HOST:PARAKEET_PORT` (default `127.0.0.1:8765`), and waits for the socket; logs land in `/tmp/parakeet-daemon.log`. If 8765 is occupied, it picks the next free port unless you pin `PARAKEET_PORT`.
   - Launches the client in a detached tmux session `parakeet-stt`, teeing output to `/tmp/parakeet-ptt.log`, and always targets the resolved endpoint.
   - Uses `target/release/parakeet-ptt` when available; otherwise falls back to `cargo run --release -- --endpoint <endpoint>`.

3) Manual start (if you prefer two terminals)
```bash
# Terminal A
cd parakeet-stt-daemon
uv run parakeet-stt-daemon --no-streaming

# Terminal B
cd parakeet-ptt
cargo run --release -- --endpoint ws://127.0.0.1:8765/ws
```
   - Health check only: `uv run parakeet-stt-daemon --check`.
   - Hotkey defaults to Right Ctrl.

## Key Commands

### Daemon (`parakeet-stt-daemon`)

| Command | Description |
|---------|-------------|
| `uv run parakeet-stt-daemon --check` | Run startup checks (audio, model, config) and exit. |
| `uv run parakeet-stt-daemon --no-streaming` | Disable streaming and use offline transcription only. |
| `uv run parakeet-stt-daemon --mic-device <ID>` | Select a specific microphone device ID. |

See `uv run parakeet-stt-daemon --help` for all options.

### Client (`parakeet-ptt`)

| Command | Description |
|---------|-------------|
| `cargo run --release` | Run the client in release mode. |
| `cargo run -- --help` | Show client configuration options. |

### Helper Script (`scripts/stt-helper.sh`)
- Source it in your shell to get `stt start|stop|status|logs|show|tmux|check`.
- Detached tmux session: `stt start` spins up the daemon + client, tails logs in a second pane, and returns you to your shell.
- Paste-mode controls are exposed through `stt start`:
  - `--paste-shortcut <ctrl-v|ctrl-shift-v|shift-insert>`
  - `--paste-shortcut-fallback <none|ctrl-v|shift-insert>`
  - `--paste-restore-policy <never|delayed>` (default `never`)
  - `--paste-restore-delay-ms <ms>`
  - `--paste-copy-foreground <true|false>`
  - `--paste-mime-type <mime>`
- Logs live in `/tmp/parakeet-daemon.log` and `/tmp/parakeet-ptt.log`; `stt show` attaches to the tmux layout.
- Keep your personal shell config private: only this helper is intended for sharing. You can copy the function into your dotfiles or re-source the script when needed.

## Documentation

- [Protocol Specification](docs/SPEC.md): Details the WebSocket protocol between client and daemon.
- [Design Notes](docs/Designing%20a%20Local%20Live%20Speech-to-Text%20Dictation%20Solution%20(Wayland,%20Parakeet%20ASR).pdf): Background on the system design.
- Streaming experiments are tracked on the `streaming-experimental-20251122` branch. (streaming rnnt inference : https://github.com/NVIDIA-NeMo/NeMo/blob/main/examples/asr/asr_chunked_inference/rnnt/speech_to_text_streaming_infer_rnnt.py)
- Troubleshooting the `stt` helper: see [docs/stt-troubleshooting.md](docs/stt-troubleshooting.md).
