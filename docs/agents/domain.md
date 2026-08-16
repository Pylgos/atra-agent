# Domain Docs

How the engineering skills should consume this repo's domain documentation when exploring the codebase.

## Before exploring, read these

- **`CONTEXT.md`** at the repo root.
- **`docs/adr/`** for ADRs that touch the area about to be changed.

If either location does not exist, proceed silently. Do not suggest creating domain documentation upfront; the domain-modeling skill creates it lazily when terms or decisions are resolved.

## File structure

This is a single-context repository:

```text
/
├── CONTEXT.md
└── docs/
    └── adr/
        ├── 0001-example.md
        └── 0002-example.md
```

## Use the glossary's vocabulary

When output names a domain concept in an issue title, proposal, hypothesis, or test name, use the term defined in `CONTEXT.md`. Do not drift to synonyms the glossary explicitly avoids.

If a needed concept is absent, reconsider whether it belongs to the project language or note the gap for domain modeling.

## Flag ADR conflicts

If output contradicts an existing ADR, surface the conflict explicitly rather than silently overriding it.
