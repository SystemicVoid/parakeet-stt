# Parakeet STT

Parakeet STT is a local, low-latency speech-to-text stack for Linux/Wayland.
It has two runtime components:

- `parakeet-stt-daemon` (Python/FastAPI): captures audio and runs NeMo Parakeet ASR.
- `parakeet-ptt` (Rust): global hotkey client, daemon WebSocket client, and text injector.

## Current State (Feb 2026)

Since `21d8f74` and follow-up commits, the injection path is now reliability-first:

- Runtime injection surface is now `paste` or `copy-only` (legacy `type` mode removed).
- Default routing mode is adaptive, selecting shortcut by focused surface class.
- Default backend is `auto` with runtime ladder `uinput → ydotool`.
- Backend failures default to `copy-only` so transcript delivery is preserved in clipboard.
- Backend stage failure accounting includes `ydotool` spawn failures (missing/non-executable binary), not just non-zero exit statuses.
- Clipboard readiness barrier and post-chord ownership timing controls are implemented.
- `stt diag-injector` reports capability prechecks and runs reproducible injection tests.
- Event-loop lag summaries are derived from Tokio tick scheduling (not a drifting baseline), so percentile windows recover after transient stalls.

This keeps the system usable now while uinput behavior is hardened across app surfaces.

## Prerequisites

- Python 3.11+ (`uv`)
- Rust toolchain (`cargo`)
- Linux (Wayland/X11 compatible input stack)
- NVIDIA GPU optional for lower latency; CPU path works for development/testing

## Quick Start

1. Install daemon dependencies:
```bash
cd parakeet-stt-daemon
uv sync --dev
```
Optional CUDA/nightly inference extras:
```bash
uv sync --dev --extra inference --prerelease allow \
  --index https://download.pytorch.org/whl/nightly/cu130 \
  --index-strategy unsafe-best-match
```

2. Start via helper (recommended):
```bash
source scripts/stt-helper.sh
stt start
```

3. Inspect runtime:
```bash
stt status
stt show
stt logs both
```

4. Stop:
```bash
stt stop
```

Manual two-terminal start is still supported:
```bash
# Terminal A
cd parakeet-stt-daemon
PARAKEET_STREAMING_ENABLED=false uv run parakeet-stt-daemon

# Terminal B
cd parakeet-ptt
cargo run --release -- --endpoint ws://127.0.0.1:8765/ws
```

## Helper Defaults (`stt start`)

Default profile:

- `--injection-mode paste`
- `--paste-key-backend auto` (ladder: uinput → ydotool)
- `--paste-backend-failure-policy copy-only`
- `--uinput-dwell-ms 18`
- `PARAKEET_STREAMING_ENABLED=false` for daemon launch (set `PARAKEET_STREAMING_ENABLED=true` for streaming validation profile)
- Adaptive routing: Terminal → Ctrl+Shift+V, General → Ctrl+V, Unknown → Ctrl+Shift+V
- Wayland focus cache: 30s stale threshold, 500ms transition grace
- Clipboard: foreground wl-copy, 700ms post-chord hold, `text/plain;charset=utf-8`

Helper readiness timing:

- `PARAKEET_CLIENT_READY_TIMEOUT_SECONDS` controls client readiness wait (default `30`)
- helper extends readiness wait when `cargo run --release` compile activity is detected

COSMIC focus-navigation baseline for best adaptive behavior:
- `Focus follows cursor = ON`
- `Focus follows cursor delay = 0ms`
- `Cursor follows focus = ON`

Primary helper commands:

- `stt start|restart|stop|status`
- `stt show` (attach tmux)
- `stt logs [client|daemon|both]`
- `stt check` (daemon health)
- `stt diag-injector` (injection diagnostics)
- `stt help` and `stt help start` (full helper + start flag reference)

`stt start` flag parsing/help/runtime args are driven by a single metadata table in
`scripts/stt-helper.sh` (`start_option_rows`).

## Testing and Validation

Client checks:
```bash
cd parakeet-ptt
cargo fmt
cargo clippy --all-targets --all-features -- -D warnings
cargo test
```

Local hardware-optimized release build (Zen 5):
```bash
cd parakeet-ptt
RUSTFLAGS="-C target-cpu=znver5" cargo build --release
```
`target-cpu=znver5` already enables the relevant AVX-512 feature set on this host; no manual `target-feature=+avx512...` list is required.

Daemon checks:
```bash
cd parakeet-stt-daemon
uv run ruff check .
uv run ruff format --check .
ty check --project . --error-on-warning
uv run deptry .
uv run parakeet-stt-daemon --check
```

Offline benchmark harness (repeatable local regression gate):
```bash
cd parakeet-stt-daemon
uv run python check_model.py \
  --bench-offline \
  --device cuda \
  --bench-output bench_audio/latest-benchmark.json \
  --max-avg-wer 0.45 \
  --max-p95-infer-ms 1800 \
  --max-p95-finalize-ms 2200
```
Personal high-signal workflow (agent-prompt-heavy, local-only assets):
```bash
cd parakeet-stt-daemon

# 1) Mine Codex CLI user prompts into reviewable candidates for this repo.
#    Default: scans recent 20 threads from this cwd and uses only role=user messages.
uv run python scripts/build_personal_eval_candidates.py \
  --output bench_audio/personal/candidates.tsv

# Optional: widen history depth.
# uv run python scripts/build_personal_eval_candidates.py \
#   --codex-max-threads 80 \
#   --output bench_audio/personal/candidates.tsv

# Optional: include command-like sources as well.
# uv run python scripts/build_personal_eval_candidates.py \
#   --include-codex-exec-commands \
#   --include-bash-history \
#   --output bench_audio/personal/candidates.tsv

# 2) Manually review candidates.tsv and set include=yes for approved prompts.
# 3) Materialize manifest + prompt list.
uv run python scripts/materialize_personal_manifest.py \
  --input bench_audio/personal/candidates.tsv \
  --output bench_audio/personal/manifest.jsonl \
  --prompts-output bench_audio/personal/prompts.tsv \
  --tier daily

# 4) Record approved prompts (interactive).
bash scripts/record_personal_clips.sh \
  --manifest bench_audio/personal/manifest.jsonl \
  --output-dir bench_audio/personal/audio

# 5) Calibrate baseline (recommended first run after corpus refresh).
uv run python check_model.py \
  --bench-offline \
  --bench-manifest bench_audio/personal/manifest.jsonl \
  --bench-tier daily \
  --calibrate-baseline \
  --baseline-output bench_audio/personal/baseline.json \
  --bench-output bench_audio/personal/latest-calibration.json

# 6) Daily hybrid gate (absolute + baseline-relative thresholds).
uv run python check_model.py \
  --bench-offline \
  --bench-manifest bench_audio/personal/manifest.jsonl \
  --bench-tier daily \
  --baseline bench_audio/personal/baseline.json \
  --bench-output bench_audio/personal/latest-daily.json
```
The benchmark command prints a per-sample + aggregate summary to stdout and writes JSON with:
- `benchmark`, `model`, `requested_device`, `effective_device`
- `bench_dir`, `manifest_path|transcripts_path`, `bench_tier`, `bench_runs`, `sample_count`
- `aggregate.avg_wer`, `aggregate.weighted_wer`, `aggregate.command_exact_match_rate`, `aggregate.critical_token_recall`
- `aggregate.infer_ms.*`, `aggregate.finalize_ms.*`, `aggregate.warm_finalize_ms.*`, `aggregate.cold_start_ms`
- `thresholds.*`, `regression_gate.pass`, `regression_gate.failures`
- `samples[]` entries including `sample_id`, `tier`, `domain`, `critical_tokens`, `reference`, `hypothesis`, `wer`, `infer_ms`, `finalize_ms`
- `runs[]` with per-run aggregates and per-sample rows when `--bench-runs > 1`
When any configured threshold is exceeded, the harness exits non-zero.

Commit and push gates (repo root):
```bash
prek install -t pre-commit -t pre-push
prek run --all-files
prek run --stage pre-push --all-files
```
Hook stages are split for speed:
- `pre-commit`: maintenance cadence reminder, `ruff format`, `ruff check`, `ty check`, `cargo fmt`
- `pre-push`: `pytest`, `cargo clippy`, `cargo test`
- Hooks are language-scoped, so Python checks run only for `parakeet-stt-daemon/` changes and Rust checks run only for `parakeet-ptt/` changes.

Maintenance audits (warned every 10 commits, non-blocking):
```bash
scripts/harness-maintenance.sh check --threshold 10
scripts/harness-maintenance.sh run
```
`run` executes `deptry` and `cargo +nightly udeps`; install `cargo-udeps` first with `cargo install cargo-udeps`.

Manual injector validation:
```bash
stt diag-injector
```

## Docs Map

- Harness engineering playbook: `docs/engineering/harness-engineering-playbook.md`
- Protocol contract: `docs/SPEC.md`
- Troubleshooting (canonical operator source): `docs/stt-troubleshooting.md`
- Historical docs archive index (non-canonical): `docs/archive/README.md`
- Historical injector handoff archive (non-canonical): `docs/archive/HANDOFF-clipboard-injector-2026-02-08.md`
- Historical cross-surface incident handoff archive (non-canonical): `docs/archive/HANDOFF-stt-cross-surface-injection-2026-02-19.md`
- Historical injection implementation roadmap (non-canonical): `docs/archive/STT-INPUT-INJECTION-ROADMAP-2026-02.md`
- UX roadmap (new): `ROADMAP.md`
