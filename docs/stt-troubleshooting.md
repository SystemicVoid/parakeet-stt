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

## Current helper behavior (after fixes)
- Start flow: start daemon → wait for port 8765 → truncate client log with a timestamp header → run release binary; if it dies, tail logs → rebuild release → retry; if still dead, fall back to `cargo run -- --endpoint ws://127.0.0.1:8765/ws`.
- All steps log timestamps into `/tmp/parakeet-ptt.log` so we always have artifacts now.
- Commands: `stt start`, `stt stream`, `stt restart`, `stt stop`, `stt status`, `stt logs`, `stt check`.

## Suspicions / hypotheses
- The release binary may occasionally be in a bad state (stale build artifacts) and exits immediately; a rebuild should fix that, but we need logs to confirm.
- Environment differences between shells (PATH, Rust toolchain) could make `cargo build --release` fail in non-interactive shells; capturing stderr will clarify.
- There could be a race where the helper declares the client dead before it fully initialises, but we now wait longer and add retries.
- If `cargo`/`rustc` are missing in a shell, the build step would fail—this should now be visible in the log.

## Next debugging steps
1) Reproduce failure with logging kept:  
   ```bash
   stt stop
   rm -f /tmp/parakeet-ptt.log
   stt start
   sleep 8
   tail -n +1 /tmp/parakeet-ptt.log
   ps -ef | grep parakeet-ptt
   ```
   Share the full log so we can see the build/run error.

2) If the release binary keeps dying, force a clean rebuild:  
   ```bash
   cd parakeet-ptt
   cargo clean && cargo build --release
   ```
   Then `stt start` again and check the log.

3) If the fallback `cargo run` path triggers, inspect the log for compile errors; ensure Rust toolchain and deps are present (`rustup show`, `cargo --version`).

4) To rule out PATH issues in login shells, capture env from the failing shell:  
   ```bash
   env | sort > /tmp/stt-env.txt
   ```
   Compare to a working shell.

5) If failures persist, increase verbosity: set `RUST_LOG=debug` before `stt start` and check `/tmp/parakeet-ptt.log`.

With the current instrumentation, the next failing start should leave evidence in `/tmp/parakeet-ptt.log` that will point to the exact cause.***
