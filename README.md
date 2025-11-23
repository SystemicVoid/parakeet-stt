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

2) Start the daemon (non-streaming)
```bash
uv run parakeet-stt-daemon --no-streaming
```
   - Health check only: `uv run parakeet-stt-daemon --check`

3) Start the client
```bash
cd parakeet-ptt
cargo run
```
   - Hotkey defaults to Right Ctrl. Update your shell aliases (`stt`, `stt start`, etc.) to match the daemon command above.

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

## Documentation

- [Protocol Specification](docs/SPEC.md): Details the WebSocket protocol between client and daemon.
- [Design Notes](docs/Designing%20a%20Local%20Live%20Speech-to-Text%20Dictation%20Solution%20(Wayland,%20Parakeet%20ASR).pdf): Background on the system design.
- Streaming experiments are tracked on the `streaming-experimental-20251122` branch. (streaming rnnt inference : https://github.com/NVIDIA-NeMo/NeMo/blob/main/examples/asr/asr_chunked_inference/rnnt/speech_to_text_streaming_infer_rnnt.py)
- Troubleshooting the `stt` helper: see [docs/stt-troubleshooting.md](docs/stt-troubleshooting.md).
