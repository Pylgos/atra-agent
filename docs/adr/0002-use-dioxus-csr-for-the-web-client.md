---
status: accepted
---

# Use Dioxus CSR for the Web Client

The Web Client will use the stable Dioxus 0.7 line as a client-rendered WASM application. SSR, hydration, Dioxus server functions, and Dioxus Desktop are deliberately excluded because this is a local application with no SEO requirement, and its native boundary is already the `atra-web` daemon.

## Consequences

- The release build is staged: build the Dioxus Web bundle first, then embed the generated bundle into the separate `atra-web` binary.
- `atra` remains free of Dioxus and HTTP server dependencies.
- pnpm manages the small Playwright smoke-test suite; Web Client CSS is a committed static asset with no Tailwind dependency.
- Agent Markdown is rendered as a safe subset: raw HTML is disabled, links require explicit user action, and tool or terminal output is escaped.

