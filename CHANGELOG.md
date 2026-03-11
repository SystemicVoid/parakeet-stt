# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added
- Persistent in-process uinput sender to restore paste reliability lost in subprocess refactor
- One-time warm-up delay for freshly created uinput devices
- Paste-gap matrix runner (`scripts/paste-gap-matrix.sh`) for validation testing
- Comprehensive session lifecycle tests (`test_session_cleanup.py`)
- Enhanced overlay event stream tests (`test_overlay_event_stream.py`)
- CLI argument precedence tests (`test_cli_precedence.py`)
- Injector child diagnostics and enriched backend evidence
- Parameterized diag-injector runs via `stt diag-injector`
- Host-optimized build command (`just build`) with `RUSTFLAGS="-C target-cpu=native"`
- Soak testing command (`just soak-perf`) for performance sampling
- uinput lifecycle telemetry fields (generation, freshness, age, use count, recovery)
- Hotkey modifier state seeding from kernel on attach

### Changed
- Restored paste reliability lost during subprocess injector architecture refactor
  - Previous persistent in-process model achieved ~95% success
  - Subprocess refactor regressed to ~20% due to per-job virtual keyboard recreation
  - New persistent in-process model with lazy/retryable initialization fixes the regression
- Daemon sessions now bound to owning websocket (PR #19)
- Enforced bounded session capture to prevent resource leaks
- Transcriber access serialized across stop operations
- Paste backend is now uinput-only (ydotool and auto modes removed)
- Legacy paste backend values coerced to uinput
- Paste backend failures follow copy-only policy by default
- Audio pre-roll bounded before session caps
- Stop races aborted on breached session caps
- Hotkey listeners stop cooperatively with proper cleanup
- Injector execution moved to killable subprocesses with tree termination
- Injector stderr drain threads bounded and deferred to exit
- WL-copy helper stdio detached to prevent pipe leaks
- Focus snapshot preservation across serde
- Runtime troubleshooting docs updated with persistent sender behavior

### Fixed
- **Paste reliability regression**: Root cause traced to subprocess injector refactor
  - Previous persistent in-process model achieved ~95% success
  - Refactor to subprocess-per-injection regressed to ~20% reliability
  - Root cause: per-job `/dev/uinput` device recreation triggered compositor discovery race
  - Fix: Persistent in-process uinput sender with lazy/retryable initialization
  - Added bounded warm-up delay after fresh device creation for compositor readiness
- Daemon session boundary issues identified in security audit (PR #20)
  - Sessions now properly bound to owning websocket
  - Fixed stop race conditions on breached session caps
- Hotkey modifier state not seeded from kernel on attach/resume
- Dead listener key state not unwound properly on exit
- Overlapping hotkey bindings not rejected
- Parakeet virtual keyboard devices not filtered from hotkey detection
- Null parent focus snapshots not guarded
- Clipboard write failures not reported
- Stale llama PID not cleaned up before health wait
- Managed LLM base URL not validated against managed host/port
- Managed LLM overrides not preserved on recursive start
- Llama argv not passed through env to tmux
- Helper pane selection assumed `.0` index (now index-agnostic)

### Removed
- ydotool paste backend and auto mode
- Legacy paste backend ladder
- Retired paste backend selector
- Overlay perf suggestions (archived as not priority)
- Unused audio feedback handler argument

### Deprecated
- Legacy paste backend values (`ydotool`, `auto`) now coerced to `uinput`

### Security
- Fixed session ownership issues identified in rigorous architecture risk audit (PR #20)
  - Sessions now bound to owning websocket
  - Enforced bounded session capture
  - Fixed stop race conditions

### Documentation
- Archived paste-gap investigation documents to `docs/archive/`
  - `PASTE-RELIABILITY-ANALYSIS-2026-03-10.md`
  - `paste-gap-deep-dive-2026-03-11.md`
  - `HANDOFF-raw-ptt-paste-gap-2026-03-08.md`
  - `codemap-deepwiki-uinput.md`
- Added paste-gap evidence gathering user manual
- Updated runtime troubleshooting docs with persistent sender behavior
- Tightened AGENTS.md wording and removed stale entries
- Consolidated draft files to archive with unified ignore pattern
- Removed stale status sections and archived obsolete documents

### Testing
- Added comprehensive session lifecycle tests (245 lines)
- Enhanced overlay event stream tests (160 lines)
- Added CLI argument precedence tests (24 lines)
- Fixed streaming truth tests for streaming changes
- Added injector subprocess leak proof tests
- Added session guardrail validation tests

### Dependencies
- Updated `quinn-proto` from 0.11.13 to 0.11.14

## [0.2.1] - 2026-03-07

### Added
- Streaming overlay support
- LLM query mode with managed llama-server
- Hotkey modifier-based intent switching (Shift+PTT = LLM)
- Paste-gap diagnostics (`stt diag-injector`)

### Changed
- Default profile now uses online stream+seal with overlay enabled
- Helper uses PID-file + socket health checks for daemon lifecycle
- Added bounded worker queue for final-result injection
- Added event-loop lag summaries

### Fixed
- Client readiness wait extends when cargo compile is active
- Helper pane selection now index-agnostic
- Managed llama-server start/stop lifecycle

## [0.2.0] - 2026-02-25

### Added
- Initial streaming support
- Paste injection with adaptive routing
- Overlay renderer
- Session ownership and guardrails

## [0.1.1] - 2026-02-19

### Fixed
- Cross-surface injection routing
- Focus snapshot handling
- Daemon start/restart lifecycle

## [0.1.0] - 2026-02-08

### Added
- Initial release
- Push-to-talk dictation
- Basic paste injection
- WebSocket daemon protocol
