# Web Client implementation plan

## Status

Implemented on 2026-08-16.

## Goal

Add a browser-based Client that coexists with the TUI Client and can complete the same user workflows against any running local Workspace Controller. The interface is desktop-oriented but responsive, and every supported workflow remains usable on a narrow mobile viewport.

The Web Client is experimental until the complete parity checklist in this document passes.

## Scope

### Required workflows

- Discover and switch among running Workspaces.
- Create, select, rename, and delete Threads.
- Select model and reasoning effort.
- Send messages, observe streaming turns, cancel, continue, and compact.
- Allow or deny approvals and answer structured questions.
- Create and inspect checkpoints; fork, rewind, and restore.
- Inspect managed processes and stop them.
- Preserve per-Thread drafts, per-Workspace sent-message history, and display preferences in browser storage.
- Notify about approval and question waits across all running Workspaces.

Provider login is intentionally excluded and remains a CLI/TUI workflow.

### Non-goals

- Remote or LAN access.
- Starting or stopping Workspace Controllers.
- Persistently registering stopped Workspaces.
- Reproducing TUI slash commands or terminal layout.
- Sharing a transcript presentation model with the TUI.
- Offline command queuing.
- SSR, hydration, server functions, or Dioxus Desktop.
- Notifications after every Web Client page has closed.

## Architecture

```text
Browser tabs
  Dioxus 0.7 CSR application
        |
        | same-origin HTTP POST + resource-specific SSE
        v
atra-web daemon
  static asset server
  Workspace discovery
  browser transport adapter
        |
        | existing NDJSON local protocol over Unix sockets
        v
one or more Workspace Controllers
```

### Process boundary

Add a separate `atra-web` binary package. Running `atra-web`:

1. opens the existing daemon URL when the daemon is healthy;
2. otherwise starts a background daemon, waits for readiness, and opens the browser.

The same binary exposes `serve`, `status`, and `stop` subcommands. `serve` is the foreground entry point used by daemonization and tests. The default origin is proposed as `http://127.0.0.1:2872`; `--port` overrides it, and a collision is an error rather than an automatic fallback.

Store only daemon lifecycle data needed by `status`, `stop`, and `open` in the user's private runtime directory. Do not introduce a daemon database.

### Workspace discovery

When the existing Workspace startup path has resolved the canonical root and Workspace ID, atomically write a private JSON sidecar beside `controller.sock`:

```json
{
  "workspace_id": "0123456789abcdef",
  "path": "/canonical/workspace/path"
}
```

The filename and schema are private implementation details. The Web daemon scans the existing per-user Atra runtime directory, validates sidecar ownership and permissions, and includes a Workspace only when it can subscribe to the corresponding Controller. Stale directories and sidecars are ignored.

Display the root directory basename as the primary label and the canonical path as disambiguating text. Use the Workspace ID in URLs and API routes.

### Browser API

The browser API is private to the Web Client and is not a second public Controller protocol.

Initial route shape:

- `GET /api/workspaces/events`
- `GET /api/workspaces/{workspace_id}/controller/events`
- `GET /api/workspaces/{workspace_id}/threads/{thread_id}/events`
- `GET /api/workspaces/{workspace_id}/threads/{thread_id}/checkpoints/{checkpoint_id}/events`
- `GET /api/workspaces/{workspace_id}/runners/{runner}/processes/{process_id}/events`
- `POST /api/workspaces/{workspace_id}/commands`

Each resource SSE connection opens one matching `atra-client` subscription. It emits the existing snapshot message first and then existing operation messages. When EventSource reconnects, the daemon opens a new Controller subscription and sends a fresh snapshot; the browser replaces that resource state before applying later operations. Do not implement event replay buffers or browser sessions.

Commands use the existing `atra-protocol` command and response payloads as JSON. Each request opens a short-lived local protocol connection. HTTP status distinguishes malformed/forbidden/unavailable requests; accepted Controller responses retain their existing typed result.

### Security boundary

- Bind IPv4 loopback only by default.
- Serve the application and API from one origin.
- Validate the exact Host and Origin for state-changing requests.
- Do not emit permissive CORS headers.
- Accept commands only as `application/json`; reject form and text payloads.
- Add conservative request body and header limits.
- Never serve arbitrary Workspace files or Controller output paths.
- Escape tool and terminal output.
- Disable raw HTML in Markdown and sanitize generated links.
- Require a confirmation dialog for Thread deletion, rewind, restore, and process stop.

No authentication token is required because the local OS user is the trust boundary. Any future non-loopback bind requires a new security design and ADR.

## Web application design

### State ownership

- Controller, Thread, checkpoint, process, approval, question, and execution state come only from SSE snapshots and operations.
- Reuse `atra-protocol` state types and their operation-application logic in WASM.
- Keep URL navigation state in the URL.
- Keep only UI state in `localStorage`: per-Thread drafts, per-Workspace sent-message history, theme, collapsed sections, and notification preference.
- Do not copy conversation or execution state into browser persistence.
- During disconnection, show status, retain drafts, disable state-changing actions, and let EventSource reconnect.

The Web Client will build its own transcript view model directly from `ThreadState`. Do not extract a shared TUI/Web presentation crate initially; revisit only after both renderers demonstrate stable, genuinely shared invariants.

### Routes and layout

Use deep-linkable routes such as:

- `/`
- `/w/{workspace_id}`
- `/w/{workspace_id}/threads/{thread_id}`
- nested checkpoint and process views where direct links are useful

Desktop layout:

- left: Workspace switcher and Thread list;
- center: transcript and composer;
- right drawer: model, checkpoint, process, and detail views.

Narrow layout keeps every workflow but turns sidebars and drawers into routes or sheets. Use semantic HTML, visible focus, keyboard navigation, and touch-sized controls. Browser-native buttons, menus, dialogs, and forms replace slash commands; add shortcuts only for frequent actions such as send, cancel, search, and navigation.

### Transcript

Render completed and active turn content, tool calls and results, runner operations, todos, approvals, questions, compaction markers, errors, and cancellation states. Preserve append-only event order and never hide or reorder conversation events during normal operation.

Use a safe Markdown subset for model text. Raw HTML remains text. External links require an explicit click and open with safe opener isolation. Long tool and process output is collapsed or virtualized without changing the underlying event order.

### Notifications

Maintain Controller subscriptions for all discovered Workspaces so Thread status badges remain current. Notify when a Thread enters `AwaitingApproval` or `AwaitingQuestion`.

- Always show in-application Workspace and Thread badges.
- Browser notifications are opt-in from settings.
- Send them only while a Web Client page is connected.
- Include only Workspace name, Thread name, and the wait category.
- If permission is absent or denied, badges remain the fallback.
- Deduplicate notifications by Workspace, Thread, status transition, and active interaction identity where available.

Concurrent tabs and the TUI Client are allowed. The Controller response is authoritative if two Clients race to answer or mutate the same resource; stale Web actions surface the returned rejection and then converge through the subscription.

## Packaging and toolchain

- Pin the current stable Dioxus 0.7 release rather than a 0.8 prerelease.
- Add Playwright through pnpm with a committed lockfile; keep Web Client CSS as a committed static asset with no Tailwind dependency.
- Keep the Dioxus/WASM application separate from the native daemon target inside the Web package boundary.
- Production packaging is a two-stage build:
  1. build the Dioxus Web distribution;
  2. archive that distribution and compile it into `atra-web`.
- The native build must only consume generated assets; it must not recursively invoke Cargo.
- Development may serve a filesystem asset directory, but release artifacts must be self-contained.
- Extend Nix/CI packaging to build the WASM target, pnpm assets, `atra-web`, and its embedded archive reproducibly.
- Ship `atra` and `atra-web` together while keeping them separate executables.

The first implementation gate is a packaging spike proving that a clean checkout can reproducibly build and run the self-contained `atra-web` binary without checking generated Web artifacts into source control.

## Proposed code shape

- `crates/atra-web/`: native binary, daemon lifecycle, HTTP/SSE adapter, discovery, embedded asset serving.
- `crates/atra-web-ui/`: Dioxus CSR application compiled for `wasm32-unknown-unknown`.
- `web/`: pnpm workspace metadata and Playwright smoke tests.
- `tools/build-web-assets.*`: explicit asset pipeline used by local release builds and Nix; never called recursively by Cargo tests.
- `crates/atra-cli/src/workspace.rs`: write the private runtime Workspace sidecar during existing startup preparation.
- `Cargo.toml`, `flake.nix`, and packaging scripts: add the two Web packages, WASM/Node tooling, and the `atra-web` release artifact.

Keep HTTP route types, discovery metadata, asset-provider details, and daemon lifecycle internals private. Do not add compatibility aliases, version negotiation, tolerant parsers, or generic transport traits.

## Test strategy

### Rust tests

- Workspace sidecar creation, permissions, atomic replacement, and stale-sidecar rejection.
- Discovery across zero, one, and multiple live Controllers.
- Exact Host/Origin/content-type enforcement.
- SSE snapshot-first ordering and fresh-snapshot reconnection.
- Command forwarding and typed error mapping.
- Disconnect/read-only state transitions.
- Dioxus state reducers for every subscription operation.
- Transcript projection for active and completed tool, approval, question, compaction, failure, and cancellation events.
- `localStorage` key scoping and migration-free strict decoding.
- Concurrent-client races using fake Controllers; never use a real model provider.

### Browser smoke tests

Use a small Playwright suite against fake Controller fixtures:

- daemon startup and Workspace discovery;
- create/select a Thread and send a message;
- stream a turn and resolve an approval/question;
- reload during a stream and recover from a fresh snapshot;
- perform one checkpoint action and stop one process;
- verify narrow-viewport navigation;
- verify disconnected actions are disabled;
- verify notification opt-in behavior with mocked permission.

Do not recursively launch Cargo from integration tests.

## Delivery sequence

1. **Packaging gate** — prove Dioxus 0.7 CSR, pnpm, WASM, and embedded assets can produce a reproducible `atra-web` binary.
2. **Daemon foundation** — lifecycle commands, fixed loopback origin, static assets, security checks, readiness, and tests.
3. **Workspace discovery** — runtime sidecar, global discovery stream, Workspace switcher, and connection status.
4. **Read-only shell** — deep links, Controller/Thread subscriptions, reducers, responsive layout, and transcript rendering.
5. **Conversation loop** — composer, local drafts/history, model/reasoning selection, send, streaming, cancel, continue, and compact.
6. **Interactions** — approvals, questions, cross-Workspace badges, and opt-in browser notifications.
7. **History workflows** — Thread management and checkpoint create/view/fork/rewind/restore with destructive confirmations.
8. **Process workflows** — process list, output tail, stop, and responsive detail views.
9. **Hardening** — reconnection races, concurrent Clients, accessibility, output limits, browser smoke tests, Nix packaging, and parity review.

Each stage should remain a reviewable vertical slice. Keep the Web Client marked experimental until stage 9 passes.

## Acceptance criteria

- A single `atra-web` process lists every running Workspace with valid runtime metadata and no stopped Workspace.
- Multiple tabs and the TUI Client can operate the same or different Controllers and converge on Controller state.
- Every required workflow is usable at desktop and narrow widths.
- Reloading or reconnecting never requires daemon replay state and reconstructs each resource from a fresh snapshot.
- No state-changing endpoint accepts a cross-origin, non-JSON, or non-loopback request.
- Browser persistence contains no conversation, approval, question, checkpoint, process, or execution state.
- Provider login remains absent.
- The release `atra-web` executable serves all Web assets without an external asset directory.
- Automated tests never contact a real provider or recursively invoke Cargo.

## Confirmed implementation boundary

- The default origin is `http://127.0.0.1:2872`.
- Native daemon code lives in `atra-web`.
- The Dioxus WASM application lives in the private `atra-web-ui` crate.
- The only new executable shipped to users is `atra-web`.
