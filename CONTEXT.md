# Atra

Atra is a personal coding-agent system whose Controller owns conversations and execution while user-facing Clients operate it.

## Language

**Controller**:
The authority that owns conversations, agent turns, approvals, persistence, and Runner lifecycles.
_Avoid_: Server, backend

**Runner**:
An execution environment owned by the Controller that performs commands, managed processes, and patches.
_Avoid_: Agent, worker

**Workspace**:
A project directory with its own Controller, conversations, and execution environments.
_Avoid_: Project, repository

**Client**:
A user-facing interface that observes and operates a Controller without owning conversation or execution state.
_Avoid_: Frontend

**TUI Client**:
The terminal-based Client.
_Avoid_: TUI

**Web Client**:
The browser-based Client that coexists with the TUI Client and supports the same user workflows.
_Avoid_: Web UI, dashboard

**Assistant Message**:
A model-authored message classified as either Commentary or Final Answer.

**Commentary**:
An Assistant Message emitted while the current turn still requires further model or tool work.

**Final Answer**:
An Assistant Message that completes the current turn.

**Provider**:
A concrete facade that binds authentication, a Model Catalog, an API Adapter, rate limits, and model invocation for one external model service.
_Avoid_: Provider trait hierarchy

**Model Surface**:
The ordered, model-visible conversation derived purely from the append-only Thread Event log. It is not persisted separately.
_Avoid_: Provider history, wire messages

**API Adapter**:
A private serializer and stream parser for one upstream model API shape, such as Responses, Chat Completions, or Messages.
_Avoid_: Universal model API

**Exact Reasoning Option**:
A model-specific reasoning setting accepted by that model's API. Changing models requires selecting one of the destination model's exact options.
_Avoid_: Generic reasoning effort

**Tool Binding**:
The model-specific realization of a logical tool, either hosted by the model API or exposed as a function and executed by Atra.
_Avoid_: Tool route
