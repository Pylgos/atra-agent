---
status: accepted
---

# Use a global local Web daemon

The Web Client will run through one user-scoped `atra-web` daemon that binds only to loopback, discovers all running Workspace Controllers, and keeps each Controller authoritative. The daemon will connect to Controllers through the existing local protocol, while browsers use resource-specific SSE subscriptions and JSON command requests; this avoids adding browser transport, discovery, or Web security concerns to the Controller and lets one Client switch among multiple running Workspaces.

## Consequences

- Only running Workspaces are discoverable; `atra-web` is not a persistent Workspace registry or launcher.
- Workspace startup writes a private runtime JSON sidecar containing its ID and canonical path. The daemon accepts a sidecar only while the corresponding Controller socket is reachable.
- Each SSE request owns an independent Controller subscription and starts with a fresh snapshot. The daemon stores no replay or browser session state.
- Browser commands are accepted only on loopback, from the daemon's own origin, with an exact Host and JSON content type. Remote access, TLS, login authentication, and CORS are out of scope.

