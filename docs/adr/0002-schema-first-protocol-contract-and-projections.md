# Schema-first protocol contract with per-language projections

Endpoint defaults (host, port, paths, env vars) and wire message shapes used to live as
literals scattered across the shell helper, the Python daemon, the Rust client, and the
docs. Changing one — a port, a path, a new status field — meant editing every copy by
hand, and any missed copy diverged silently until something broke at runtime. We replace
that duplication with one canonical contract: `docs/protocol/runtime-interface.json` for
endpoints/paths/env, `docs/protocol/schema/messages.schema.json` for message shapes, and
`docs/protocol/fixtures/` for canonical wire examples.

Each language keeps a hand-written projection of that contract — `runtime_interface.rs`,
`runtime_interface.py`, `stt-runtime-interface.sh` — plus its own idiomatic codec
(`protocol.rs`, `messages.py`). This is deliberately not codegen: the projections and
codecs stay readable and native to each side. What makes "one source of truth" real
rather than aspirational is enforcement. A drift test compares all three projections
against the JSON contract, and conformance tests round-trip every fixture through both
codecs, validate fixtures against the schema, and cross-check that `StatusMessage`
properties match the field groups. If any projection or codec diverges from the canonical
contract, the build fails (#31, #63, #134).

`StatusMessage` doubles as the protocol-level Runtime Truth contract for the daemon
`/status` payload; its `x-runtime-truth-field-groups` metadata is the single owner of that
field set (see `docs/adr/0004-runtime-truth-single-owner-contract.md`).

The cost is that the projections and codecs are hand-maintained, so adding a field touches
the schema, both codecs, and a fixture (the flow in `docs/protocol/README.md`); the drift
and conformance tests are what keep that disciplined instead of fragile. This is a deep
module behind a narrow interface: the three JSON files are the small, stable contract, and
the codecs, projections, and tests are the machinery hiding the cross-language coordination
that ADR 0001's two-process split created. Flattening it back — re-hardcoding defaults and
message shapes per language — would reintroduce exactly the silent divergence this contract
exists to prevent.
