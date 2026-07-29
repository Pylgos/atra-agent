# Codex–Atra comparison on Terminal-Bench 2.1

This benchmark runs Codex and Atra on the same 20 Terminal-Bench 2.1 tasks with
`gpt-5.6-sol` and `medium` reasoning effort. The checked-in job uses one attempt
and one concurrent trial to keep the initial usage bounded.
[Terminal-Bench 2.1][tb21] and the [Harbor dataset][dataset] are the upstream
sources.

The task set contains only tasks marked `hard`. It is selected by cycling
through categories in alphabetical order and, within each category, taking the
task with the largest expert-time estimate first. This gives broad coverage
without replacing the selection with tasks that happen to favor either agent.
The dataset is pinned by content digest. Codex is pinned by CLI version.
`run.py` derives Atra's `agent_version` from the current Git commit and relevant
working-tree changes, overriding the checked-in YAML value for each run.

## Prerequisites

- Docker with the Compose plugin
- Python 3.12 or newer and `uv`
- Codex 0.146.0 authenticated with `codex login`
- A Nix-built and authenticated Atra CLI

From the repository root:

```bash
nix build .#atra --out-link result-atra
result-atra/bin/atra codex login
```

The Nix package includes the matching static Runner platform, so a separate
platform installation is not needed.

The Atra adapter keeps the Controller and its credentials on the host. It
uploads only the static Runner to Harbor's task container, then connects it over
`docker exec`. All task commands and patches therefore execute inside the same
container that Harbor verifies.

## Run

`run.py` pins Harbor 0.20.0, builds the current Atra source with Nix, checks
Docker and authentication, and gives each Harbor batch a unique name.

First validate both agents on one task:

```bash
./benchmarks/terminal_bench_2_1/run.py pilot
```

Then run the fixed 20-task campaign:

```bash
./benchmarks/terminal_bench_2_1/run.py full --campaign first-20
```

The full command creates 40 trials: 20 tasks × 2 agents × 1 attempt. An official
leaderboard-style estimate needs repeated attempts, but increasing
`n_attempts` should be deferred until the one-attempt comparison is useful and
stable.

### Shared incremental results

Results are append-only and shared across compatible campaigns. Repeating a
command—or starting another campaign with the same benchmark conditions—skips
every compatible task-agent pair that already reached the verifier, including
pairs with reward zero. Missing or interrupted pairs run in a new Harbor batch.
Failed pairs are held unless retrying them is explicitly requested:

```bash
./benchmarks/terminal_bench_2_1/run.py full --campaign first-20
./benchmarks/terminal_bench_2_1/run.py full --campaign first-20 --retry-errors
```

Compatibility includes the dataset digest, task, execution conditions, agent
version, model, and reasoning effort. A pilot result can therefore be reused by
a full campaign for the task they share. Use the explicit override only when a
fresh completed trial is actually wanted:

```bash
./benchmarks/terminal_bench_2_1/run.py pilot \
  --campaign quota-codex --agent codex --rerun-completed
```

Run the agents separately when attributing quota consumption:

```bash
./benchmarks/terminal_bench_2_1/run.py full \
  --campaign first-20 --agent codex
./benchmarks/terminal_bench_2_1/run.py full \
  --campaign first-20 --agent atra
```

Before calling a provider, the command displays the completed, held-error, and
pending counts plus the maximum number of new API trials. It asks for
confirmation unless `--yes` is supplied. Use `--dry-run` to inspect the plan
without building Atra, starting Docker containers, or calling a provider.

A campaign records its mode and task selection, but neither its name nor its
task list forms a cache boundary. Updating Atra therefore schedules the tasks
for the new Atra revision while reusing a compatible Codex baseline from any
campaign; a Codex update behaves symmetrically. Reports keep each agent revision
and setting in a separate row. Every failed attempt remains available for token
and quota accounting within its row.

## Report

The campaign command prints a report after each batch and writes `results.csv`
inside the campaign directory. It includes compatible attempts from all
campaigns, filtered to the current task selection. Reports count all attempts
for spend, but use the latest completed attempt for each task-agent pair when
calculating benchmark scores.

The reporter can also aggregate arbitrary Harbor job directories:

```bash
python3 benchmarks/terminal_bench_2_1/report.py \
  benchmarks/terminal_bench_2_1/jobs/atra-codex-terminal-bench-2-1 \
  --csv benchmarks/terminal_bench_2_1/results.csv
```

`input_tokens` includes cached input, matching Harbor's definition.
`uncached_input_tokens` is `input_tokens - cached_input_tokens`. Dollar cost is
not compared because subscription-backed Codex and Atra runs do not expose a
meaningful per-trial billed amount. The raw Atra event stream, Controller log,
and final output are retained in each trial's agent log directory.

Model request counts come from Atra's event metadata and Codex's rollout
`token_count` events. Quota reporting uses the Codex weekly-window
`used_percent` snapshots saved by both agents. The observed change excludes the
first request in each agent's measured range and has one percentage-point
resolution. A decreasing value or a changed window is reported as an unknown
reset instead of a consumption estimate.

Harbor redacts values from Codex's authentication JSON in saved artifacts. In
Harbor 0.20.0 this can replace JSON literals with a bare `[REDACTED]` token; the
reporter treats those redacted values as `null` while loading results.

[tb21]: https://github.com/harbor-framework/terminal-bench-2-1
[dataset]: https://hub.harborframework.com/datasets/terminal-bench/terminal-bench-2-1/6
