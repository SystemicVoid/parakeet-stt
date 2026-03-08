# STT helper status (updated 2026-03-05)

This document now has two parts:

1. **Current truth (2026 migration branch)** for day-to-day operations.
2. **Historical investigation notes** from earlier debugging passes.

Canonical-source policy:
- This file is the canonical operator source of truth for runtime behavior and troubleshooting.
- `scripts/stt-helper.sh` (`start_option_rows`) is the canonical source for helper start flags/defaults/env wiring.
- `docs/archive/HANDOFF-clipboard-injector-2026-02-08.md` is historical context only and not operational guidance.
- `docs/archive/HANDOFF-stt-cross-surface-injection-2026-02-19.md` is archived historical context only and not operational guidance.

## Current truth (2026 migration branch)

- `stt start` now uses PID-file + socket health checks for daemon lifecycle decisions, not name-only process matching.
- `stt start` rejects unknown options to avoid silent misconfiguration during injector tuning.
- Default startup profile is now online stream+seal (`stt` / `stt start`) with paste-mode and adaptive cross-surface shortcut routing using internal defaults:
  - `--injection-mode paste`
  - `--paste-key-backend auto` (ladder: uinput → ydotool)
  - `--paste-backend-failure-policy copy-only`
  - daemon launch default: `PARAKEET_STREAMING_ENABLED=true` for the default profile
  - overlay launch default: `PARAKEET_OVERLAY_ENABLED=true` with `--overlay-adaptive-width=false`
  - `stt off` switches to offline profile defaults (`PARAKEET_STREAMING_ENABLED=false`, overlay disabled)
  - Wayland focus cache with 30s stale threshold, 500ms transition grace
- low-confidence focus snapshots (`focus_focused=false`) now route as `unknown` (terminal-first default)
- Paste backend failures are policy-driven:
  - `copy-only` (default): preserve transcript delivery by writing clipboard even if key backend is unavailable.
  - `error`: fail fast for strict debugging.
- `auto` backend now performs runtime fallback attempts (uinput → ydotool) per shortcut execution.
- Final-result injection is now enqueued to a dedicated bounded worker queue (`capacity=32`) so hotkey/websocket handling paths do not await blocking clipboard/chord execution inline.
- Worker enqueue backpressure is timeout-limited (`20ms`) with explicit dropped-job warnings when the queue stays saturated.
- Injector logs now tag stage outcomes and durations with `stage=<clipboard_ready|route_shortcut|backend>` and `status=<start|ok|fail>`.
- Backend stage failure accounting includes `ydotool` spawn failures (missing/non-executable binary), not just non-zero exit statuses.
- Queue and stage metric summaries are emitted periodically from the client loop (`injector worker queue metrics summary`, `injector stage metrics summary`).
- Event-loop lag summaries are emitted every 30 seconds (`event loop lag window summary`) with p50/p95/p99 fields, measured against the interval schedule so windows recover after transient stalls.
- Hotkey listeners now seed already-held `llm_pre_modifier` state from the kernel when they attach or re-attach, so the first utterance after startup/resume/device recovery still routes to LLM mode if Shift was already held.
- `stt diag-injector` reports backend capability prerequisites (`ydotool`, `/dev/uinput` write access) before running matrix cases.
- Client readiness wait for `stt start` is timeout-based (`PARAKEET_CLIENT_READY_TIMEOUT_SECONDS`, default `30`) and extends when cargo compile is still active.
- Helper pane selection is index-agnostic (no `.0` assumption), so tmux `pane-base-index 1` configs are supported.
- Adaptive routing treats `focus_focused=false` snapshots as low-confidence and routes using unknown policy (terminal-first default).
- Routing shortcuts (Terminal→CtrlShiftV, General→CtrlV, Unknown→CtrlShiftV), clipboard MIME type, and copy-foreground behavior are hardcoded constants — no longer configurable via CLI.
- `stt` auto-loads ignored repo-local files `.parakeet-stt.local.env` and `.parakeet-stt.local.sh` when present, so machine-local launcher paths stay out of tracked config.
- `stt llm` manages a local `llama-server` in tmux session `parakeet-llm`, waits for `http://<host>:<port>/health`, then delegates to the normal `stt start` path.
- Machine-local LLM overrides should stay in `PARAKEET_LLM_*` or `PARAKEET_LLM_SERVER_*` env vars from your shell or the ignored repo-local files; do not commit workstation-specific endpoints or launcher paths.

## Historical notes (pre-2026 migration hardening)

## What works
- Running the commands manually (from the README) in two terminals works:  
  - Terminal A: `cd parakeet-stt-daemon && uv run parakeet-stt-daemon`  
  - Terminal B: `cd parakeet-ptt && cargo run` (or `cargo run --release`).
- The helper reliably starts the daemon and the daemon stays up.
- When the helper succeeds in starting the client, PTT sessions run and return transcripts quickly (latency ~50–120 ms in logs).

## What failed previously (resolved on 2026-02-19)
- When invoked from `stt start` (or plain `stt`), the helper previously could report “Client failed to stay up” while `cargo run --release` compilation was still in progress.
- In failing runs, `/tmp/parakeet-ptt.log` ended up empty because the helper truncated the log before spawning the client and the client exited before writing anything. We now keep a header and more instrumentation in the log.
- If another process binds port 8765 (e.g., Anki), the daemon would previously crash with `address already in use`; the helper now rebinds to the next free port unless `PARAKEET_PORT` is explicitly set.

## Evidence from logs
- Successful runs show `/tmp/parakeet-ptt.log` entries like:
  - “Starting hotkey loop; press Right Ctrl to talk”
  - “Connected to daemon”
  - Session start/stop and “final result received … latency_ms=xx”
- The daemon log `/tmp/parakeet-daemon.log` consistently shows a healthy startup on `cuda`, audio capture starting, websocket accepted, and session start/stop pairs with reasonable inference times.
- On failing runs, the client log was empty or missing; the helper reported a rebuild failure. No daemon errors were present during these failures.

## Current helper behavior (after rewrite)
- Default start uses tmux, detached: `stt` or `stt start` launches the daemon (nohup), then creates a tmux session `parakeet-stt` with a single window split into panes (top: client via `tee` to `/tmp/parakeet-ptt.log`; bottom: live `tail -f` of daemon+client logs). It waits for the daemon socket and a running client PID before printing “Dictation ready” and returning you to your shell.
- Resolves repo paths dynamically from the helper location (or `PARAKEET_ROOT`), auto-loads ignored repo-local helper overrides, sets `RUST_LOG=info` if unset, and keeps `/tmp` PID files for the daemon (client PID is discovered after start).
- Daemon PID tracking now refreshes from the bound listener port after startup/status checks, because the initial `uv run` launcher PID may differ from the long-lived Python server PID.
- Managed LLM start (`stt llm`) uses a separate tmux session and log (`/tmp/parakeet-llama-server.log`), refreshes its PID from the bound listener port after health checks, and refuses mismatched `PARAKEET_LLM_BASE_URL` versus the managed host/port to avoid split-brain local config.
- Daemon start: `cd parakeet-stt-daemon && nohup uv run parakeet-stt-daemon >> /tmp/parakeet-daemon.log 2>&1 &`, records PID, then waits up to ~30s for `PARAKEET_HOST:PARAKEET_PORT` (default 127.0.0.1:8765) and will hop to the next free port if the default is busy (unless `PARAKEET_PORT` is set). Profile defaults determine whether `PARAKEET_STREAMING_ENABLED` is true (`stt`) or false (`stt off`). On failure, it prints the last daemon log lines.
- Client start (in tmux): appends a session header to `/tmp/parakeet-ptt.log`, runs the release binary if present, otherwise `cargo run --release -- --endpoint <resolved endpoint>`; output flows through `tee` so attaching to tmux shows live logs while still writing to the file.
- Logging: append-only (`>>`) for both daemon and client; helper emits markers like `start client in tmux`, `running cargo run --release` into the client log.
- Commands: `stt`/`stt start` (default detached tmux, stream+seal profile), `stt llm` (managed llama + STT), `stt off` (offline profile), `stt show`/`stt attach` (attach to tmux), `stt restart`, `stt stop`, `stt status`, `stt logs [client|daemon|both]`, `stt llm logs`, `stt llm show`, `stt tmux [attach|kill]` (legacy direct tmux layout), `stt check` (daemon `--check`).

## Suspicions / hypotheses
- The release binary may occasionally be in a bad state (stale build artifacts) and exits immediately; a rebuild should fix that, but we need logs to confirm.
- Environment differences between shells (PATH, Rust toolchain) could make `cargo build --release` fail in non-interactive shells; capturing stderr will clarify.
- There could be a race where the helper declares the client dead before it fully initialises, but we now wait longer and add retries.
- If `cargo`/`rustc` are missing in a shell, the build step would fail—this should now be visible in the log.

## Next debugging steps
1) Reload the helper and try a clean start: `source scripts/stt-helper.sh && stt stop && stt start`. It will detach after “Dictation ready”. Use `stt show` to view the tmux panes (top: client, bottom: live logs).
2) If the daemon wait times out, grab the last 80 lines of `/tmp/parakeet-daemon.log` (printed automatically on failure).
3) If the client drops to cargo fallback or still exits, tail `/tmp/parakeet-ptt.log` and look for helper markers. Share the log.
4) Still empty logs? Capture env for that shell: `env | sort > /tmp/stt-env.txt`, set `RUST_LOG=debug`, and rerun `stt start`.

With the append-only logging, tmux-based client start, PID tracking, and longer socket wait, any new failure should leave a clear trace in `/tmp/parakeet-ptt.log` or `/tmp/parakeet-daemon.log`.

## Clipboard injection tuning (Feb 8, 2026)

Paste/copy injection now exposes a stable operator surface through `stt start` and
`parakeet-ptt`.

Client knobs:
- `--injection-mode paste|copy-only`
- `--paste-key-backend ydotool|uinput|auto` (default: `auto`, ladder: uinput→ydotool)
- `--paste-backend-failure-policy copy-only|error` (default: `copy-only`)
- `--uinput-dwell-ms <ms>` (default: `18`)
- `--paste-seat <seat>` (optional)
- `--paste-write-primary true|false` (default: `false`)
- `--ydotool <path>` (optional explicit path override)
- `--completion-sound true|false` (default: `true`)
- `--completion-sound-path <path>` (optional)
- `--completion-sound-volume <0-100>` (default: `100`)
- `--overlay-enabled true|false` (profile default: `true` for `stt`/`stt start`, `false` for `stt off`, env: `PARAKEET_OVERLAY_ENABLED`)

Recommended baseline for Ghostty/COSMIC:

```bash
stt start --paste \
  --paste-key-backend auto \
  --paste-backend-failure-policy copy-only
```

COSMIC focus-navigation baseline for adaptive routing:
- `Focus follows cursor = ON`
- `Focus follows cursor delay = 0ms`
- `Cursor follows focus = ON`

If automatic paste is still unstable, force deterministic behavior while preserving transcript
delivery:

```bash
stt start --copy-only
```

### Injector diagnostics

Use the new helper matrix command:

```bash
stt diag-injector
```

It prints backend capability checks and then runs three `--test-injection` backend cases (`auto`, `uinput`, `ydotool`) with injector debug logging.
