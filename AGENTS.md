# Repository Guidelines

The role of this file is to describe common mistakes and confusion points that agents might encounter as they work in this project. If you ever encounter something in the project that surprises you, please alert the developer working with you and indicate that this is the case in the agent MD file to help prevent future agents from having the same issue.

## Project Structure & Module Organization
- `parakeet-stt-daemon/`: Python 3.11 FastAPI/WebSocket daemon (uv-managed). Core modules live in `src/parakeet_stt_daemon/` (`server.py`, `session.py`, `model.py`, `audio.py`, `config.py`, `messages.py`). `test-run.py` and sample audio support manual model checks. `deploy/` holds systemd units.
- `parakeet-ptt/`: Rust tokio client/hotkey injector. Entry point is `src/main.rs` with helper modules (`client`, `protocol`, `hotkey`, `injector`, `config`, `state`). Builds to the `parakeet-ptt` binary.
- `docs/`: Protocol and migration records. Key docs are `SPEC.md`, `stt-troubleshooting.md`, `HANDOFF-clipboard-injector-2026-02-08.md`, and `STT-INPUT-INJECTION-ROADMAP-2026-02.md`.
- `scripts/stt-helper.sh`: Canonical operator entrypoint (`stt start|stop|status|show|logs|diag-injector`).

## Build, Test, and Development Commands
- Daemon (without GPU stack): `cd parakeet-stt-daemon && uv sync --dev` then `uv run parakeet-stt-daemon --host 127.0.0.1 --port 8765`.
- Daemon (with inference extras): add `--extra inference --prerelease allow --index https://download.pytorch.org/whl/nightly/cu130 --index-strategy unsafe-best-match` to `uv sync`/`uv run` when CUDA is available.
- Daemon lint/type-check: `uv run ruff check .`, `uv run ruff format --check .`, `ty check .`.
- Client: `cd parakeet-ptt && cargo fmt && cargo clippy --all-targets --all-features -- -D warnings && cargo test`. Run locally with `cargo run --release -- --endpoint ws://127.0.0.1:8765/ws`.
- Unified quality gate (repo root): `prek install -t pre-commit -t pre-push`, then `prek run --all-files` and `prek run --stage pre-push --all-files`.
- Helper (recommended runtime): `source scripts/stt-helper.sh && stt start`.

## Coding Style & Naming Conventions
- Python: Ruff enforces linting/formatting with 100-col width; keep modules/functions snake_case; prefer type hints and Pydantic settings objects. Maintain lazy imports in `model.py` to avoid GPU dependencies for protocol work. Use structured logging (`loguru`) and environment variables prefixed `PARAKEET_` for overrides.
- Rust: Let `cargo fmt` drive formatting; address all Clippy warnings. Stick with `anyhow::Result` for fallible flows and `thiserror` for typed errors. Use `tracing` macros for logs; avoid `unwrap`/`expect` in async paths.
- Naming: binaries remain `parakeet-stt-daemon` and `parakeet-ptt`; config flags match protocol and injector surfaces (`shared_secret`, `endpoint`, `hotkey`, `paste_*`).

## Testing Guidelines
- No formal suite yet; new features should add targeted tests (`parakeet-stt-daemon/tests/` or `parakeet-ptt/tests/`) and keep `cargo test`/`uv run pytest` green. For ASR plumbing, `parakeet-stt-daemon/test-run.py` plus the sample WAV provides a smoke check; capture expected transcripts in docs or fixtures.
- Exercise WebSocket protocol changes against `docs/SPEC.md` and keep start/stop/session invariants intact. Prefer fast unit tests over end-to-end GPU runs when validating message flow.
- For injection work, include regression tests for strategy/fallback order in `parakeet-ptt/src/injector.rs` and manually validate with `stt diag-injector` against at least one terminal and one browser field.

## Runtime Defaults (Current)
- `stt start` defaults to reliability-first paste profile:
  - `--injection-mode paste`
  - `--paste-shortcut ctrl-shift-v`
  - `--paste-shortcut-fallback none`
  - `--paste-strategy single`
  - `--paste-key-backend auto` (`uinput -> ydotool -> wtype`)
  - `--paste-routing-mode adaptive`
  - adaptive shortcuts: terminal=`ctrl-shift-v`, general=`ctrl-v`, unknown=`ctrl-shift-v`
  - low-confidence focus snapshots (`focus_focused=false`) route as `unknown` (terminal-first)
  - `--paste-backend-failure-policy copy-only`
- Helper client readiness wait is timeout-based and controlled by `PARAKEET_CLIENT_READY_TIMEOUT_SECONDS` (default `30`).
- Chaining is troubleshooting-only and must be opt-in (`--paste-strategy on-error|always-chain`).

## STT Helper Flag Policy
- Single source of truth: `scripts/stt-helper.sh` `start_option_rows`.
- Do not hardcode `stt start` flag lists in parser/help/client args/diagnostics; derive behavior from metadata helpers.
- After any helper flag/default/env change, run `scripts/check-stt-helper-flags.sh` locally.

## Commit & Pull Request Guidelines
- Commits are short, present-tense summaries (see log: “Default mic to pulse and fix UUID serialization”). Group unrelated changes separately.
- PRs: include a brief description, linked issue (if any), manual/automated test output, and notes on GPU/index flags used. Update `docs/SPEC.md` or systemd units in `deploy/` when protocol/config shape changes, and mention any user-visible CLI flag additions.
