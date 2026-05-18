# Domain docs

Single-context repo. Skills should consume domain documentation as follows.

## Before exploring, read

- [`CONTEXT.md`](../../CONTEXT.md) at the repo root — the project glossary.
- [`docs/adr/`](../adr/) — architectural decision records. Read any ADR that touches the area being worked on.
- Canonical implementation docs already referenced from [`AGENTS.md`](../../AGENTS.md) — `docs/SPEC.md`, `docs/stt-troubleshooting.md`, `docs/engineering/harness-engineering-playbook.md`, `scripts/stt-helper.sh`.

## Use the glossary's vocabulary

When naming a domain concept (issue title, refactor proposal, hypothesis, test name), use the term as defined in `CONTEXT.md`. Don't drift to synonyms the glossary explicitly avoids.

If the concept isn't in the glossary yet, that's a signal: either the project doesn't use that language (reconsider), or there's a real gap (add the term — same discipline as `/grill-with-docs`).

## Flag ADR conflicts

If a proposal contradicts an existing ADR, surface it explicitly rather than silently overriding:

> _Contradicts ADR-0001 (two-process split) — but worth reopening because…_

## Layout

```
/
├── CONTEXT.md          ← project glossary
├── docs/
│   ├── adr/            ← architectural decisions (0001-…, 0002-…)
│   └── agents/         ← these files
└── ...
```

No `CONTEXT-MAP.md`; this is not a multi-context repo.
