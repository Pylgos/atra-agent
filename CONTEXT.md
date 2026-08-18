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
