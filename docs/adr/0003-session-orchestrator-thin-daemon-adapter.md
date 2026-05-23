# SessionOrchestrator behind a thin WebSocket adapter

The Daemon's WebSocket server had grown into a god object. One class mixed transport
(WebSocket accept/receive/disconnect), Session lifecycle, model invocation, audio
buffering, and status reporting. The 2026-03 architecture risk audit found that real
concurrency, ownership, and resource bugs were tangled in that mud: there was no single
place to enforce who owns the active Session, to serialize inference, or to bound the
audio the Daemon retains. This work (issues #60, #61, and the deepening
follow-ups #122-#127) carves the orchestration out and leaves the server as a
transport adapter.

We introduce an intent/event seam. Transport messages become typed intents
(`StartSessionIntent` / `StopSessionIntent` / `AbortSessionIntent`); the
`SessionOrchestrator` runs the Stream path and Seal path lifecycle and reports progress
back through an `EventSink` (`events.py`). `DaemonServer` is now only the WebSocket
adapter: it translates messages to intents and pumps `SessionEvent`s to the socket. The
orchestrator has no knowledge of WebSockets.

This is the shallow-to-deep module move. `SessionOrchestrator` is a deep module — a
narrow intent/event interface hiding the hard capture and transcription lifecycle. The
`EventSink` seam lets the same orchestrator be driven by a real `WebSocketEventSink` or
by a test fixture (`RecordingEventSink`), so the lifecycle is exercisable without a
socket. It also gives us one testable boundary at which Session ownership
(`owner_token`), inference serialization (`_inference_lock` /
`_transcribe_samples_serialized`), and bounded buffering are actually enforceable —
the same boundary the architecture risk audit and the single-owner contract
(`docs/adr/0004-runtime-truth-single-owner-contract.md`) depend on. The orchestrator
composes deeper sub-modules in turn (the Seal path tail trimmer #59, the Overlay interim
stabilizer #58).

This extends the two-process split (ADR 0001) inward: the IPC edge stays thin in both
languages. The cost is more files and one indirection — intents plus events — instead of
inline handler methods. That cost is deliberate: re-merging the orchestrator back into
the transport layer would collapse the boundary and reintroduce exactly the audited
ownership, serialization, and resource bugs, so this seam is load-bearing.
