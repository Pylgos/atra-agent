# Subagents

Status: proposed design; not implemented.

## Goals

- Let a running thread create and coordinate normal child threads.
- Keep thread lifecycle, model execution, hierarchy, and persistence owned by
  the Controller.
- Let users open and operate child threads exactly like other threads.
- Reuse the Runner's managed-process waiting behavior so `agent wait` does not
  consume the invoking command's detach timer.
- Preserve useful child output without returning large tool results.
- Keep wait cursors valid when automatic compaction occurs during a child turn.

## Non-goals

- A separate persistent agent entity or agent ID.
- Copying or summarizing the parent history into a child.
- Coupling child lifetime to the spawning parent turn.
- Allowing detached managed processes to start model work after their tool call
  has ended.
- Legacy database migration or protocol compatibility.
- LLM-generated summaries of child output.

## Data model

An agent is a normal thread. `ThreadId` is its only identity.

Thread metadata gains an immutable optional `parent_thread_id`. A thread made by
`agent create` uses the invoking thread as its parent. A normal thread has no
parent.

The relationship is ownership, not provenance:

- A child may create children at any depth.
- Forking a child creates a sibling by copying the source
  `parent_thread_id`.
- Forking also clones every checkpoint reachable from copied
  `CompactionEvent` values into the new thread and rewrites those events to the
  cloned checkpoint IDs. Checkpoints remain thread-owned; a fork never retains
  a checkpoint reference into its source thread.
- Names are required for `agent create`, but are not unique. `ThreadId` remains
  canonical.
- A child may be renamed and otherwise operated through the normal thread UI.
- A child may receive any number of sequential turns.

No terminal agent state is stored. `completed`, `failed`, and `cancelled`
describe the latest turn in the current Controller lifetime. After a Controller
restart, inactive threads are reported as `idle`; messages and hierarchy remain
persistent.

## CLI

```text
atri agent create --name <name> [--model <provider>/<model>] [--effort <effort>]
atri agent send <thread-id> [<message>]
atri agent wait [--timeout <seconds>] <thread-id>[@<after-sequence>]...
atri agent list
atri agent cancel [--recursive] <thread-id>...
atri agent delete [--recursive] <thread-id>...
```

The commands are available only from a process belonging to an active command
tool call. They may target any descendant of the invoking thread, but not the
invoking thread itself, its ancestors, siblings, or unrelated threads.

### `create`

- Creates an empty child thread without starting a turn.
- `--name` is required; duplicate names are allowed.
- Without overrides, provider, model, and reasoning effort are inherited from
  the parent.
- `--model <provider>/<model>` and `--effort <effort>` independently override
  inherited values.
- Unknown providers, models, or unsupported efforts are errors.
- Success output is:

```text
thread_id=42
```

### `send`

- Starts one turn and returns as soon as the Controller accepts it.
- Rejects a thread that already has an active turn.
- The positional message is used when present.
- If the message is omitted, stdin is read through EOF when stdin is not a TTY.
  Omitting the message on a TTY is an error.
- Only turns started through `agent send` disable the question tool. A user turn
  started directly in the child UI keeps the normal question behavior.
- A failed first send leaves the empty child in place.
- Success output is:

```text
after_sequence=17
```

The returned value is the exclusive event cursor immediately before events
belonging to the accepted turn. Event sequences start at zero, while cursors
also admit the sentinel `-1`, meaning "before the first event". The first send
to a completely empty thread therefore returns:

```text
after_sequence=-1
```

### Concurrency limit

`agent send` allows at most eight active descendant turns under the topmost root
thread. The check and turn registration are atomic. Reaching the limit rejects
the new send without affecting existing turns.

Only `agent send` is subject to this limit. A user directly starting a turn in a
child through the TUI may exceed it.

### `wait`

- The default timeout is 120 seconds and there is no configured maximum.
- Multiple targets share one deadline.
- The active turn present at the start of the request is captured. A later turn
  on the same thread is not included.
- A target with no active turn returns immediately using its current/latest
  state.
- Targets are validated before waiting. One missing, malformed, or out-of-scope
  target fails the whole request without waiting.
- Results are emitted together in CLI argument order.
- One failed target does not cause fail-fast behavior; other targets continue
  until they finish or the shared timeout expires.
- A failed target makes the command exit non-zero. Timeout, cancellation, and
  awaiting input do not.
- Reaching an unresolved question returns that target immediately with
  `status=awaiting_question` so the user can answer it. This can occur when the
  user, rather than `agent send`, started the captured child turn. That target
  is then satisfied for this wait; other targets still follow the shared
  deadline.
- Approval waits continue until resolved or timed out.
- A timeout returns output collected so far, the current status, and a new
  cursor.
- Cancelling the invoking parent command interrupts its wait but does not cancel
  any child.

`THREAD` displays the latest turn, beginning at its latest user message.
`THREAD@SEQUENCE` displays events strictly after `SEQUENCE`.
`SEQUENCE` must be `-1` or a non-negative integer.

Each result header includes the inclusive last sequence observed as
`through=<sequence>`. It can be used for a later incremental wait:

```text
atri agent wait 42@17
```

Waiting on a completely empty thread returns immediately:

```text
== researcher thread=42 status=idle events=none through=-1 ==
```

Example result:

```text
== researcher thread=42 status=completed events=18..24 through=24 ==
[user]
Inspect the storage invariants.
[assistant/commentary]
I will trace the write path first.
[tool command]
runner=sandbox status=ok command="rg -n ..."
[assistant/final]
The invariant is enforced by the mutation lock.
```

Statuses exposed by agent reporting include `idle`, `running`, `compacting`,
`awaiting_question`, `awaiting_approval`, `cancelling`, `completed`, `failed`,
and `cancelled`.

### Deterministic output filtering

Wait output is constructed without another model call:

- User messages are emitted in full.
- Assistant commentary and final messages are emitted in full and retain their
  channel labels.
- Each tool is summarized in at most three lines and 512 characters.
- A tool summary retains its name, principal target or arguments, and detectable
  success or failure.
- Successful tool-result bodies are omitted.
- For detectable failures, only a short error tail is retained.
- Reasoning, raw model output, token usage, rate limits, instructions, skill
  synchronization, and runner synchronization are omitted without markers.

### `list`

Displays the invoking thread as the root of a tree followed by all descendants.
Each row includes name, `ThreadId`, model profile, and current/latest status.

### `cancel`

- Accepts multiple targets and reports results in argument order.
- Cancellation is asynchronous; use `wait` to observe completion.
- Cancelling a thread without an active turn is a successful no-op.
- `--recursive` snapshots the target subtree at invocation and sends a
  cancellation request to every active turn in that snapshot.
- Overlapping and duplicate targets are normalized so each thread is processed
  once.
- Child cancellation never follows automatically from parent cancellation.

### `delete`

- Accepts multiple targets.
- A target with children is rejected unless `--recursive` is present.
- Any active turn in a selected subtree rejects deletion. Cancellation and
  waiting must happen first.
- One recursive subtree is validated and deleted atomically.
- Separate top-level targets use partial-success semantics; one failure does not
  roll back successful disjoint targets.
- Duplicate and overlapping targets are normalized.

Normal thread deletion follows the same descendant rules.

## Controller and Runner responsibilities

The Controller owns:

- parent/descendant validation;
- model inheritance and validation;
- thread creation and deletion;
- active-turn registration and the root concurrency limit;
- child turn execution and cancellation;
- fixed-turn waiting and output selection;
- compaction-aware event traversal.

The Runner owns:

- associating a foreground process and its descendants with an opaque execution
  context supplied by the Controller;
- verifying that an `atri agent` request came from a live process in that
  context;
- forwarding the request to the Controller;
- pausing the invoking process timer for the duration of `agent wait`.

The Runner does not own agent state or make hierarchy decisions.

## Bidirectional Runner RPC

The existing Controller/Runner stdio transport becomes a bidirectional,
multiplexed protocol:

```text
Controller -> Runner: Request | CallbackResponse
Runner -> Controller: Response | CallbackRequest | CallbackCancel
```

Request IDs are scoped by direction. The Controller's Runner reader continues
to resolve ordinary pending responses while independently dispatching callback
requests and cancellations. Callback IDs are scoped to one Runner connection
and are never reused within that connection.

For a command operation:

1. The Controller creates an opaque execution context associated privately with
   the current thread and tool operation.
2. The Runner associates the context with the foreground process and spawned
   descendants.
3. `atri agent` sends its request to the existing Runner control socket.
4. The Runner resolves the calling process context and sends a callback request
   over stdio.
5. The Controller resolves the context to the invoking thread and performs the
   operation.
6. The Runner relays the callback response to `atri`.

The context expires when the command tool call ends. A detached managed process
retains no authority to issue later agent operations.

For `agent wait`, the Runner holds the same process wait guard used by managed
process waiting while the Controller callback is outstanding. This pauses the
active detach timer exactly for the RPC duration. No pause lease, explicit
resume request, Controller endpoint, or Controller token is exposed to the
command environment.

The Runner monitors both the local control connection and the invoking process
while a callback is outstanding. If either ends first, the Runner:

1. marks the callback cancelled locally;
2. releases its process wait guard;
3. sends `CallbackCancel` to the Controller.

Response and cancellation use first-terminal-wins semantics:

- If `CallbackResponse` wins, the Runner removes the pending callback and
  ignores a later local cancellation.
- If local cancellation wins, the Runner removes the pending callback, sends
  `CallbackCancel`, and ignores a later `CallbackResponse`.
- If the Controller receives `CallbackCancel` before callback completion, it
  cancels the callback task and emits no response.
- A cancellation for an already completed or unknown callback is an idempotent
  no-op.

When a Runner connection closes, the Controller cancels every outstanding
callback belonging to that connection. Callback cancellation stops only the
Controller-side operation; captured child turns continue.

## Compaction

Automatic compaction is part of a turn and is not a turn boundary. An agent
wait continues while the child is compacting. If the wait times out during
compaction, it returns `status=compacting` and all output available so far.

The current implementation resets event sequences and deletes preceding events
during compaction. That is incompatible with stable agent cursors and exact
message reporting. Compaction therefore gains checkpoint-linked monotonic
cursors:

1. Immediately before replacing history, create the existing checkpoint.
2. Record that checkpoint ID in the new `CompactionEvent`.
3. Delete active events as today, but assign the compaction event a sequence
   greater than the pre-compaction maximum instead of resetting to zero.
4. Continue subsequent event sequences monotonically.
5. When agent reporting crosses a compaction event, recursively read its linked
   checkpoint to recover qualifying user, assistant, and tool events.
6. Apply the normal deterministic filtering to the reconstructed stream.

Multiple compactions form a checkpoint chain. Failure to resolve a linked
checkpoint or an inconsistent sequence is an error; incomplete output is not
reported as successful.

Checkpoints remain owned by one thread. Forking history that contains a
`CompactionEvent` recursively clones its reachable checkpoint chain in the same
transaction as thread creation. Cloning proceeds oldest dependency first,
maintains an old-to-new checkpoint ID map, rewrites every copied compaction
payload to the mapped ID, and copies checkpoint metadata and events under the
new thread. A source checkpoint is cloned at most once per fork. Deleting the
source thread can therefore never invalidate compaction traversal in a fork.

Provider history and the normal compacted transcript continue using the active
event set. Checkpoint traversal is specific to operations that require
pre-compaction reporting.

Parent compaction cannot interrupt a currently blocked `agent wait`, because
the parent is executing a tool rather than issuing a model request. The parent
may compact before its next model request after wait returns; this has no effect
on child identity, hierarchy, or execution.

## TUI

`/thread` displays all workspace threads as a tree:

- Roots and siblings are ordered by descending creation order.
- The initial state has roots collapsed, except that ancestors of the current
  thread are expanded.
- Collapse state is local to the TUI session.
- Left collapses an expanded node or moves to its parent.
- Right expands a node.
- Enter selects the thread.
- A collapsed node shows aggregate counts for running, awaiting-question, and
  awaiting-approval descendants without automatically expanding.
- The normal chat header shows a root-to-current breadcrumb.

Deleting a thread with descendants asks for recursive confirmation and displays
the descendant count. After deleting the currently selected subtree, the TUI
selects the nearest surviving parent.

## Tool instructions

The existing command tool description gains a concise `atri agent` section
covering:

- create, send, wait, list, cancel, and delete syntax;
- the maximum of eight concurrent `agent send` turns per root;
- the lack of inherited parent conversation context;
- disabled questions for automated child turns;
- independent child lifetime;
- the recommendation to use `wait` cursors for incremental output.

A separate skill or dedicated model tool is not added.

## Failure and race semantics

- Agent operations first validate execution context and descendant scope.
- `wait` captures the active turn object and acquires an observation pin for
  every target before releasing validation locks.
- A later turn cannot replace the captured wait target.
- An observation pin lasts through event/checkpoint traversal and construction
  of the immutable callback response. It is released before transporting that
  response back to the Runner.
- When a wait reaches completion, question, or timeout, the Controller captures
  `through` and reconstructs all report events at or below it in one consistent
  store read snapshot. Concurrent appends or compaction by the captured turn
  are therefore observed wholly before or wholly after their transaction.
- While a thread is observation-pinned, deletion, rewind, restore, manual
  compaction, and registration of a later turn are rejected. The already
  captured active turn, including its automatic compaction, continues normally.
- Recursive deletion rejects a subtree containing any observation pin.
- Callback completion, timeout, error, or cancellation releases all observation
  pins through scoped cleanup.
- Concurrency-limit checking and active-turn registration are one atomic
  lifecycle operation.
- Recursive delete snapshots and validates a subtree under the Controller
  mutation lock before deleting it.
- Recursive cancel snapshots once but does not monitor for turns started later.
- Agent create and send remain separate operations; send failure never rolls
  back creation.
- Controller restart does not resume active turns or restore latest turn
  outcomes.

## Testing

Automated tests use fake providers and fake/local Runners only. They must not
call a real provider or recursively invoke Cargo.

Coverage includes:

- protocol serialization and concurrent bidirectional request routing;
- callback handling while an ordinary Runner request is pending;
- callback cancellation on local control disconnect, invoking-process exit, and
  Runner disconnect;
- response/cancellation races and idempotent late cancellation;
- exact timer pause duration for `agent wait`;
- execution-context expiry and descendant authorization;
- model and effort inheritance/override;
- the atomic root concurrency limit;
- fixed-turn wait behavior, shared timeout, ordering, and exit status;
- deterministic output filtering and size limits;
- cursor continuation across one and multiple compactions;
- missing/corrupt compaction checkpoint errors;
- forking compacted history, rewriting a multi-level checkpoint chain, deleting
  the source thread, and successfully traversing the forked chain afterward;
- empty-thread `after_sequence=-1`, `through=-1`, and `THREAD@-1` behavior;
- observation pins racing with turn completion, new send, rewind, and recursive
  deletion, including cleanup after callback cancellation;
- immediate return for unresolved questions while approvals continue waiting;
- fork-as-sibling and recursive cancellation/deletion;
- CLI parsing and stdin fallback;
- TUI tree flattening, collapse behavior, aggregate badges, breadcrumbs, and
  recursive-delete selection.

## Schema and compatibility

The thread schema gains `parent_thread_id`, and compaction payloads gain their
checkpoint link. No compatibility parser, version negotiation, aliases, or
legacy database migration is added. Existing workspace databases must be
recreated after the schema change.
