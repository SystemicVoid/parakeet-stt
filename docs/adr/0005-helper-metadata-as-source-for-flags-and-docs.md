# Helper metadata as the single source for `stt start` flags and docs

The same knowledge about each `stt start` option — its flag name, default, env-var wiring, and help text — used to live independently in the argument parser, the help output, the daemon launch args, the diagnostics, and the operator docs. Changing one default meant editing all of them, and they drifted: the docs would describe a default the helper no longer used.

We made one metadata table the source of that knowledge. `start_option_rows` (and the companion `start_profile_rows`) in `scripts/stt-helper.sh` is a list of pipe-delimited rows describing each option once; every operator surface is now a projection of those rows rather than a hand-maintained copy (#132, #135):

- the argument parser and option validation,
- `stt help start` and `stt help llm`,
- the daemon launch args passed to the Rust client,
- the binary-capability diagnostic (does this `parakeet-ptt` build accept every flag we emit?),
- the generated reference region in `docs/stt-troubleshooting.md`, between the `stt-helper:start-reference` markers.

This is the single-source-of-truth move applied to the operator surface: the metadata row is the narrow contract, and the menu is printed from it rather than retyped where it goes stale. The mechanism is load-bearing because it is enforced, not aspirational — a drift test (`test_operator_docs_start_reference_matches_helper_metadata`) regenerates the docs region from the metadata and fails if the checked-in text differs, and after any flag/default/env change we run `bash -n scripts/stt-helper.sh`, `source scripts/stt-helper.sh && stt help start`, and `source scripts/stt-helper.sh && stt help llm`. Hardcoding a flag, default, or env name back into any single surface would reintroduce the drift this decision exists to remove; CLAUDE.md's "STT Helper Flag Policy" is the standing rule, this ADR is its reason.

A metadata-driven helper is only as honest as the process state it reports, so lifecycle identity is single-sourced too (#133): start/status/stop go through a daemon-lifecycle adapter that resolves the real daemon PID from the bound listener port and refreshes `/tmp/parakeet-daemon.pid`, instead of trusting the `uv run` launcher PID. The cost is one indirection — a pipe-delimited row format and an adapter layer in place of inline flag handling — which is cheaper than the silent divergence it replaces. See ADR 0001 for the two-process split that gives the helper a Rust client and Python daemon to wire together in the first place.
