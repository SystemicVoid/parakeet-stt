# STT helper status (Nov 22, 2025)

This notes what we tried for the `stt` bash helper, what currently works, what does not, and the next debugging steps.

## What works
- Running the commands manually (from the README) in two terminals works:  
  - Terminal A: `cd parakeet-stt-daemon && uv run parakeet-stt-daemon --no-streaming`  
  - Terminal B: `cd parakeet-ptt && cargo run` (or `cargo run --release`).
- The helper reliably starts the daemon with `--no-streaming` and the daemon stays up.
- When the helper succeeds in starting the client, PTT sessions run and return transcripts quickly (latency ~50–120 ms in logs).

## What fails (intermittent client start)
- When invoked from `stt start` (or plain `stt`), the client sometimes exits immediately. The helper then reports “Client failed to stay up” and may also report “Client rebuild failed” even though manual `cargo run` works.
- In failing runs, `/tmp/parakeet-ptt.log` ended up empty because the helper truncated the log before spawning the client and the client exited before writing anything. We now keep a header and more instrumentation in the log.

## Evidence from logs
- Successful runs show `/tmp/parakeet-ptt.log` entries like:
  - “Starting hotkey loop; press Right Ctrl to talk”
  - “Connected to daemon”
  - Session start/stop and “final result received … latency_ms=xx”
- The daemon log `/tmp/parakeet-daemon.log` consistently shows a healthy startup on `cuda`, audio capture starting, websocket accepted, and session start/stop pairs with reasonable inference times.
- On failing runs, the client log was empty or missing; the helper reported a rebuild failure. No daemon errors were present during these failures.

## Current helper behavior (after rewrite)
- Uses absolute paths to the repo (`~/Documents/Engineering/parakeet-stt`), sets `RUST_LOG=info` if unset, and keeps `/tmp` PID files for daemon/client.
- Daemon start: `cd parakeet-stt-daemon && nohup uv run parakeet-stt-daemon --no-streaming >> /tmp/parakeet-daemon.log 2>&1 &`, records PID, then waits up to ~30s for port 8765 (0.5s polling). On failure, it prints the last daemon log lines.
- Client start: appends a session header to `/tmp/parakeet-ptt.log`, logs helper events into the file, tries the release binary first, waits briefly for the PID to stay up, then falls back to `cargo run --release -- --endpoint ws://127.0.0.1:8765/ws` if the release binary exits.
- Logging: append-only (`>>`) for both daemon and client; helper now emits markers like `start release binary`, `release binary exited quickly; fallback to cargo run` into the client log.
- Tmux option: `stt tmux` spins up a tmux session `parakeet-stt` with panes for the daemon, client, and combined log tail; `stt tmux kill` kills that session.
- Commands: `stt start` (default), `stt restart`, `stt stop`, `stt status`, `stt logs [client|daemon|both]`, `stt tmux [attach|kill]`, `stt check` (daemon `--check`).

## Suspicions / hypotheses
- The release binary may occasionally be in a bad state (stale build artifacts) and exits immediately; a rebuild should fix that, but we need logs to confirm.
- Environment differences between shells (PATH, Rust toolchain) could make `cargo build --release` fail in non-interactive shells; capturing stderr will clarify.
- There could be a race where the helper declares the client dead before it fully initialises, but we now wait longer and add retries.
- If `cargo`/`rustc` are missing in a shell, the build step would fail—this should now be visible in the log.

## Next debugging steps
1) Reload the helper and try a clean start: `source ~/Documents/Engineering/parakeet-stt/74-aliases-functions.bash && stt stop && stt start`. Give it ~10–15s (daemon warm-up), then tail both logs: `stt logs client` and `stt logs daemon`.
2) If the daemon wait still times out, grab the last 80 lines of `/tmp/parakeet-daemon.log` (the helper prints them automatically on failure).
3) If the client drops to the cargo fallback or still exits, tail `/tmp/parakeet-ptt.log` and look for the helper markers (`start release binary`, `release binary exited quickly`, `fallback cargo run --release -- --endpoint ...`). Share the full log.
4) Prefer tmux? Run `stt tmux` to get daemon/client panes plus a live log tail; `stt tmux kill` tears it down. If tmux is missing, `sudo apt install tmux`.
5) Still empty logs? Capture env for that shell: `env | sort > /tmp/stt-env.txt`, set `RUST_LOG=debug`, and rerun `stt start`.

With the append-only logging, PID tracking, and longer socket wait, any new failure should leave a clearer trace in `/tmp/parakeet-ptt.log` or `/tmp/parakeet-daemon.log`.***
