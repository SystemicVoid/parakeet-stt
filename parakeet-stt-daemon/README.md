# Parakeet STT Daemon

Early skeleton for the local Parakeet push-to-talk daemon. It exposes the
JSON/WebSocket protocol defined in `SPEC.md` and keeps the audio/model plumbing
pluggable for the next iteration.

## Quickstart

```bash
uv sync --dev                # base + dev tooling (ruff, black, pyright)
uv sync --extra inference    # add torch/torchaudio/nemo_toolkit (GPU build)
uv run parakeet-stt-daemon --host 127.0.0.1 --port 8765
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

- The `inference` extra group pins `torch`, `torchaudio`, and
  `nemo_toolkit[asr]`; skip it if you only need protocol testing.
- Package uses a `src/` layout with Hatch build backend.
