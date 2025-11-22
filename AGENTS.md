# Repository Guidelines

## Project Structure & Module Organization
- `parakeet-stt-daemon/`: Python 3.11 FastAPI/WebSocket daemon (uv-managed). Core modules live in `src/parakeet_stt_daemon/` (`server.py`, `session.py`, `model.py`, `audio.py`, `config.py`, `messages.py`). `test-run.py` and sample audio support manual model checks. `deploy/` holds systemd units.
- `parakeet-ptt/`: Rust tokio client/hotkey injector. Entry point is `src/main.rs` with helper modules (`client`, `protocol`, `hotkey`, `injector`, `config`, `state`). Builds to the `parakeet-ptt` binary.
- `docs/`: Protocol/design references (`SPEC.md`, `gpt-design.md`) to align API and UX decisions.

## Build, Test, and Development Commands
- Daemon (without GPU stack): `cd parakeet-stt-daemon && uv sync --dev` then `uv run parakeet-stt-daemon --host 127.0.0.1 --port 8765`.
- Daemon (with inference extras): add `--extra inference --prerelease allow --index https://download.pytorch.org/whl/nightly/cu130 --index-strategy unsafe-best-match` to `uv sync`/`uv run` when CUDA is available.
- Daemon lint/type-check: `uv run ruff check .`, `uv run black --check .`, `uv run pyright`.
- Client: `cd parakeet-ptt && cargo fmt && cargo clippy --all-targets --all-features -D warnings && cargo test`. Run locally with `cargo run --release -- --endpoint ws://127.0.0.1:8765`.

## Coding Style & Naming Conventions
- Python: Black + Ruff enforce 100-col width; keep modules/functions snake_case; prefer type hints and Pydantic settings objects. Maintain lazy imports in `model.py` to avoid GPU dependencies for protocol work. Use structured logging (`loguru`) and environment variables prefixed `PARAKEET_` for overrides.
- Rust: Let `cargo fmt` drive formatting; address all Clippy warnings. Stick with `anyhow::Result` for fallible flows and `thiserror` for typed errors. Use `tracing` macros for logs; avoid `unwrap`/`expect` in async paths.
- Naming: binaries remain `parakeet-stt-daemon` and `parakeet-ptt`; config flags match the protocol (`shared_secret`, `endpoint`, `hotkey`).

## Testing Guidelines
- No formal suite yet; new features should add targeted tests (`parakeet-stt-daemon/tests/` or `parakeet-ptt/tests/`) and keep `cargo test`/`uv run pytest` green. For ASR plumbing, `parakeet-stt-daemon/test-run.py` plus the sample WAV provides a smoke check; capture expected transcripts in docs or fixtures.
- Exercise WebSocket protocol changes against `docs/SPEC.md` and keep start/stop/session invariants intact. Prefer fast unit tests over end-to-end GPU runs when validating message flow.

## Commit & Pull Request Guidelines
- Commits are short, present-tense summaries (see log: “Default mic to pulse and fix UUID serialization”). Group unrelated changes separately.
- PRs: include a brief description, linked issue (if any), manual/automated test output, and notes on GPU/index flags used. Update `docs/SPEC.md` or systemd units in `deploy/` when protocol/config shape changes, and mention any user-visible CLI flag additions.
