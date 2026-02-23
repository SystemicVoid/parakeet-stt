# Parakeet STT Daemon

Canonical usage and commands live in the top-level `README.md`. Use this file only for daemon-specific notes.

- Run the server (matches the shell aliases): `uv run parakeet-stt-daemon --host 127.0.0.1 --port 8765 --no-streaming`
- Install deps: `uv sync --dev` (add `--extra inference --prerelease allow --index https://download.pytorch.org/whl/nightly/cu130 --index-strategy unsafe-best-match` for GPU inference)
- Config overrides use `PARAKEET_` env vars (e.g., `PARAKEET_SHARED_SECRET`, `PARAKEET_MIC_DEVICE`); status endpoint is enabled by default, disable with `--no-status`.
- Dev checks: `uv run ruff check .`, `uv run black --check .`, `uv run --no-project ty check`

For protocol details, see `../docs/SPEC.md`. All other usage docs are maintained at the repository root to avoid drift.
