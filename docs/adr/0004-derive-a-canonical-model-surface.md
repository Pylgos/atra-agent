---
status: accepted
---

# Derive a canonical Model Surface from Thread Events

The append-only Thread Event log is the only durable conversation record. Before invoking a model, the Controller derives an ordered, provider-independent Model Surface with one pure projection. Private API Adapters serialize that Surface into Responses, Chat Completions, or Messages requests and parse streams back into canonical events.

Provider-native response payloads are not a second source of truth. Replay-only data such as encrypted reasoning or signed thinking is attached to the relevant canonical block as opaque metadata with a namespaced replay key. An API Adapter reuses it only when the destination model declares the same key.

Provider implementations are concrete facades composed from private authentication, Model Catalog, API Adapter, and tool components. API differences remain in concrete adapters and typed profiles rather than a universal wire schema.

## Consequences

- A thread can change Provider or model without requiring provider-native history.
- Surface projection and API serialization must preserve semantic round trips for text, reasoning, tool calls, tool results, ordering, and phases.
- Compaction shadows a history prefix without rewriting the Thread Event log.
- Normal and compaction requests use the same projected and serialized prefix so compaction does not unnecessarily invalidate prompt caching.
- Provider-native output events are not retained through a compatibility parser or migration.
- Signed or encrypted provider state remains intentionally non-portable unless replay keys match.
- Adding an API shape requires a concrete API Adapter, not extensions to a universal request schema.
