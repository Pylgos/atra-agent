# Web Client UI Redesign

Status: Agreed design  
Date: 2026-08-16

## Purpose

Redesign the experimental Web Client around the active Thread rather than
presenting Controller protocol state as a collection of equally weighted
panels.

The redesign keeps the existing Web Client capabilities, but exposes them
progressively:

- the normal experience optimizes for understanding the conversation;
- active work shows detailed, structured progress;
- completed work collapses into readable summaries;
- exact domain events remain available through a separate Raw mode;
- secondary management operations remain reachable without permanently
  reducing the Transcript area.

This document records the agreed product and interaction design. It is not an
implementation status document.

## Product model

- The Web Client is **Thread-focused**, not an operations dashboard.
- The active Thread is the primary workspace.
- Cross-Workspace attention is handled by navigation status and an Activity
  Inbox rather than a permanently visible dashboard.
- Existing functionality remains available through progressive disclosure.
- Desktop is the primary environment.
- Mobile must still support the core workflow: conversation, approvals,
  structured questions, and turn cancellation.
- The UI is English-only for now. Do not introduce localization
  infrastructure.

## Application shell

The desktop shell has three regions:

```text
┌ Navigation sidebar ┬──────────── Main Thread ────────────┬ Utility panel ┐
│ Pinned roots        │ Context header                      │ Thread         │
│                     │                                     │ Children       │
│ Workspaces          │ Pretty Transcript / Raw Event list  │ Checkpoints    │
│  └ root Threads     │                                     │ Processes      │
│                     │ Sticky Composer                     │                │
└─────────────────────┴─────────────────────────────────────┴────────────────┘
```

The Main Thread remains the visual center. The side regions are supporting
tools, not equal columns.

### Context header

Use one sticky context header rather than separate global and Thread headers.
It contains:

- Navigation toggle;
- current Workspace and Thread name;
- connection and turn status;
- Pretty / Raw mode switch;
- Thread actions;
- Activity Inbox;
- Utility panel toggle.

Do not show provider, model, effort, paths, or other secondary metadata in the
header.

On mobile, keep the Thread name, turn status, Navigation toggle, Utility
toggle, and Activity badge visible. Put Pretty / Raw and Thread actions in an
overflow menu.

### Resizable side regions

- Navigation and Utility are independently resizable on desktop.
- Their widths and open states are browser-local display preferences.
- Store desktop widths separately from mobile drawer state.
- Double-clicking a resize handle resets that region to its default width.
- Preserve a minimum usable width for the Main Thread.
- Utility starts closed for a new browser profile.
- Remember the last Utility tab.
- Switching Threads preserves the Utility open state and selected tab.
- Switching Threads exits any Checkpoint preview and returns the center to the
  live Transcript.

## Navigation sidebar

Navigation has two vertically adaptive sections:

1. Pinned root Threads;
2. Workspace and root Thread tree.

Pinned uses its natural content height while space is available. When both
sections contain too much content, they shrink within the available sidebar
height and become independently scrollable. Keep a minimum usable height for
the Workspace tree. Do not impose an arbitrary Pin count limit.

### Pinned Threads

- Only top-level/root Threads can be pinned.
- A Pin is a shortcut. The same root Thread remains visible in its Workspace
  tree, with the Pin rendered differently enough to communicate that it is a
  shortcut.
- Pinned entries show the Thread name, factual status, child count, and a
  small Workspace label.
- New Pins are inserted at the beginning of the list.
- Activity on an existing Pin does not silently reorder it.
- Unpinning and pinning again inserts it at the beginning.
- Users can reorder Pins with a drag handle.
- Provide Move to top, Move up, and Move down menu actions for keyboard and
  touch operation.
- Persist Pin membership and order in browser local storage.

Automatically pin the root Thread after a successful user-initiated command
that targets that Thread or one of its descendants. Deleting a root removes
its Pin. Deleting a child does not remove the root Pin.

Creating a new root Thread immediately pins it. Forking a child does not pin
the child; if its root is not pinned, pin the root.

Pins belonging to a stopped Workspace remain visible as offline shortcuts and
retain their order.

### Workspace tree

- Sort Workspaces by display name, case-insensitively.
- Sort root Threads by descending/newest Thread ID.
- Do not add `last_activity_at` or other ordering state.
- Show only root Threads in Navigation.
- Show Thread name, factual status, and child count.
- Do not show provider, model, or effort in each row.
- Each Workspace has an independently collapsible section.
- Persist Workspace collapse state in the browser.
- Put New Thread in the corresponding Workspace header.

Selecting a Thread normally does not change Workspace collapse state.
Selecting a Pinned shortcut must not repeatedly expand and collapse the
Workspace tree. If a deep link selects an unpinned Thread that has no visible
navigation entry, expand its Workspace once so the current location can be
found.

### Initial selection and offline state

Without an explicit deep link:

1. restore the last selected Thread if it still exists;
2. otherwise select the first available Pin;
3. otherwise show the actionable landing state.

If the currently displayed Workspace stops, preserve the cached Transcript as
read-only and show an offline state. Do not automatically switch to another
Thread and do not add a special navigation call to action.

## Utility panel

Utility is a closable, tabbed supporting region:

1. Thread;
2. Children;
3. Checkpoints;
4. Processes.

Selecting an object that belongs to a tab opens Utility and activates the
required tab.

### Thread tab

Keep Thread-specific configuration in one place:

- display name;
- provider;
- model;
- reasoning effort;
- delegation information;
- current token/context/rate-limit diagnostics where additional detail is
  useful.

Separate Thread deletion into a Danger zone.

Global theme, browser notification, and display settings belong in a global
application menu, not in the Thread tab.

### Children tab

- Display the full root family tree for the currently selected Thread.
- Selecting a child keeps the same root family visible.
- Highlight the current Thread.
- Sort descendants by descending/newest Thread ID.
- Keep child Threads out of the Navigation sidebar and Pinned list.

### Checkpoints tab

- Show the Checkpoint list and metadata in Utility.
- Selecting a Checkpoint replaces the center Transcript with a read-only
  Checkpoint preview.
- The preview uses the full Main Thread width instead of nesting details in
  Utility.
- Keep Navigation and Utility visible.
- Replace the Composer with a preview bar explaining the mode and offering
  Return to live.
- Support Pretty and Raw rendering in the preview.
- Preserve separate preview scroll positions.

Checkpoint actions belong in the preview header:

- Fork is the primary, non-destructive action.
- Restore is a warning action.
- Rewind is a warning action.
- Thread deletion remains the destructive action.

Restore and Rewind are not irreversible: the Controller first saves the
current history as a Checkpoint. They still replace current history and
therefore require an application confirmation dialog that explains the
effect.

### Processes tab

- Group Processes by Runner.
- Put running Processes before terminal Processes.
- Sort terminal Processes by descending/newest Process ID.
- Allow multiple Process rows to be expanded simultaneously.
- A focused Process from a deep link is expanded and emphasized without
  closing other locally expanded Processes.
- Give each expanded output area a bounded height.
- Follow output only while its scroll position is near the bottom.
- Scrolling upward suspends following; returning to the bottom resumes it.
- Do not add a separate follow toggle.
- Do not add a Copy action.
- Show Stop only for a running Process.
- Do not interpret a non-zero exit code as semantic failure. Display the exit
  state factually.

## Mobile shell

Reuse Navigation and Utility as overlay drawers instead of stacking the
desktop regions vertically.

- A horizontal drag beginning in the central content opens a drawer.
- Dragging right opens Navigation.
- Dragging left opens Utility.
- Explicit header buttons remain available as the accessible and discoverable
  alternative.
- Do not begin the gesture from horizontally scrollable code, tables, Process
  output, form controls, selections, or other conflicting interaction
  regions.
- Only one drawer is open at a time.
- Tapping the backdrop or using the corresponding close action closes it.
- Drawer transitions are short and disabled under reduced-motion
  preferences.

## Transcript modes

The Transcript has two Thread-wide modes.

### Pretty

Pretty is the default and optimizes for understanding the conversation.
Protocol JSON must not leak into this mode as a fallback.

### Raw

Raw is a simple, exact domain-event view:

- render persisted `ThreadEvent` values plus current Active Turn items;
- do not show SSE snapshots or transport operations;
- show each event as formatted JSON in sequence order;
- do not add custom search or Copy controls initially;
- rely on browser Find, selection, and Copy;
- build the Raw DOM only when Raw is open;
- keep Raw mode as Thread-specific session state;
- return to Pretty after a full reload.

Pretty and Raw maintain independent scroll positions. Switching modes aligns
to the corresponding Turn/event when possible instead of always jumping to
the end.

## Pretty Transcript structure

Group events into Turns. A Turn begins with a User message and continues
through the corresponding Assistant completion or terminal outcome.

Render a Turn as an open document rather than a card or pair of chat bubbles:

```text
User prompt

Activity

Assistant document
```

- Give the User prompt a restrained background treatment.
- Render Assistant output as readable document content.
- Keep User, Activity, and Assistant on the same reading axis.
- Limit the reading column to approximately 50–65rem.
- Allow code and tables to scroll horizontally when needed.
- Separate Turns with spacing and subtle dividers, not nested card chrome.
- Do not display event sequence numbers or event type labels in normal prose.
- Thread events do not gain timestamps solely for UI display.

### Turn actions

Expose Turn actions on hover and keyboard focus:

- Copy prompt;
- Copy response;
- Fork from here;
- Rewind to here.

Fork requests a name in a validated application dialog. Rewind goes directly
to an impact-aware confirmation dialog; it does not open an intermediate
preview.

## Activity rendering

The active Turn shows detailed, structured progress. Completed Turns collapse
their Activity into a compact summary.

### Active behavior

- Automatically expand only the activity currently running.
- Collapse an activity when it reaches a terminal state, regardless of exit
  code.
- Do not keep an item expanded because the UI inferred that it failed.
- Preserve a user's explicit expansion state.
- When Assistant final-answer streaming begins, keep Activity above it in a
  compact form while the answer streams below.
- If the viewport is near the Transcript bottom, follow streaming updates.
- If the user scrolls away, stop following and show a Latest control.

### Completed behavior

- Collapse Activity to a one-line count summary such as tool/search/process
  counts.
- Expanding the summary reveals the meaningful activity rows.
- Coalesce token deltas and repeated updates; retain meaningful tool, search,
  process, skill, and reasoning units.
- Do not reduce the entire Turn to a single opaque event count.

### Commentary, reasoning, and todos

- Render `AssistantMessage(Commentary)` as live prose inside Activity.
- Do not retain Commentary as a second Assistant answer.
- Render available reasoning summaries as the current live activity.
- Collapse completed reasoning to a small `Reasoned` row.
- Keep the full reasoning event in Raw.
- Render Assistant todos as an Activity checklist.
- Collapse a completed checklist to a progress count while keeping it
  expandable.

### Tool activities

Pair `ToolCall` and `ToolResult` into one activity using `call_id` and stable
fallback matching where the protocol permits it.

Pretty tool details show structured fields such as:

- tool name;
- Runner;
- command;
- significant paths;
- search query;
- model-visible arguments;
- model-visible textual result;
- factual status;
- artifacts.

Keep the complete model-visible text in the DOM when the activity is
expanded. Bound the visual height and allow internal scrolling; do not
truncate to a tail. During active streaming, follow an inner output area only
while it is near its bottom.

When output masking applies, Pretty shows the projection currently visible to
the model. Raw continues to expose the exact domain event, including masking
fields.

Managed Process activity in the Transcript shows the start, factual state, and
a bounded output summary. Full Process monitoring belongs in the Processes
tab.

### Non-conversation events

Pretty hides context payloads, instructions, Runner declarations,
provider-specific model output, token-usage events, and rate-limit event
history.

Show only history-changing boundaries such as Compaction and Frozen Boundary
as small system markers. Put their full data in Raw.

If Pretty cannot interpret a supported domain event, show a small Unsupported
event marker with a switch-to-Raw action. Never dump JSON into Pretty and
never silently hide the existence of an unrendered event.

## Markdown and document rendering

- Render Assistant messages with explicit document typography.
- Define spacing and hierarchy for headings, paragraphs, lists, blockquotes,
  tables, inline code, and code blocks.
- Use horizontal scrolling for wide code and tables.
- Keep links sanitized and visibly distinguish external links.
- Use system sans-serif for UI and prose.
- Use system monospace only for code, commands, output, IDs, and JSON.
- Do not load an external Web font.

## Composer

The Composer is sticky at the bottom of the Main Thread.

- Auto-grow with content up to a bounded maximum height.
- Enter inserts a newline.
- Cmd/Ctrl+Enter sends.
- Keep the input editable during an Active Turn so the next prompt can be
  prepared.
- Disable only Send while sending is not allowed.
- Use the primary action position for Send while idle and Stop while active.
- Stop acts immediately and is available only through its button.
- Escape closes transient UI; it never cancels a Turn.
- Move Continue, Compact, and Create Checkpoint to the Thread actions menu.
- Enable actions according to Controller state instead of displaying
  knowingly invalid operations.

Persist drafts per Thread. Sending clears only the successfully sent draft.

### Sent-message history

- At the first line, Up enters older sent-message history.
- At the last line, Down moves toward newer history.
- Preserve and restore the draft that existed before entering history.
- Do not trigger history navigation during IME composition or while a text
  selection is active.
- Do not show a permanent Recall last button.

### Composer status line

Show one compact line below the Composer:

- provider/model;
- reasoning effort;
- latest token usage;
- context usage when derivable;
- rate-limit status.

Hide unavailable values. On narrow screens, remove lower-priority fields
before wrapping to another line.

## Approval and structured questions

Pending interactions are rendered at their causal position in the
Transcript.

- Keep the Composer and its draft visible and editable.
- Disable only Send while input is required.
- Show an attention bar near the Composer that scrolls to the pending form.
- After response, replace the active form with a read-only response summary.
- The current protocol permits at most one `PendingInteraction` per Active
  Turn.
- Other Threads' pending interactions are reached through Activity Inbox.

### Approval

- Show tool name, operation label, and significant arguments.
- Make complete arguments expandable.
- Present Allow and Deny with equal visual weight.
- Do not preselect, initially focus, or visually imply a default answer.
- Disable both actions while the response command is pending.

### Questions

- Show options as description-bearing radio cards.
- Mark recommended options explicitly.
- Include `None of these` as a peer option.
- Always provide an optional multiline note for every question, regardless of
  selected option.
- Render all questions in one form.
- Validate and submit the complete request in one command.

## Activity Inbox and notifications

Activity Inbox is a global chronology rather than a Workspace-grouped list.

Add an item only when the affected Thread is not currently visible and one of
these occurs:

- Approval required;
- Question required;
- `TurnOutcome::Failed`;
- `TurnOutcome::Completed`.

Do not add Cancelled outcomes. Do not duplicate events already observed in
the visible Thread.

Each item contains the Workspace, Thread, event type, short summary, and
browser-observed time.

- Opening the item selects its Thread and marks it read.
- Provide individual dismissal.
- Provide Clear all read.
- Never clear unread items through Clear all read.
- Keep unread items without an age limit.
- Keep at most the latest 100 read items.
- Store Inbox state in browser local storage.

Desktop opens Inbox as a header popover. Mobile uses a drawer with the same
content.

Browser notifications are explicit opt-in:

- request permission only when the user enables notifications;
- do not repeatedly request after denial;
- explain denied browser state in Global settings;
- use Android Web Push with a minimal Service Worker rather than requiring an
  installable PWA;
- notify for Approval, Question, Failed, and Completed transitions;
- provide a test-notification button beside the setting.

## Connection, loading, and feedback

- Use skeletons during initial loading.
- Distinguish loading from an empty collection.
- During reconnection, preserve existing content as read-only.
- Show connection state in the header and a non-destructive banner with the
  actual failure reason and Retry.
- Do not replace cached content with a generic error page.
- Use actionable empty states for no Workspaces, no Threads, no Checkpoints,
  no Processes, and no Inbox results.
- Each empty state exposes at most one primary next action.

Command feedback:

- show pending/disabled state at the action location;
- show brief success toasts;
- show failures inline at the originating control plus a toast summary;
- make Copy actions briefly change state rather than generating a full toast;
- clear stale errors after a later successful operation;
- prevent duplicate submission while a command is pending.

## Dialogs and action severity

Replace native `prompt()` and `confirm()` with accessible application dialogs.

- Rename and Fork use validated input dialogs.
- Stop, Rewind, Restore, and Delete use confirmation dialogs describing the
  target and effect.
- Fork is primary/non-destructive.
- Restore and Rewind are warning/history-replacing actions.
- Delete is danger/destructive.
- Do not color Process exit codes as success or failure based only on whether
  they are zero.

## URL and browser history

Encode meaningful viewed resources in the URL:

- Workspace;
- Thread;
- Checkpoint preview;
- focused Process.

Do not encode sidebar width, panel open state, selected Utility tab, expanded
Process set, scroll positions, or Pretty/Raw mode.

- Selecting a Checkpoint or focused Process pushes browser history.
- Back returns to the live Transcript or previous meaningful resource.
- Utility tab changes and panel open/close do not create history entries.
- A Process deep link focuses and expands one Process while preserving other
  browser-local expanded Processes.

## Visual language

- Use a quiet document/IDE visual language.
- Prefer flat surfaces, spacing, and subtle dividers over nested cards.
- Give conversation prose comfortable density.
- Keep Navigation, Utility, and Activity compact.
- Use system theme initially and allow System, Light, and Dark selection.
- Limit semantic colors to a small set such as attention, active, warning, and
  danger.
- Express status with an icon and factual text, not color alone.
- Use restrained transitions for drawers, collapse, and toast.
- Do not animate streaming text or force smooth scrolling.
- Disable nonessential motion under `prefers-reduced-motion`.

## Accessibility requirements

- Expose selected Workspace and Thread with the correct current/selected
  semantics.
- Give repeated actions contextual accessible names.
- Preserve visible focus indication.
- Move focus deliberately after meaningful route transitions and dialog
  actions.
- Do not place `aria-live` on the entire streaming Transcript.
- Announce concise turn, interaction, and connection state changes instead.
- Keep drawer and dialog focus trapped while open and restore focus on close.
- Provide button/menu alternatives to resize, drag, and swipe interactions.
- Ensure touch targets meet mobile sizing requirements.
- Update the document title from the selected Workspace and Thread.

## Rendering and performance

- Build Pretty details lazily when a collapsed Turn or activity is expanded.
- Keep full model-visible content in application state even when its detailed
  DOM is not mounted.
- Build Raw event DOM only while Raw mode is active.
- Do not virtualize Raw initially because browser Find and selection are part
  of its intended debugging workflow.
- Do not render Pretty and Raw simultaneously off-screen.
- Preserve Thread-specific Pretty and Raw scroll positions for the current
  browser session.

## Explicit non-goals

- A permanent multi-Workspace dashboard;
- pinning child Threads;
- Controller-owned Pin state;
- `last_activity_at` solely for navigation ordering;
- event timestamps solely for UI display;
- custom Raw search or Copy controls;
- Process output Copy or explicit follow toggles;
- semantic interpretation of Process exit codes;
- localization infrastructure;
- native browser prompt/confirm dialogs;
- feature parity through mobile vertical stacking;
- provider login or authentication UI.

## Implementation boundary

This agreement authorizes recording the design only. It does not authorize
implementation. Before changing the Web Client, present an implementation
plan that identifies:

- the new presentation model for Turn grouping and activity pairing;
- shell and routing changes;
- browser-local state and storage keys;
- protocol changes, if any are later found necessary;
- migration away from the current monolithic `main.rs`;
- test strategy for Pretty/Raw rendering, scroll behavior, interactions,
  responsive drawers, and route restoration.

Implementation begins only after explicit approval of that plan.
