# STT helper status (updated 2026-02-08)

This document now has two parts:

1. **Current truth (2026 migration branch)** for day-to-day operations.
2. **Historical investigation notes** from earlier debugging passes.

## Current truth (2026 migration branch)

- `stt start` now uses PID-file + socket health checks for daemon lifecycle decisions, not name-only process matching.
- `stt start` rejects unknown options to avoid silent misconfiguration during injector tuning.
- Paste backend failures are policy-driven:
  - `copy-only` (default): preserve transcript delivery by writing clipboard even if key backend is unavailable.
  - `error`: fail fast for strict debugging.
- `auto` backend now performs runtime fallback attempts (`uinput -> ydotool -> wtype`) per shortcut execution.
- `stt diag-injector` reports backend capability prerequisites (`wtype`, `ydotool`, `/dev/uinput` write access) before running matrix cases.

## Historical notes (pre-2026 migration hardening)

## What works
- Running the commands manually (from the README) in two terminals works:  
  - Terminal A: `cd parakeet-stt-daemon && uv run parakeet-stt-daemon --no-streaming`  
  - Terminal B: `cd parakeet-ptt && cargo run` (or `cargo run --release`).
- The helper reliably starts the daemon with `--no-streaming` and the daemon stays up.
- When the helper succeeds in starting the client, PTT sessions run and return transcripts quickly (latency ~50–120 ms in logs).

## What fails (intermittent client start)
- When invoked from `stt start` (or plain `stt`), the client sometimes exits immediately. The helper then reports “Client failed to stay up” and may also report “Client rebuild failed” even though manual `cargo run` works.
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
- Uses absolute paths to the repo (`~/Documents/Engineering/parakeet-stt`), sets `RUST_LOG=info` if unset, and keeps `/tmp` PID files for the daemon (client PID is discovered after start).
- Daemon start: `cd parakeet-stt-daemon && nohup uv run parakeet-stt-daemon --no-streaming >> /tmp/parakeet-daemon.log 2>&1 &`, records PID, then waits up to ~30s for `PARAKEET_HOST:PARAKEET_PORT` (default 127.0.0.1:8765) and will hop to the next free port if the default is busy (unless `PARAKEET_PORT` is set). On failure, it prints the last daemon log lines.
- Client start (in tmux): appends a session header to `/tmp/parakeet-ptt.log`, runs the release binary if present, otherwise `cargo run --release -- --endpoint <resolved endpoint>`; output flows through `tee` so attaching to tmux shows live logs while still writing to the file.
- Logging: append-only (`>>`) for both daemon and client; helper emits markers like `start client in tmux`, `running cargo run --release` into the client log.
- Commands: `stt start` (default detached tmux), `stt show`/`stt attach` (attach to tmux), `stt restart`, `stt stop`, `stt status`, `stt logs [client|daemon|both]`, `stt tmux [attach|kill]` (legacy direct tmux layout), `stt check` (daemon `--check`).

## Suspicions / hypotheses
- The release binary may occasionally be in a bad state (stale build artifacts) and exits immediately; a rebuild should fix that, but we need logs to confirm.
- Environment differences between shells (PATH, Rust toolchain) could make `cargo build --release` fail in non-interactive shells; capturing stderr will clarify.
- There could be a race where the helper declares the client dead before it fully initialises, but we now wait longer and add retries.
- If `cargo`/`rustc` are missing in a shell, the build step would fail—this should now be visible in the log.

## Next debugging steps
1) Reload the helper and try a clean start: `source ~/Documents/Engineering/parakeet-stt/74-aliases-functions.bash && stt stop && stt start`. It will detach after “Dictation ready”. Use `stt show` to view the tmux panes (top: client, bottom: live logs).
2) If the daemon wait times out, grab the last 80 lines of `/tmp/parakeet-daemon.log` (printed automatically on failure).
3) If the client drops to cargo fallback or still exits, tail `/tmp/parakeet-ptt.log` and look for helper markers. Share the log.
4) Still empty logs? Capture env for that shell: `env | sort > /tmp/stt-env.txt`, set `RUST_LOG=debug`, and rerun `stt start`.

With the append-only logging, tmux-based client start, PID tracking, and longer socket wait, any new failure should leave a clear trace in `/tmp/parakeet-ptt.log` or `/tmp/parakeet-daemon.log`.

## Clipboard injection tuning (Feb 8, 2026)

Paste/copy injection now exposes a strategy-driven pipeline through `stt start` and
`parakeet-ptt`:

- `--injection-mode type|paste|copy-only`
- `--paste-shortcut ctrl-v|ctrl-shift-v|shift-insert`
- `--paste-shortcut-fallback none|ctrl-v|ctrl-shift-v|shift-insert`
- `--paste-strategy single|on-error|always-chain` (default: `always-chain`)
- `--paste-chain-delay-ms <ms>` (default: `45`)
- `--paste-post-chord-hold-ms <ms>` (default: `700`)
- `--paste-restore-policy never|delayed` (default: `never`)
- `--paste-restore-delay-ms <ms>` (default: `250`)
- `--paste-copy-foreground true|false` (default: `true`)
- `--paste-mime-type text/plain;charset=utf-8` (default)
- `--paste-key-backend wtype|ydotool|uinput|auto` (default: `wtype`)
- `--paste-backend-failure-policy copy-only|error` (default: `copy-only`)
- `--uinput-dwell-ms <ms>` (default: `18`)
- `--paste-seat <seat>` (optional)
- `--paste-write-primary true|false` (default: `false`)
- `--ydotool <path>` (optional explicit path override)

Recommended baseline for Ghostty/COSMIC:

```bash
stt start --paste \
  --paste-shortcut ctrl-shift-v \
  --paste-shortcut-fallback shift-insert \
  --paste-strategy always-chain \
  --paste-chain-delay-ms 45 \
  --paste-post-chord-hold-ms 700 \
  --paste-key-backend wtype \
  --paste-backend-failure-policy copy-only \
  --paste-restore-policy never \
  --paste-copy-foreground true
```

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

It prints backend capability checks and then runs three `--test-injection` shortcut combinations with injector debug logging.
