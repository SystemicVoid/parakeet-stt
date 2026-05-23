# Runtime Truth: single-owner snapshot contract

Operator-visible truth — is it streaming, is the helper active, is VAD running, is the overlay on, which finalization path ran, what are the timing metrics — used to be reassembled independently by the daemon `/status` handler, the Rust client diagnostics, the shell helper, and the docs. Each surface read its own scattered runtime fields, so they could silently disagree and an operator could not trust any single readout to describe the live session.

`RuntimeTruthSnapshot` is now the single owner of that truth. The daemon computes one `RuntimeTruth` per probe inside the SessionOrchestrator boundary (see `0003-session-orchestrator-thin-daemon-adapter.md`); `to_status()` serializes it into the one `StatusMessage` that `/status` returns, and `to_log_record()` logs the same facts. The schema's `x-runtime-truth-field-groups` metadata (`core`, `device`, `stream_path`, `seal_path`, `tail_trim_vad`, `interim_transcript`, `overlay_transport`, `timing`) names the contract groups, and the `status_*` fixtures pin the wire shape. Downstream surfaces render those serialized fields instead of recomputing them. `StatusMessage` is part of the schema-first protocol contract (see `0002-schema-first-protocol-contract-and-projections.md`); this ADR makes it the single status authority rather than one of several status views.

Drift checks fail the build when any consumer's view diverges. The Python and Rust conformance suites assert that the schema groups, the message-model fields, and the rendered status strings stay in lockstep; the helper validates the live `/status` payload against the schema groups before printing; and a docs test fails if `docs/SPEC.md` or `docs/stt-troubleshooting.md` stops naming a group. The four consumers held to the snapshot are:

- the daemon `/status` payload and structured log record;
- the Rust client diagnostics rendering;
- the shell helper status output;
- the operator docs.

Stream-path facts and interim-transcript facts are reported as separate field groups so the live overlay interim — which is presentation, not source of truth — can never be conflated with the seal-path finalization that produces the canonical text. The cost is that every operator-visible fact must flow through the snapshot, schema, fixtures, and drift checks rather than being printed ad hoc; letting any surface recompute status independently again would reintroduce exactly the silent disagreement this contract removes. This contract presumes the two-process split of `0001-two-process-python-daemon-rust-client.md`, where daemon and client are separate codebases that would otherwise drift apart.

Issues: #62, #86, #87, #92, #107, #118, #119, #120, #121.
