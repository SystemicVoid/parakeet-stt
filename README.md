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

### 1. Setup Daemon

Navigate to the daemon directory and install dependencies:

```bash
cd parakeet-stt-daemon
# Install base dependencies + inference support (Torch, NeMo)
uv sync --extra inference --prerelease allow
```

### 2. Run Daemon

Start the daemon (default port 8765):

```bash
# Run with default settings
uv run --prerelease allow parakeet-stt-daemon --host 127.0.0.1 --port 8765

# Or run a health check first
uv run parakeet-stt-daemon --check
```

### 3. Run Client

In a new terminal, build and run the client:

```bash
cd parakeet-ptt
cargo run --release
```

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
