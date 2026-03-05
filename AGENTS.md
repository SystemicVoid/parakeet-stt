# Repository Guidelines

The role of this file is to describe common mistakes and confusion points that agents might encounter as they work in this project. If you ever encounter something in the project that surprises you, please alert the developer working with you and indicate that this is the case in the AGENTS.md file to help prevent future agents from having the same issue.

## Build, Test, and Development Commands
- Unified quality gate (repo root): `prek install -t pre-commit -t pre-push`, then `prek run --all-files` and `prek run --stage pre-push --all-files`.
- Overlay reliability gate: `just phase6-contract` (single pass), `just phase6-promotion 3` (promotion gate with repeated clean runs + eval compare).
- Eval shortcuts (existing dataset): `just eval compare` (default), `just eval offline`, `just eval stream`, `just eval calibrate-offline`, `just eval calibrate-stream`.

## Runtime Operator Defaults (2026-03)
- `stt` / `stt start`: online stream+seal profile with overlay enabled and adaptive width disabled by default.
- `stt off`: offline profile defaults (no streaming, overlay disabled).
- Overlay mode override remains `PARAKEET_OVERLAY_MODE=auto|layer-shell|fallback-window|disabled`.

## Coding Style & Naming Conventions
- Maintain lazy imports in `model.py` to avoid GPU dependencies for protocol work. Use structured logging (`loguru`) and environment variables prefixed `PARAKEET_` for overrides.
- Naming: binaries remain `parakeet-stt-daemon` and `parakeet-ptt`; config flags match protocol and injector surfaces (`shared_secret`, `endpoint`, `hotkey`, `paste_*`).

## Harness Engineering
- Canonical playbook: `docs/engineering/harness-engineering-playbook.md`.
- Keep `AGENTS.md` short and map-style; operational depth belongs in canonical docs and scripts.
- Python static quality is consolidated on Ruff + ty. Prefer adding Ruff rules before adding overlapping one-off tools.
- Clarification: Ruff `BLE001` is blind exception handling; unreachable duplicate handler checks are `B014` / `B025`.

## STT Helper Flag Policy
- Single source of truth: `scripts/stt-helper.sh` `start_option_rows`.
- Do not hardcode `stt start` flag lists in parser/help/client args/diagnostics; derive behavior from metadata helpers.
- Validation: run `bash -n scripts/stt-helper.sh` and `source scripts/stt-helper.sh && stt help start` after helper flag/default/env changes.
- Multi-binary caveat: this repo has both `parakeet-ptt` and `parakeet-overlay`; any helper fallback using `cargo run` must pass `--bin parakeet-ptt`.
- Runtime override: `PARAKEET_OVERLAY_MODE` supports `auto|layer-shell|fallback-window|disabled` for compositor-specific overlay bring-up.
- Note: `scripts/check-stt-helper-flags.sh` is referenced in older docs/history but is currently not present in this repository.
