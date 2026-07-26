---
name: setup-atra-workspace
description: Design and configure an Atra workspace from its Controller and Runner architecture. Use when Codex needs to create or review .config/atra.toml and .config/atra-setup.bash, choose a Runner topology, set Runner approval policy, or connect a host or remote execution environment.
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
- `atra workspace start` starts the workspace Controller, then runs the
  configured setup command from the workspace root.
- The setup command launches each Runner as a child process connected to the
  Controller through Atra's stdio protocol. Runners stop when the Controller
  stops.
- The Controller deploys available workspace skills and installed platform
  tools to each Runner.

Use Runner descriptions to expose meaningful execution boundaries to the
model, such as host, container, or remote machine. Do not create multiple
Runners when they execute in the same environment with the same policy.

## Workspace configuration

`.config/atra.toml` accepts exactly one field:

```toml
setup = "bash .config/atra-setup.bash"
```

`setup` is a shell command string. Atra executes it with Bash from the
workspace root and provides:

- `ATRA_BINARY`: path to the currently running Atra executable.
- `ATRA_CONTROLLER_ENDPOINT`: endpoint used by Runner launch commands.

Keep orchestration in the setup script rather than embedding a complex command
in TOML.

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

## Minimal host workspace

Use one host Runner unless isolation or remote execution is required:

```toml
# .config/atra.toml
setup = "bash .config/atra-setup.bash"
```

```bash
#!/usr/bin/env bash
set -euo pipefail

"${ATRA_BINARY:-atra}" runner launch \
  --name host \
  --description "Run commands in the workspace host environment" \
  --approval ask
```

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

Add this pattern only when the workspace actually needs a remote execution
environment.
