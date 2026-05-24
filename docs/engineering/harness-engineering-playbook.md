# Harness Engineering Playbook

_Last updated: 2026-05-19_

## Purpose

This playbook captures the harness-engineering practices we are enforcing before the next feature sprint.
It is the canonical reference for tool policy, maintenance cadence, and agent-legible repository hygiene.

## Key Insights Applied Here

From "Harness engineering: leveraging Codex in an agent-first world" (OpenAI, 2026-02-11), the actionable points for this repository are:

1. Use repository knowledge as the system of record.
2. Keep top-level instructions as a map, not an encyclopedia.
3. Encode invariants in tools and checks instead of relying on reviewer memory.
4. Optimize for legibility to future agents (discoverable docs, explicit runbooks, deterministic commands).
5. Treat cleanup as continuous garbage collection, not occasional heroic rewrites.

## Repo Mapping

| Harness principle | Implementation in this repo |
| --- | --- |
| Map, not manual | `AGENTS.md` points to focused canonical docs (`README.md`, `docs/SPEC.md`, this playbook). |
| Mechanical invariants | Pre-commit/pre-push hooks in `.pre-commit-config.yaml`; Ruff + ty + Rust gates. |
| Tool consolidation | Ruff is the primary Python static quality engine (dead code + lint surface) with ty for typing. |
| Continuous cleanup loop | Commit-cadence maintenance checks via `scripts/harness-maintenance.sh`. |
| Legible operations | Canonical runbooks in `README.md` and `docs/stt-troubleshooting.md`. |

## Tooling Policy

### Python quality and dead code

- Primary: Ruff (`E,F,I,B,W,UP,BLE`).
- `F841` (unused variables) is enforced through `F`.
- `BLE001` (blind exception) is enforced; intentional broad catches must be explicitly annotated.
- "Unreachable except handler" checks come from `B` rules (`B014`, `B025`), not from `BLE`.

### Python typing

- Standardize on global `ty` invocation:
  - `ty check --project parakeet-stt-daemon --error-on-warning`
- No repo-local pyright config is used.

### Python dependency bloat

- `deptry` is in daemon dev dependencies and treated as the source of truth for dependency declaration drift.
- Direct imports must be direct dependencies (do not rely on transitives).

### Rust dead code and bloat

- Dead code: Clippy in pre-push (`-D warnings`).
- Dependency bloat: `cargo-udeps` in maintenance audits (not on every push).

### Code-shape audit

- `scripts/harness-maintenance.sh code-shape` reports oversized/mixed-responsibility risk surfaces.
- The audit is warn-only: findings guide follow-up issue planning and do not fail the maintenance run.
- Each finding includes file, LOC, category, threshold, and suggested owner/action.
- Categories and current thresholds:
  - `production-rust`: 900 LOC.
  - `production-python`: 700 LOC.
  - `shell`: 800 LOC.
  - `tests`: 700 LOC.
- Excluded paths: `.git/`, virtualenvs such as `.venv/` and `venv/`, `target/`, `node_modules/`, Python/Rust/tool caches such as `__pycache__/`, `.ruff_cache/`, `.pytest_cache/`, `.mypy_cache/`, `.ty/`, `.cache/`, vendor directories, and `docs/archive/`.
- Actions are intentionally architectural rather than mechanical: apply the deletion test, deepen Module/Interface seams, improve Locality and test setup, and avoid arbitrary file splitting.

### Vulture policy

- Vulture is one-off cleanup tooling only (not persistent in hooks).
- 2026-02-27 baseline:
  - `vulture` at `--min-confidence 80`: no findings.
  - Lower-confidence findings were framework-typical false positives and not used as hard gates.

## Maintenance Cadence

- Trigger model: every 10 commits, warn-only (non-blocking).
- Hook reminder: pre-commit runs `scripts/harness-maintenance.sh check --threshold 10`.
- Full audit command:
  - `scripts/harness-maintenance.sh run`
- Code-shape-only report:
  - `scripts/harness-maintenance.sh code-shape`
- State marker:
  - `.git/harness-maintenance.state` (local-only, untracked)

## Command Reference

```bash
# Python
cd parakeet-stt-daemon
uv run ruff check --config pyproject.toml src tests
uv run ruff format --check .
ty check --project .
uv run deptry .

# Rust
cargo clippy --manifest-path parakeet-ptt/Cargo.toml --all-targets --all-features -- -D warnings
cargo +nightly udeps --manifest-path parakeet-ptt/Cargo.toml --all-targets

# Maintenance loop
scripts/harness-maintenance.sh check --threshold 10
scripts/harness-maintenance.sh code-shape
scripts/harness-maintenance.sh run
scripts/harness-maintenance.sh mark

# Release/promotion resource guard
just daemon-resource-soak
```

## Local STT Eval Policy

- Keep eval assets local-first under `parakeet-stt-daemon/bench_audio/personal/` (git-ignored).
- Build prompts from real Codex CLI prompt history by default, then require manual review before recording.
- Daily gate is hybrid when baseline is supplied: absolute floors + relative drift checks.
- Track punctuation explicitly (`punctuation_f1`, `terminal_punctuation_accuracy`) in addition to WER/token metrics.

```bash
cd <repo-root>

# Run-only flow on existing unified corpus (local manifest + legacy transcripts/audio append).
just eval
just eval offline
just eval stream
just eval compare
just eval calibrate-offline
just eval calibrate-stream

# Dataset maintenance (explicit, non-default):
just eval-dataset candidates
# Review parakeet-stt-daemon/bench_audio/personal/candidates.tsv and set include=yes.
just eval-dataset materialize
just eval-dataset record
```
