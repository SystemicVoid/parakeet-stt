# Parakeet STT Daemon

Early skeleton for the local Parakeet push-to-talk daemon. It exposes the
JSON/WebSocket protocol defined in `../docs/SPEC.md` and keeps the audio/model plumbing
pluggable for the next iteration.

## Quickstart

```bash
uv sync --dev  # base + dev tooling (ruff, black, pyright)
uv sync --extra inference --prerelease allow
uv run --prerelease allow parakeet-stt-daemon --host 127.0.0.1 --port 8765

# Start both pieces from anywhere on this machine
repo="$HOME/Documents/Engineering/parakeet-stt"

# Terminal 1: daemon (writes logs to /tmp/parakeet-daemon.log)
(cd "$repo/parakeet-stt-daemon" && uv run --prerelease allow \
  parakeet-stt-daemon --host 127.0.0.1 --port 8765 \
  > /tmp/parakeet-daemon.log 2>&1)

# Terminal 2: client (Rust)
(cd "$repo/parakeet-ptt" && cargo run --release)
```

- Uses environment variables prefixed with `PARAKEET_` for config overrides
  (e.g. `PARAKEET_SHARED_SECRET`, `PARAKEET_MIC_DEVICE`).
- Optional `/status` endpoint is enabled by default (disable with `--no-status`).
- Current implementation stubs the transcription path until the NeMo pipeline is
  wired in; it still enforces single-session semantics and returns structured
  `session_started`/`final_result`/`error` messages.

## Model usage

The model loader lives in `parakeet_stt_daemon.model` and keeps imports lazy so
protocol work does not require GPU packages:

```python
from parakeet_stt_daemon.model import load_parakeet_model, ParakeetTranscriber

asr_model = load_parakeet_model()  # downloads nvidia/parakeet-tdt-0.6b-v3
transcriber = ParakeetTranscriber(asr_model)
transcriptions = transcriber.transcribe_files(["file.wav"])
```

## Linting / type checking

```bash
uv run ruff check .
uv run black --check .
uv run pyright
```

## Notes

- The `inference` extra group pins nightly `torch` (CUDA 12.4 build) plus
  `nemo_toolkit[asr]`; skip it if you only need protocol testing.
- Package uses a `src/` layout with Hatch build backend.
