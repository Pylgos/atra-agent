# Issue tracker: Local Markdown

Issues and specs for this repo live as Markdown files in `.scratch/`.

## Conventions

- One feature per directory: `.scratch/<feature-slug>/`
- The spec is `.scratch/<feature-slug>/spec.md`
- Specs published by `to-spec` use `Status: ready-for-agent`
- Implementation issues are one file per ticket at `.scratch/<feature-slug>/issues/<NN>-<slug>.md`, numbered from `01`; never use a single combined tickets file
- Comments and conversation history append to the bottom of the file under a `## Comments` heading

## When a skill says "publish to the issue tracker"

Create a new file under `.scratch/<feature-slug>/`, creating the directory if needed.

## When a skill says "fetch the relevant ticket"

Read the file at the referenced path. The user will normally pass the path or issue number directly.

## Wayfinding operations

Used by `/wayfinder`. The map is a file with one child file per ticket.

- **Map**: `.scratch/<effort>/map.md` contains Notes, Decisions-so-far, and Fog.
- **Child ticket**: `.scratch/<effort>/issues/NN-<slug>.md`, numbered from `01`, contains the question. A `Type:` line records `research`, `prototype`, `grilling`, or `task`; a `Status:` line records `claimed` or `resolved`.
- **Blocking**: a `Blocked by: NN, NN` line near the top. A ticket is unblocked when every listed file is `resolved`.
- **Frontier**: scan `.scratch/<effort>/issues/` for files that are open, unblocked, and unclaimed; first by number wins.
- **Claim**: set `Status: claimed` and save before any work.
- **Resolve**: append the answer under an `## Answer` heading, set `Status: resolved`, then append a context pointer with a gist and link to the map's Decisions-so-far.
