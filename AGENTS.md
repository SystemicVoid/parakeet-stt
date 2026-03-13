# Repository Guidelines

Keep it short, stable, and biased toward source-of-truth locations. If something in the repo surprises an agent, note it here but prefer fixing the code, tests, CLI help, or canonical docs so the behavior becomes predictable. Only keep workflow rules here that an agent needs before acting.

## Repo Map
- `README.md`: quickstart and top-level workflow.
- `docs/SPEC.md`: product/runtime intent and behavior.
- `docs/stt-troubleshooting.md`: operator-facing runtime behavior and troubleshooting.
- `docs/engineering/harness-engineering-playbook.md`: tooling policy, repo hygiene, and maintenance cadence.
- `scripts/stt-helper.sh` `start_option_rows`: canonical source for helper start flags, defaults, and env wiring.

## Predictability Rules
- Do not turn `AGENTS.md` into an accumulating list of caveats. Move stable operational detail into the canonical doc or encode it in the tool itself.
- Keep one source of truth per surface. Do not hardcode `stt start` flags/defaults in parser/help/client args/diagnostics/docs when they can be derived from helper metadata.
- Prefer mechanical checks over memory. If a rule matters, back it with validation, tests, or generated help text.
- Keep machine-local behavior in ignored repo-local files `.parakeet-stt.local.env` and `.parakeet-stt.local.sh`, not tracked config.

## Build, Test, and Development Commands
- Unified quality gate (repo root): `prek install -t pre-commit -t pre-push`, then `prek run --all-files` and `prek run --stage pre-push --all-files`.
- Overlay reliability gate: `just phase6-contract` (single pass), `just phase6-promotion 3` (promotion gate with repeated clean runs + eval compare).
- Eval shortcuts (existing dataset): `just eval compare` (default), `just eval offline`, `just eval stream`, `just eval calibrate-offline`, `just eval calibrate-stream`.
- Python package-scope caveat: run daemon Python commands from `parakeet-stt-daemon/` so the package environment and imports resolve correctly.
- Commit-hook behavior: if pre-commit rewrites staged files, re-stage them and retry the commit.

## Runtime Operator Defaults (2026-03)
- `stt` / `stt start`: online stream+seal profile with overlay enabled by default. Exact helper defaults belong in `scripts/stt-helper.sh` and `docs/stt-troubleshooting.md`, not here.
- `stt off`: offline profile defaults (no streaming, overlay disabled).

## Harness Engineering
- Canonical playbook: `docs/engineering/harness-engineering-playbook.md`.
- Python static quality is consolidated on Ruff + ty. Prefer adding Ruff rules before adding overlapping one-off tools.
- Clarification: Ruff `BLE001` is blind exception handling; unreachable duplicate handler checks are `B014` / `B025`.

## STT Helper Flag Policy
- Do not hardcode `stt start` flag lists in parser/help/client args/diagnostics; derive behavior from metadata helpers.
- Validation: run `bash -n scripts/stt-helper.sh`, `source scripts/stt-helper.sh && stt help start`, and `source scripts/stt-helper.sh && stt help llm` after helper flag/default/env changes.
- Process-model contract: helper lifecycle checks must refresh `/tmp/parakeet-daemon.pid` from the bound port after startup/status probes instead of trusting the initial `uv run` launcher PID.
- Multi-binary contract: any helper fallback using `cargo run` must pass `--bin parakeet-ptt`.
