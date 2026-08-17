---
status: accepted
---

# Let a loopback proxy own remote Web security

The Web daemon continues to bind only to loopback and require its exact local Host and JSON command bodies, but it does not require the browser Origin to match that local authority. This allows a trusted loopback reverse proxy to preserve the browser's public Origin while normalizing Host; the proxy owns any remote TLS and authentication boundary, while the daemon still emits no permissive CORS headers.

## Consequences

- A reverse proxy that exposes the Web Client remotely must authenticate requests before forwarding them.
- Browser cross-origin command requests remain blocked by the JSON content type preflight unless the deployment deliberately adds permissive CORS handling.
- The exact-Origin command requirement recorded in ADR-0001 is superseded; its other decisions remain in force.
