---
name: setup-atra-workspace
description: Design and configure an Atra workspace from its Controller and Runner architecture. Use when Codex needs to create or review .config/atra.toml and a setup command, choose a Runner topology, set Runner approval policy, or connect a host or remote execution environment.
---

# Setup Atra Workspace

Derive the workspace configuration from Atra's execution model. Keep the topology
as small as the workspace requires.

## Architecture

- The **Controller** owns conversations, the agent loop, model providers,
  approvals, persistence, and Runner lifecycle.
- A **Runner** executes commands, managed processes, and patches. It does not
  own conversations or make approval decisions.
- The model selects a Runner for each tool call. A conversation is not bound to
  one Runner.
- `atra workspace start` starts the workspace Controller, then launches the
  built-in Runners and runs the configured setup command from the workspace root.
- The setup command launches each custom Runner as a child process connected to
  the Controller through Atra's stdio protocol. Runners stop when the Controller
  stops.
- The Controller deploys available workspace skills and installed platform
  tools to each Runner.

Use Runner descriptions to expose meaningful execution boundaries to the
model, such as host, container, or remote machine. Do not create multiple
Runners when they execute in the same environment with the same policy.

## Zero-configuration workspace

A workspace with no `.config/atra.toml` starts with two built-in Runners:

- `host`: runs commands directly in the workspace host environment and requires
  approval for each command.
- `sandbox`: runs commands in a Bubblewrap sandbox with the `standard` preset and
  does not require per-command approval.

The built-in `sandbox` makes only the workspace and a persistent sandbox HOME
writable by default, exposes the host filesystem read-only, and shares the host
network. It is a write boundary, not a security boundary against malicious code.

## Workspace configuration

`.config/atra.toml` accepts two optional fields:

```toml
builtin_runners = true
setup = "bash .config/atra-setup.bash"
```

- `builtin_runners` defaults to `true`. Set it to `false` to replace the
  built-in Runners entirely.
- `setup` is a shell command string. Atra executes it with Bash from the
  workspace root after the built-in Runners have started, and provides:
  - `ATRA_BINARY`: path to the currently running Atra executable.
  - `ATRA_CONTROLLER_ENDPOINT`: endpoint used by Runner launch commands.

Unknown fields are rejected. Keep orchestration in the setup script rather than
embedding a complex command in TOML.

## Runner launch options

Use:

```text
atra runner launch --name <NAME> --description <DESCRIPTION> \
  --approval <ask|allow> [-- <COMMAND>...]
```

All named options are required:

- `--name`: unique, non-empty Runner identity used in tool routing.
- `--description`: execution environment description shown to the model.
- `--approval ask`: require Controller approval for each routed tool call.
- `--approval allow`: execute routed tool calls without per-call approval.
- `COMMAND`: optional process that implements Atra's Runner stdio protocol.

When `COMMAND` is omitted, Atra launches its own equivalent of:

```text
<current-atra-binary> runner run --stdio
```

Use an explicit command only to cross an execution boundary, such as SSH or a
container runtime. The command's stdout must remain the Runner protocol stream.

Launching an already-running name does not start another process. It updates
that Runner's description and approval policy. If the previous process has
stopped, Atra removes it and launches a replacement.

## Custom sandbox Runner

To add a sandbox with a different preset or mount set, launch the sandbox
transport explicitly from the setup command:

```bash
"${ATRA_BINARY:-atra}" runner launch \
  --name sandbox \
  --description "Workspace sandbox" \
  --approval allow \
  -- \
  "$ATRA_BINARY" runner sandbox \
    --preset standard \
    --workspace "$PWD"
```

The sandbox transport accepts:

```text
atra runner sandbox
  [--preset standard|relaxed]
  [--workspace PATH]
  [--mount-ro PATH]...
  [--mount-rw PATH]...
  [--bwrap PATH]
  [--runner-binary PATH]
  [--bwrap-arg ARG]...
```

- `standard` hides the host HOME; `relaxed` exposes it read-only. Both keep the
  writable `$HOME` at `/home/atra`.
- `--mount-ro` and `--mount-rw` expose additional paths at their original
  absolute location.
- `--bwrap-arg` appends a raw Bubblewrap argument after Atra's generated
  arguments. It can weaken or replace the preset isolation, so avoid it in the
  built-in sandbox.
- `--runner-binary` overrides the Runner binary for development and debugging.

The sandbox inherits the launching process environment and overrides `HOME` and
the temporary directory variables. Remove confidential values from the
environment before launching the sandbox when confidentiality matters.

## Remote Runner pattern

Upload a compatible Runner binary through a command that can execute a remote
shell, then launch it through the same transport:

```bash
remote_runner="$(
  "${ATRA_BINARY:-atra}" runner upload -- ssh build-host
)"

"${ATRA_BINARY:-atra}" runner launch \
  --name build-host \
  --description "Run Linux build commands on build-host" \
  --approval ask \
  -- ssh build-host "$remote_runner" --stdio
```

`atra runner upload` accepts:

```text
atra runner upload [--runner-binary <PATH>] -- <COMMAND>...
```

- Omit `--runner-binary` to upload the Runner from the installed Atra platform
  bundle.
- Set it to upload a specific compatible Runner executable.
- Supply a command, such as `ssh build-host`, that accepts an appended `-c`
  shell script and streams stdin to the target.
- Capture the printed absolute remote path and use it in `runner launch`.

The remote Runner pattern is independent of the sandbox and remains available
alongside it.

## Platform tools

Run an installed platform tool without starting a workspace:

```text
atra platform exec TOOL [ARG...]
```

This is a low-level escape hatch for setup scripts that need a bundled tool,
such as Bubblewrap, directly.
