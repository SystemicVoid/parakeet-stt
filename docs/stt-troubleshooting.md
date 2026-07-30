# STT helper status (updated 2026-05-20)

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
- Default startup profile is online stream+seal (`stt` / `stt start`); exact
  profile defaults and stable start controls are generated from Helper metadata
  and checked below.
  <!-- stt-helper:start-reference:start -->
  Helper profile defaults (generated from `start_profile_rows`):

  | Profile | Start tokens | Direct commands | Streaming | Device | Overlay | Overlay width | Description |
  | --- | --- | --- | --- | --- | --- | --- | --- |
  | `streaming` (default) | `stream`, `streaming`, `on` | `stt`, `stt start`, `stt stream`, `stt on` | `true` | `cuda` | `true` | `false` | Launch daemon with stream+seal + overlay defaults. |
  | `offline` | `offline`, `off` | `stt off` | `false` | `cuda` | `false` | `false` | Launch daemon with streaming disabled. |
  | `cpu` | `cpu` | `stt cpu` | `false` | `cpu` | `false` | `false` | Launch daemon in offline mode on CPU (no GPU). |

  `stt start` default-profile options (generated from `start_option_rows`):

  | Option | Default for `stt start` | Env |
  | --- | --- | --- |
  | `--injection-mode <mode>` | `paste` | `PARAKEET_INJECTION_MODE` |
  | `--paste-backend-failure-policy <v>` | `copy-only` | `PARAKEET_PASTE_BACKEND_FAILURE_POLICY` |
  | `--uinput-dwell-ms <n>` | `18` | `PARAKEET_UINPUT_DWELL_MS` |
  | `--paste-seat <v>` | `<unset>` | `PARAKEET_PASTE_SEAT` |
  | `--paste-write-primary <v>` | `false` | `PARAKEET_PASTE_WRITE_PRIMARY` |
  | `--completion-sound <v>` | `true` | `PARAKEET_COMPLETION_SOUND` |
  | `--completion-sound-path <path>` | `<system default>` | `PARAKEET_COMPLETION_SOUND_PATH` |
  | `--completion-sound-volume <n>` | `100` | `PARAKEET_COMPLETION_SOUND_VOLUME` |
  | `--overlay-enabled <v>` | `true` | `PARAKEET_OVERLAY_ENABLED` |
  | `--overlay-adaptive-width <v>` | `false` | `PARAKEET_OVERLAY_ADAPTIVE_WIDTH` |
  | `--llm-pre-modifier-key <key>` | `KEY_SHIFT` | `PARAKEET_LLM_PRE_MODIFIER_KEY` |
  | `--llm-base-url <url>` | `http://127.0.0.1:8081/v1` | `PARAKEET_LLM_BASE_URL` |
  | `--llm-model <name>` | `local` | `PARAKEET_LLM_MODEL` |
  | `--llm-timeout-seconds <n>` | `20` | `PARAKEET_LLM_TIMEOUT_SECONDS` |
  | `--llm-max-tokens <n>` | `512` | `PARAKEET_LLM_MAX_TOKENS` |
  | `--llm-temperature <n>` | `0.7` | `PARAKEET_LLM_TEMPERATURE` |
  | `--llm-system-prompt <text>` | `<assistant prompt>` | `PARAKEET_LLM_SYSTEM_PROMPT` |
  | `--llm-overlay-stream <v>` | `true` | `PARAKEET_LLM_OVERLAY_STREAM` |
  <!-- stt-helper:start-reference:end -->
  - Wayland focus cache with 30s stale threshold, 500ms transition grace
- low-confidence focus snapshots (`focus_focused=false`) now route as `unknown` (terminal-first default)
- Paste backend failures are policy-driven:
  - `copy-only` (default): preserve transcript delivery by writing clipboard even if key backend is unavailable.
  - `error`: fail fast for strict debugging.
- The in-process paste path now keeps one `uinput` virtual keyboard alive while healthy instead of creating a fresh device for every job.
- If `/dev/uinput` is unavailable, the client no longer poisons the whole session:
  - failed create attempts fall back per policy (`copy-only` or `error`)
  - the worker retries sender creation on later jobs after a short internal backoff
- Freshly created or recovered `uinput` devices now pay a one-time warm-up delay before the first real paste chord so COSMIC/libinput can discover and route the device before the shortcut is emitted.
- Final-result injection is now enqueued to a dedicated bounded worker queue (`capacity=32`) so hotkey/websocket handling paths do not await blocking clipboard/chord execution inline.
- Worker enqueue backpressure is timeout-limited (`20ms`) with explicit dropped-job warnings when the queue stays saturated.
- Injector logs now tag stage outcomes and durations with `stage=<clipboard_ready|route_shortcut|backend>` and `status=<start|ok|fail>`.
- Backend stage failure accounting reflects initialization and command failures from the uinput-only injector path.
- Backend-attempt summaries for `uinput` now include sender lifecycle fields so paste-gap runs can distinguish fresh vs reused devices:
  - `ugen=<n>` sender generation
  - `ufresh=1|0` whether this was the first routed use after create/recovery
  - `uage_ms=<ms>` device age when the chord was attempted
  - `uuse=<n>` prior successful uses on the current sender
  - `ucreated_this_job=1|0` whether the sender was created for the current job
  - `ucreate_ms=<ms>` sender creation latency for fresh jobs
  - `urecovered=1|0` whether the current sender generation came from recovery after a prior create failure
- Queue and stage metric summaries are emitted periodically from the client loop (`injector worker queue metrics summary`, `injector stage metrics summary`).
- Event-loop lag summaries are emitted every 30 seconds (`event loop lag window summary`) with p50/p95/p99 fields, measured against the interval schedule so windows recover after transient stalls.
- Hotkey listeners now seed already-held `llm_pre_modifier` state from the kernel when they attach or re-attach, so the first utterance after startup/resume/device recovery still routes to LLM mode if Shift was already held.
- `stt diag-injector` reports `/dev/uinput` capability before running reproducible uinput-only cases.
- Client readiness wait for `stt start` is timeout-based (`PARAKEET_CLIENT_READY_TIMEOUT_SECONDS`, default `30`) and extends when cargo compile is still active.
- Helper pane selection is index-agnostic (no `.0` assumption), so tmux `pane-base-index 1` configs are supported.
- Adaptive routing treats `focus_focused=false` snapshots as low-confidence and routes using unknown policy (terminal-first default).
- Routing shortcuts (Terminal→CtrlShiftV, General→CtrlV, Unknown→CtrlShiftV), clipboard MIME type, and copy-foreground behavior are hardcoded constants — no longer configurable via CLI.
- `stt` auto-loads ignored repo-local files `.parakeet-stt.local.env` and `.parakeet-stt.local.sh` when present, so machine-local launcher paths stay out of tracked config.
- `stt llm` manages a local `llama-server` in tmux session `parakeet-llm`, waits for `http://<host>:<port>/health`, then delegates to the normal `stt start` path.
- Machine-local LLM overrides should stay in `PARAKEET_LLM_*` or `PARAKEET_LLM_SERVER_*` env vars from your shell or the ignored repo-local files; do not commit workstation-specific endpoints or launcher paths.

Runtime truth for `/status` and Daemon logs is produced by the
`parakeet_stt_daemon.runtime_truth_snapshot` module and serialized through the
protocol `StatusMessage` Runtime Truth contract. That contract is the source of
truth for effective device, Stream path helper state, Stream path execution
evidence, Seal path finalization source, tail trim mode, VAD fallback, interim
transcript source activity, overlay-event transport, timing, and counter fields;
the Helper should read those fields from `/status` instead of re-deriving them.

Use `stt status` for Helper process health: Daemon PID, Client PID, endpoint,
tmux session, matching processes, and the normalized Daemon Runtime Truth block
when `/status` is available. Use the Daemon `/status` payload, the Client
startup status log line, and Daemon session runtime truth logs for deeper
runtime truth. The Helper may parse `/status` to decide whether an existing
Daemon matches the requested Profile, but shell logic should not infer Stream
path, Seal path, interim transcript, or Overlay transport state from process
names, flags, or logs alone.

### Runtime truth field guide

The schema-defined Runtime Truth groups are `core`, `device`, `stream_path`,
`seal_path`, `tail_trim_vad`, `interim_transcript`, `overlay_transport`, and
`timing`.

- Device and status core: `state`, `sessions_active`, `device`,
  `effective_device`, and `gpu_mem_mb`.
- Stream path: `streaming_enabled`, `stream_helper_active`,
  `stream_helper_scope`, `stream_fallback_reason`, `stream_path_executed`, and
  `stream_chunks_processed`, and `chunk_secs`.
- Seal path: `finalization_mode`, `final_audio_source`, `tail_trim_mode`,
  `vad_enabled`, `vad_active`, `vad_fallback_reason`, and finalization timing
  fields such as `audio_stop_ms`, `finalize_ms`, `infer_ms`, and `send_ms`.
- Daemon interim transcript sources: `interim_transcript_enabled`,
  `interim_transcript_last_source`,
  `interim_transcript_live_chunks_processed`,
  `interim_transcript_stop_replay_chunks_processed`,
  `interim_transcript_updates_emitted`,
  `interim_transcript_live_updates_emitted`,
  `interim_transcript_stop_replay_updates_emitted`,
  `interim_transcript_live_failed`,
  `interim_transcript_stop_replay_failed`, and
  `interim_transcript_source_fallback_reason`.
- Overlay event transport: `overlay_events_enabled`,
  `overlay_events_emitted`, and `overlay_events_dropped`.
- Timing and counters: `active_session_age_ms`, `audio_stop_ms`,
  `finalize_ms`, `infer_ms`, `send_ms`, `last_audio_ms`, `last_infer_ms`, and
  `last_send_ms`.
- Client-side LLM answer deltas: generated and streamed by the Client in LLM
  query mode. They can update the Overlay while the local LLM answers, but they
  are not Daemon interim transcript fields and do not affect Stream path or Seal
  path truth.
- Renderer animation: local Overlay presentation only. Character fades,
  listening/finalizing motion, width changes, and opacity transitions are not
  inference, transport, or LLM progress evidence.

### Live Overlay text with `stream_path_executed=false`

This is not automatically a contradiction. The Stream path truth fields report
whether the NeMo Stream path helper processed chunks. The Daemon interim
transcript sources are a separate display-only path for Overlay text: the
`live` source consumes arriving audio chunks during a Session, and the
`stop_replay` source can replay ready chunks while stopping. A Session can
therefore show live Overlay interim text while `/status` still reports
`stream_path_executed=false`.

When this happens, inspect the groups separately:

1. Check `streaming_enabled`, `stream_helper_active`,
   `stream_fallback_reason`, `stream_path_executed`, and
   `stream_chunks_processed` to see whether the NeMo Stream path was requested,
   available, and exercised.
2. Check `interim_transcript_enabled`,
   `interim_transcript_last_source`,
   `interim_transcript_live_chunks_processed`,
   `interim_transcript_live_updates_emitted`,
   `interim_transcript_stop_replay_chunks_processed`,
   `interim_transcript_stop_replay_updates_emitted`, and
   `interim_transcript_source_fallback_reason` to see which Daemon interim
   source produced visible text.
3. Check `overlay_events_enabled`, `overlay_events_emitted`, and
   `overlay_events_dropped` to confirm whether Overlay events were transported
   or dropped.
4. Check `finalization_mode`, `final_audio_source`, `tail_trim_mode`, and the
   finalization timing fields to verify Seal path finalization for the result
   that will be injected.
5. If the Session used LLM query mode, distinguish Client-side LLM answer
   deltas from Daemon interim transcript updates. LLM deltas are not evidence
   that the Stream path executed.

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
  - Session start/stop and “final result received … daemon_latency_ms=xx”
  - Injector completion lines with `enqueue_to_injection_complete_ms`, `hotkey_up_elapsed_ms_at_completion`,
    and `stop_message_elapsed_ms_at_completion`
- The daemon log `/tmp/parakeet-daemon.log` consistently shows a healthy startup on `cuda`, audio capture starting, websocket accepted, and session start/stop pairs with reasonable inference times.
- On failing runs, the client log was empty or missing; the helper reported a rebuild failure. No daemon errors were present during these failures.

## Current helper behavior (after rewrite)
- Default start uses tmux, detached: `stt` or `stt start` launches the daemon (nohup), then creates a tmux session `parakeet-stt` with a single window split into panes (top: client via `tee` to `/tmp/parakeet-ptt.log`; bottom: live `tail -f` of daemon+client logs). It waits for the daemon socket and a running client PID before printing “Dictation ready” and returning you to your shell.
- Resolves repo paths dynamically from the helper location (or `PARAKEET_ROOT`), auto-loads ignored repo-local helper overrides, sets `RUST_LOG=info` if unset, and keeps `/tmp` PID files for the daemon (client PID is discovered after start).
- Daemon PID tracking now refreshes from the bound listener port after startup/status checks, because the initial `uv run` launcher PID may differ from the long-lived Python server PID.
- Daemon endpoint and path defaults are projected from `docs/protocol/runtime-interface.json`; update that contract and its checked projections when changing the default host, port, or status/health/WebSocket paths.
- Managed LLM start (`stt llm`) uses a separate tmux session and log (`/tmp/parakeet-llama-server.log`), refreshes its PID from the bound listener port after health checks, and refuses mismatched `PARAKEET_LLM_BASE_URL` versus the managed host/port to avoid split-brain local config.
- Daemon start: `cd parakeet-stt-daemon && nohup uv run parakeet-stt-daemon >> /tmp/parakeet-daemon.log 2>&1 &`, records PID, then waits up to ~30s for `PARAKEET_HOST:PARAKEET_PORT` (default 127.0.0.1:8765) and will hop to the next free port if the default is busy (unless `PARAKEET_PORT` is set). Profile defaults determine streaming, device, and overlay behavior; the generated profile table above is the source of truth. On failure, it prints the last daemon log lines.
- Client start (in tmux): appends a session header to `/tmp/parakeet-ptt.log`, prefers a compatible prebuilt `target/release/parakeet-ptt` binary, and falls back to `cargo run --release -- --endpoint <resolved endpoint>` only when the binary is missing or incompatible with the expected helper flags; output flows through `tee` so attaching to tmux shows live logs while still writing to the file.
- Logging: append-only (`>>`) for both daemon and client; helper emits markers like `start client in tmux`, `running cargo run --release` into the client log.
- Commands: `stt`/`stt start` (default detached tmux, stream+seal profile), `stt llm` (managed llama + STT), `stt off` (offline profile), `stt cpu` (offline CPU profile), `stt show`/`stt attach` (attach to tmux), `stt restart`, `stt stop`, `stt status`, `stt logs [client|daemon|both]`, `stt llm logs`, `stt llm show`, `stt tmux [attach|kill]` (attach/kill the helper tmux session created by `stt start`; does not launch daemon/client), `stt check` (daemon `--check`).

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

Client knobs are listed in the checked Helper metadata reference above.

Recommended baseline for Ghostty/COSMIC:

```bash
stt start --paste \
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

It prints `/dev/uinput` capability checks and then runs reproducible `uinput` test-injection cases with injector debug logging.

### Paste-gap matrix harness

For the raw-paste Ghostty investigation, use the repo-local harness instead of manually juggling `/tmp` artifacts:

```bash
just paste-gap-start
```

That command:

- records the current commit SHA and worktree status
- clears `/tmp/parakeet-ptt.log`, `/tmp/parakeet-daemon.log`, and `/tmp/parakeet-ghostty-sink.txt`
- starts `stt` with the fixed `uinput` path and `--paste-backend-failure-policy error`
- seeds operator observation templates under `/tmp/parakeet-paste-gap/...`

After the manual Ghostty utterance run, archive and summarize with:

```bash
just paste-gap-stop
just paste-gap-diag
just paste-gap-summary
```

Useful follow-ups:

```bash
just paste-gap-current
just paste-gap-summary run_dir=/tmp/parakeet-paste-gap/<run-dir>
```

The harness writes:

- `summary.txt`: counts by origin, route, backend-attempt string, focus app, and clipboard fields
- `injector-subprocess-report.tsv`: mechanically extracted `injector subprocess report` rows
- `injector-subprocess-report.raw.tsv`: raw-only subset for the failing path
- `raw-observation-joined.tsv`: created only when raw report count matches the operator observation row count

This keeps the experiment honest: same stack, same `/tmp` artifacts, different backend.
