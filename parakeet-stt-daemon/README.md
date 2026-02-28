# Parakeet STT Daemon

Canonical usage and commands live in the top-level `README.md`. Use this file only for daemon-specific notes.

- Run the server (matches helper defaults): `PARAKEET_STREAMING_ENABLED=false uv run parakeet-stt-daemon --host 127.0.0.1 --port 8765`
- Install deps: `uv sync --dev` (add `--extra inference --prerelease allow --index https://download.pytorch.org/whl/nightly/cu130 --index-strategy unsafe-best-match` for GPU inference)
- Offline benchmark harness (legacy transcripts mode): `uv run python check_model.py --bench-offline --bench-output bench_audio/latest-benchmark.json --max-avg-wer 0.45 --max-p95-infer-ms 1800 --max-p95-finalize-ms 2200`
- Personal eval tooling (local-only corpus): use repo-root `just eval` for run/compare/calibrate on the existing unified corpus (`just eval`, `just eval offline`, `just eval stream`, `just eval compare`, `just eval calibrate-offline`, `just eval calibrate-stream`). Use `just eval-dataset ...` only when intentionally refreshing prompts/audio. Recorder defaults remain `bench_audio/personal/manifest.jsonl` -> `bench_audio/personal/audio` with key controls `s`, `r`, `n/p`, `q`.
- Config overrides use `PARAKEET_` env vars (e.g., `PARAKEET_SHARED_SECRET`, `PARAKEET_MIC_DEVICE`); status endpoint is enabled by default, disable with `--no-status`.
- Dev checks: `uv run ruff check .`, `uv run ruff format --check .`, `ty check --project . --error-on-warning`, `uv run deptry .`
- Commit/push gates are managed from repo root via `prek` (`.pre-commit-config.yaml`).

For protocol details, see `../docs/SPEC.md`. All other usage docs are maintained at the repository root to avoid drift.
