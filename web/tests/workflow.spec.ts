import { expect, test, type Page } from "@playwright/test";

async function installWebClientMocks(
  page: Page,
  snapshots: Record<string, unknown>
) {
  await page.addInitScript((snapshots) => {
    class FakeEventSource {
      static readonly CONNECTING = 0;
      static readonly OPEN = 1;
      static readonly CLOSED = 2;
      readonly CONNECTING = 0;
      readonly OPEN = 1;
      readonly CLOSED = 2;
      readyState = FakeEventSource.CONNECTING;
      onopen: ((event: Event) => void) | null = null;
      onmessage: ((event: MessageEvent) => void) | null = null;
      onerror: ((event: Event) => void) | null = null;
      readonly url: string;
      withCredentials = false;

      constructor(url: string | URL) {
        this.url = String(url);
        queueMicrotask(() => {
          this.readyState = FakeEventSource.OPEN;
          this.onopen?.(new Event("open"));
          const path = new URL(this.url, window.location.href).pathname;
          const snapshot = snapshots[path];
          if (snapshot !== undefined) {
            this.onmessage?.(new MessageEvent("message", {
              data: JSON.stringify(snapshot)
            }));
          }
        });
      }

      addEventListener() {}
      removeEventListener() {}
      dispatchEvent() { return true; }
      close() { this.readyState = FakeEventSource.CLOSED; }

      emit(message: unknown) {
        this.onmessage?.(new MessageEvent("message", {
          data: JSON.stringify(message)
        }));
      }
    }

    const sources = new Map<string, FakeEventSource>();
    Object.defineProperty(window, "EventSource", {
      configurable: true,
      value: class extends FakeEventSource {
        constructor(url: string | URL) {
          super(url);
          sources.set(new URL(this.url, window.location.href).pathname, this);
        }
      }
    });
    Object.defineProperty(window, "__atraEventSources", { value: sources });
    const notificationTitles: string[] = [];
    Object.defineProperty(window, "Notification", {
      configurable: true,
      value: class {
        static permission = "granted";
        static requestPermission() { return Promise.resolve("granted"); }
        constructor(title: string) { notificationTitles.push(title); }
      }
    });
    Object.defineProperty(window, "__atraNotificationTitles", {
      value: notificationTitles
    });
    localStorage.setItem("atra:notifications", "enabled");
  }, snapshots);
}

async function swipe(
  page: Page,
  from: { x: number; y: number },
  to: { x: number; y: number }
) {
  const client = await page.context().newCDPSession(page);
  await client.send("Emulation.setTouchEmulationEnabled", {
    enabled: true,
    maxTouchPoints: 1
  });
  await client.send("Input.dispatchTouchEvent", {
    type: "touchStart",
    touchPoints: [{ ...from, id: 1 }]
  });
  await client.send("Input.dispatchTouchEvent", {
    type: "touchMove",
    touchPoints: [{ ...to, id: 1 }]
  });
  await client.send("Input.dispatchTouchEvent", {
    type: "touchEnd",
    touchPoints: []
  });
  await page.waitForTimeout(50);
  await client.send("Emulation.setTouchEmulationEnabled", { enabled: false });
  await client.detach();
}

test("critical Thread workflow uses streamed snapshots and forwards commands", async ({ page }) => {
  const pageErrors: Error[] = [];
  page.on("pageerror", (error) => pageErrors.push(error));
  const workspace = {
    workspace_id: "workspace-1",
    name: "workspace",
    path: "/tmp/workspace"
  };
  const thread = {
    id: 1,
    parent_thread_id: null,
    display_name: "Web Thread",
    provider: "fake",
    model: "test-model",
    reasoning_effort: "medium"
  };
  const messages: Record<string, unknown> = {
    "/api/workspaces/events": {
      workspaces: [workspace]
    },
    "/api/workspaces/workspace-1/controller/events": {
      message: "snapshot",
      state: {
        lifecycle: "running",
        threads: [thread],
        thread_statuses: [{ thread_id: 1, status: "idle" }],
        providers: [{
          id: "fake",
          lifecycle: { status: "logged_in", account: null },
          models: [{
            provider: "fake",
            id: "test-model",
            display_name: "Test Model",
            description: "No-provider test model",
            default_reasoning_effort: "medium",
            supported_reasoning_efforts: ["low", "medium", "high"]
          }, {
            provider: "fake",
            id: "alternate-model",
            display_name: "Alternate Model",
            description: "Model with a different reasoning effort",
            default_reasoning_effort: "high",
            supported_reasoning_efforts: ["high"]
          }],
          rate_limits: null
        }],
        runners: []
      }
    },
    "/api/workspaces/workspace-1/threads/1/events": {
      message: "snapshot",
      state: {
        metadata: thread,
        events: [
          { sequence: 1, kind: "user_message", payload: { content: "Existing prompt" } },
          {
            sequence: 2,
            kind: "model_request",
            payload: {
              kind: "response",
              context_window: 128000
            }
          },
          {
            sequence: 3,
            kind: "assistant_message",
            payload: {
              content: "Checking the existing result.",
              phase: "commentary",
              todos: [
                { step: "Inspect the result", status: "completed" },
                { step: "Draft the response", status: "in_progress" }
              ]
            }
          },
          {
            sequence: 4,
            kind: "assistant_message",
            payload: {
              content: "# Result\n<script>bad()</script>\n\n**Safe answer**\n\n```bash\necho hello\n```\n\n- one\n  - nested\n- [x] done",
              phase: "final_answer"
            }
          },
          {
            sequence: 5,
            kind: "token_usage",
            payload: {
              request_sequence: 2,
              usage: {
                input_tokens: 6567,
                cached_input_tokens: 3456,
                output_tokens: 17,
                total_tokens: 6584
              }
            }
          },
          {
            sequence: 6,
            kind: "rate_limits",
            payload: {
              request_sequence: 2,
              snapshots: [
                {
                  limit_id: "codex",
                  primary: {
                    used_percent: 58,
                    window_minutes: 300,
                    resets_at: 2000000000
                  },
                  secondary: {
                    used_percent: 0,
                    window_minutes: 10080,
                    resets_at: 2000000000
                  },
                  credits: { balance: 0 },
                  plan_type: "pro"
                },
                {
                  limit_id: "GPT-5.3-Codex-Spark",
                  primary: {
                    used_percent: 0,
                    window_minutes: 10080,
                    resets_at: 2000000000
                  }
                }
              ]
            }
          }
        ],
        active_turn: null,
        last_outcome: null,
        checkpoints: [],
        processes: []
      }
    },
    "/api/workspaces/workspace-1/threads/2/events": {
      message: "snapshot",
      state: {
        metadata: {
          id: 2,
          parent_thread_id: null,
          display_name: "Background Thread",
          provider: "fake",
          model: "test-model",
          reasoning_effort: "medium"
        },
        events: [],
        active_turn: null,
        last_outcome: null,
        checkpoints: [],
        processes: []
      }
    }
  };

  await installWebClientMocks(page, messages);

  let sentCommand: unknown;
  await page.route("**/api/workspaces/workspace-1/commands", async (route) => {
    sentCommand = route.request().postDataJSON();
    if ((sentCommand as { method?: string }).method === "thread_send") {
      await page.evaluate(() => {
        const source = (window as any).__atraEventSources.get(
          "/api/workspaces/workspace-1/threads/1/events"
        );
        source.emit({
          message: "operation",
          operation: {
            operation: "event_appended",
            event: {
              sequence: 7,
              kind: "user_message",
              payload: { content: "Sent from browser" }
            }
          }
        });
        source.emit({
          message: "operation",
          operation: {
            operation: "active_turn_started",
            phase: "running"
          }
        });
      });
    }
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({ result: "accepted" })
    });
  });

  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Choose a Workspace" })).toBeVisible();

  await swipe(page, { x: 30, y: 420 }, { x: 150, y: 425 });
  await expect(page.locator(".navigation")).toHaveClass(/drawer-open/);
  const initialThreadLink = page.locator(".workspace-thread-row .navigation-link");
  const initialThreadLinkBox = await initialThreadLink.boundingBox();
  expect(initialThreadLinkBox).not.toBeNull();
  await swipe(
    page,
    {
      x: initialThreadLinkBox!.x + initialThreadLinkBox!.width - 20,
      y: initialThreadLinkBox!.y + initialThreadLinkBox!.height / 2
    },
    {
      x: initialThreadLinkBox!.x + initialThreadLinkBox!.width - 160,
      y: initialThreadLinkBox!.y + initialThreadLinkBox!.height / 2 + 5
    }
  );
  await expect(page.locator(".navigation")).not.toHaveClass(/drawer-open/);
  await expect(page.getByRole("heading", { name: "Choose a Workspace" })).toBeVisible();

  await page.getByRole("button", { name: "Open navigation" }).click();
  await expect(page.locator(".navigation")).toHaveClass(/drawer-open/);
  await page.locator(".workspace-thread-row .navigation-link").click();
  await expect(page.locator(".navigation")).not.toHaveClass(/drawer-open/);
  await expect(page.getByText("Existing prompt")).toBeVisible();

  await page.setViewportSize({ width: 1280, height: 720 });
  const workspaceRow = page.locator(".workspace-thread-row").filter({ hasText: "Web Thread" });
  const workspaceRowBox = await workspaceRow.boundingBox();
  const workspaceLinkBox = await workspaceRow.locator(".navigation-link").boundingBox();
  expect(workspaceRowBox).not.toBeNull();
  expect(workspaceLinkBox).not.toBeNull();
  expect(workspaceLinkBox!.width).toBeGreaterThan(workspaceRowBox!.width - 50);
  await expect(workspaceRow).not.toContainText("Idle");
  await expect(workspaceRow).not.toContainText("children");
  await expect(workspaceRow.locator(".thread-status-indicator")).toHaveCount(0);

  const modelSelector = page.getByLabel("Provider and model");
  const reasoningSelector = page.getByLabel("Reasoning effort");
  await expect(modelSelector).toHaveValue("fake\ntest-model");
  await expect(reasoningSelector).toHaveValue("medium");
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/controller/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "provider_updated",
        provider: {
          id: "fake",
          lifecycle: { status: "refreshing" },
          models: [],
          rate_limits: null
        }
      }
    });
  });
  await expect(modelSelector).toBeVisible();
  await expect(modelSelector).toBeDisabled();
  await expect(modelSelector).toHaveValue("fake\ntest-model");
  await expect(page.getByText("Model options are temporarily unavailable.")).toBeVisible();
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/controller/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "provider_updated",
        provider: {
          id: "fake",
          lifecycle: { status: "logged_in", account: null },
          models: [{
            provider: "fake",
            id: "test-model",
            display_name: "Test Model",
            description: "No-provider test model",
            default_reasoning_effort: "medium",
            supported_reasoning_efforts: ["low", "medium", "high"]
          }, {
            provider: "fake",
            id: "alternate-model",
            display_name: "Alternate Model",
            description: "Model with a different reasoning effort",
            default_reasoning_effort: "high",
            supported_reasoning_efforts: ["high"]
          }],
          rate_limits: null
        }
      }
    });
  });
  await expect(modelSelector).toBeEnabled();
  await modelSelector.selectOption("fake\nalternate-model");
  await expect(reasoningSelector).toHaveValue("high");

  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/controller/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "thread_status_updated",
        thread_id: 1,
        status: "running"
      }
    });
  });
  await expect(workspaceRow.getByRole("img", { name: "Running" })).toBeVisible();
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/controller/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "thread_status_updated",
        thread_id: 1,
        status: "completed"
      }
    });
  });
  await expect(workspaceRow.locator(".thread-status-indicator")).toHaveCount(0);
  await expect.poll(() =>
    page.evaluate(() => (window as any).__atraNotificationTitles)
  ).toEqual(["workspace · Web Thread · Turn completed"]);

  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/controller/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "thread_added",
        thread: {
          id: 2,
          parent_thread_id: null,
          display_name: "Background Thread",
          provider: "fake",
          model: "test-model",
          reasoning_effort: "medium"
        }
      }
    });
    source.emit({
      message: "operation",
      operation: {
        operation: "thread_status_updated",
        thread_id: 2,
        status: "completed"
      }
    });
  });
  const backgroundRow = page.locator(".workspace-thread-row").filter({ hasText: "Background Thread" });
  await expect(backgroundRow.getByRole("img", { name: "Completed" })).toBeVisible();
  await expect.poll(() =>
    page.evaluate(() => (window as any).__atraNotificationTitles)
  ).toEqual([
    "workspace · Web Thread · Turn completed",
    "workspace · Background Thread · Turn completed"
  ]);
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/controller/events"
    );
    for (const status of ["awaiting_approval", "awaiting_question", "failed", "idle"]) {
      source.emit({
        message: "operation",
        operation: {
          operation: "thread_status_updated",
          thread_id: 2,
          status
        }
      });
    }
  });
  await expect.poll(() =>
    page.evaluate(() => (window as any).__atraNotificationTitles)
  ).toEqual([
    "workspace · Web Thread · Turn completed",
    "workspace · Background Thread · Turn completed",
    "workspace · Background Thread · Approval required",
    "workspace · Background Thread · Questions require answers",
    "workspace · Background Thread · Turn failed"
  ]);
  await backgroundRow.locator(".navigation-link").click();
  await expect(modelSelector).toHaveValue("fake\ntest-model");
  await expect(reasoningSelector).toHaveValue("medium");
  await expect(backgroundRow.locator(".thread-status-indicator")).toHaveCount(0);
  await workspaceRow.locator(".navigation-link").click();

  await page.getByRole("button", { name: "Pin Web Thread" }).click();
  const pinnedRow = page.locator(".pinned-section .pin-row");
  await expect(pinnedRow).toBeVisible();
  await expect(pinnedRow).toHaveAttribute("draggable", "true");
  await expect(pinnedRow.locator(".drag-handle")).toHaveCount(0);
  await expect(pinnedRow.locator(".row-menu")).toHaveCount(0);
  await expect(pinnedRow).not.toContainText("Completed");
  await expect(pinnedRow).not.toContainText("children");
  await pinnedRow.getByRole("button", { name: "Unpin Web Thread" }).click();
  await expect(pinnedRow).toHaveCount(0);
  await expect(page.getByLabel(/Activity Inbox/)).toHaveCount(0);

  await workspaceRow.locator(".navigation-link").click();

  await expect(page.getByText("Existing prompt")).toBeVisible();
  await expect(page.getByRole("heading", { name: "Result" })).toBeVisible();
  await expect(page.locator("#transcript script")).toHaveCount(0);
  await expect(page.getByText("<script>bad()</script>")).toBeVisible();
  await expect(page.getByRole("complementary", { name: "Utility panel" })).toBeVisible();
  await expect(page.getByRole("tab", { name: "Thread" })).toHaveAttribute("aria-selected", "true");
  const composerStatus = page.locator(".composer-status");
  await expect(composerStatus.getByText("test-model (medium)", { exact: true })).toBeVisible();
  await expect(composerStatus.getByText("context 5%", { exact: true })).toBeVisible();
  await expect(composerStatus.getByText("cache 53%", { exact: true })).toBeVisible();
  await expect(composerStatus.getByText("5h 42%", { exact: true })).toBeVisible();
  await expect(composerStatus.getByText("weekly 100%", { exact: true })).toBeVisible();
  await expect(composerStatus).not.toContainText("Tokens");
  await expect(composerStatus).not.toContainText("Spark");
  await expect(composerStatus).not.toContainText("Credits");
  const utility = page.getByRole("complementary", { name: "Utility panel" });
  await expect(utility.getByText("Tokens 6,584 · in 6,567 · out 17")).toBeVisible();
  await expect(utility.getByText("Context 6,567 / 128,000 (5.1%)")).toBeVisible();
  await expect(utility.getByText("Cache 3,456 / 6,567 (52.6%)")).toBeVisible();
  await expect(page.locator(".turn")).toHaveCount(1);
  await expect(page.locator(".speaker-label", { hasText: "You" })).toBeVisible();
  await expect(page.locator(".speaker-label", { hasText: "Atra" })).toBeVisible();
  await expect(page.locator(".assistant-message ul li")).toHaveCount(3);
  await expect(page.locator(".assistant-message .highlighted")).toHaveCount(1);
  await expect(page.locator(".assistant-message .highlighted")).toContainText("echo hello");
  await expect(page.getByRole("button", { name: /Load .* previous turns/ })).toHaveCount(0);
  await expect(page.locator(".activity-list")).toHaveCount(0);
  await expect(page.locator(".activity-group-compact")).toContainText("1 update · 1 todo");
  await expect(page.locator(".activity-group-compact")).not.toContainText("Activity");
  await expect(page.locator(".activity-group-compact")).not.toContainText("Todo");
  await page.locator(".activity-group-compact").click();
  await expect(page.locator(".activity-commentary")).toContainText("Checking the existing result.");
  await page.locator(".activity-todo.collapsible-compact").click();
  await expect(page.getByRole("tab", { name: "Activity" })).toHaveAttribute("aria-selected", "true");
  await expect(page.locator(".utility .activity-todo li")).toHaveCount(2);
  await expect(page.locator(".utility .activity-todo li.completed")).toContainText("Inspect the result");
  await expect(page.locator(".utility .activity-todo li.in-progress")).toContainText("Draft the response");
  await page.getByRole("button", { name: "Collapse activities" }).click();
  await expect(page.locator(".activity-commentary")).toHaveCount(0);
  await page.getByRole("button", { name: "Raw", exact: true }).click();
  await expect(page.locator(".raw-events")).toContainText('"kind": "user_message"');
  await page.getByRole("button", { name: "Pretty", exact: true }).click();

  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/threads/1/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "active_turn_started",
        phase: "running"
      }
    });
  });
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/threads/1/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "active_item_added",
        item: {
          id: 20,
          data: {
            kind: "tool_call",
            item_id: "command-item",
            call_id: null,
            name: "command",
            input: "*** Runner sandbox\nset -e\necho hello"
          }
        }
      }
    });
  });
  await expect(
    page.locator(".activity-command:has(.command-row-status .status-dots)")
  ).toBeVisible();
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/threads/1/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "active_item_finalized",
        active_id: 20,
        event: {
          sequence: 8,
          kind: "tool_call",
          payload: {
            type: "custom",
            item_id: "command-item",
            call_id: "command-call",
            name: "command",
            input: "*** Runner sandbox\nset -e\necho hello"
          }
        }
      }
    });
    source.emit({
      message: "operation",
      operation: {
        operation: "active_item_added",
        item: {
          id: 21,
          data: {
            kind: "runner_tool",
            call_id: "command-call",
            operation_index: 1,
            runner: "sandbox",
            update: {
              kind: "command_output",
              content: "hello\n",
              omitted_bytes: 0,
              timer: { elapsed_ms: 12, remaining_ms: 0, paused: false }
            }
          }
        }
      }
    });
  });
  await expect(
    page.locator(".activity-command:has(.command-row-status .status-spinner)")
  ).toBeVisible();
  await page.locator(".activity-command:has(.command-row-status .status-spinner)").click();
  await expect(page.locator(".utility .command-source")).toContainText("echo hello");
  await expect(page.locator(".utility .command-operation header code")).toHaveText("sandbox");
  await expect(page.locator(".utility .command-output")).toContainText("hello");
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/threads/1/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "active_runner_output_appended",
        id: 21,
        content: "world\n",
        omitted_bytes: 0,
        timer: { elapsed_ms: 24, remaining_ms: 0, paused: false }
      }
    });
  });
  await expect(page.locator(".utility .command-output")).toContainText("hello\nworld");
  await expect(page.locator(".utility .command-operation header span")).toContainText("elapsed");
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/threads/1/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "active_runner_updated",
        id: 21,
        update: {
          kind: "completed",
          artifact: {
            kind: "runner_operation",
            data: {
              operation: 1,
              runner: "sandbox",
              label: "Command",
              result: "hello\nworld\n",
              artifacts: []
            }
          }
        }
      }
    });
  });
  await expect(page.locator(".utility .command-operation header span")).toHaveText("finished");
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/threads/1/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "tool_result_finalized",
        event: {
          sequence: 9,
          kind: "tool_result",
          payload: {
            type: "custom",
            name: "command",
            call_id: "command-call",
            result: "hello\nworld\n",
            artifacts: [{
              kind: "runner_operation",
              data: {
                operation: 1,
                runner: "sandbox",
                label: "Command",
                result: "persisted\n",
                artifacts: []
              }
            }]
          }
        },
        runner_ids: [21]
      }
    });
  });
  await expect(page.locator(".utility .command-operation header span")).toHaveText("finished");
  await expect(page.locator(".utility .command-output")).toContainText("persisted");
  await expect(page.locator(".utility .command-operation header span")).not.toHaveText("queued");
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/threads/1/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "active_item_added",
        item: {
          id: 9,
          data: {
            kind: "assistant",
            content: "Streaming commentary",
            phase: "commentary"
          }
        }
      }
    });
  });
  const currentTurn = page.locator(".turn").last();
  const streamingCommentary = currentTurn.locator(".activity-commentary", {
    hasText: "Streaming commentary"
  });
  await expect(currentTurn.locator(".assistant-message")).not.toContainText("Streaming commentary");
  await expect(streamingCommentary).toContainText("Streaming commentary");
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/threads/1/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "active_assistant_appended",
        id: 9,
        content: " update",
        phase: "commentary"
      }
    });
  });
  await expect(streamingCommentary).toContainText("Streaming commentary update");
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/threads/1/events"
    );
    source.emit({
      message: "operation",
      operation: { operation: "active_item_discarded", id: 9 }
    });
    source.emit({
      message: "operation",
      operation: {
        operation: "active_item_added",
        item: {
          id: 10,
          data: {
            kind: "assistant",
            content: "Streaming answer",
            phase: "final_answer"
          }
        }
      }
    });
  });
  const streamingAnswer = page.locator(".assistant-message").last();
  await expect(streamingAnswer).toContainText("Streaming answer");

  await page.locator(".turn-card").last().evaluate((element) => {
    (element as HTMLElement).style.minHeight = "1600px";
  });
  await page.locator("#transcript-scroll").evaluate((element) => {
    element.scrollTop = element.scrollHeight;
    element.dispatchEvent(new Event("scroll"));
    element.scrollTop =
      element.scrollHeight - element.clientHeight - 20;
    element.dispatchEvent(new Event("scroll"));
  });
  const scrollTopBeforeUpdate = await page
    .locator("#transcript-scroll")
    .evaluate((element) => element.scrollTop);
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/threads/1/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "active_assistant_appended",
        id: 10,
        content: " again",
        phase: "final_answer"
      }
    });
  });
  const latestButton = page.getByRole("button", { name: "Latest" });
  await expect(latestButton).toBeVisible();
  const [latestBox, transcriptBox, composerBox] = await Promise.all([
    latestButton.boundingBox(),
    page.locator(".transcript-region").boundingBox(),
    page.locator(".composer-region").boundingBox()
  ]);
  expect(latestBox).not.toBeNull();
  expect(transcriptBox).not.toBeNull();
  expect(composerBox).not.toBeNull();
  expect(latestBox!.x + latestBox!.width / 2).toBeCloseTo(
    transcriptBox!.x + transcriptBox!.width / 2,
    0
  );
  expect(latestBox!.y + latestBox!.height).toBeLessThan(composerBox!.y);
  await expect.poll(() =>
    page.locator("#transcript-scroll").evaluate((element) => element.scrollTop)
  ).toBe(scrollTopBeforeUpdate);
  await latestButton.click();
  await expect.poll(() => page.locator("#transcript-scroll").evaluate((element) =>
    element.scrollHeight - element.scrollTop - element.clientHeight
  )).toBeLessThanOrEqual(1);
  await expect(page.getByText("Streaming answer again", { exact: true })).toBeVisible();
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/threads/1/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "turn_finished",
        outcome: { outcome: "cancelled" }
      }
    });
  });
  await expect(page.getByRole("button", { name: /Assistant response/ })).toHaveCount(0);

  await page.evaluate(() => {
    localStorage.setItem(
      "atra:sent-history",
      JSON.stringify(["Previous prompt"])
    );
  });
  const composer = page.getByLabel("Message");
  await composer.fill("aーb");
  await composer.press("Alt+ArrowUp");
  await expect(composer).toHaveValue("Previous prompt");

  await page.getByLabel("Message").fill("Sent from browser");
  await expect.poll(() => page.evaluate(() => localStorage.getItem("atra:draft:workspace-1:1")))
    .toBe("Sent from browser");
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.getByLabel("Message")).toHaveValue("");
  await expect.poll(() => page.evaluate(() => localStorage.getItem("atra:draft:workspace-1:1")))
    .toBeNull();
  expect(sentCommand).toEqual({
    method: "thread_send",
    thread_id: 1,
    message: "Sent from browser",
    allow_questions: true
  });
  await expect(page.locator(".pinned-section .navigation-link")).toContainText("Web Thread");
  expect(pageErrors).toEqual([]);

  await page.setViewportSize({ width: 1280, height: 480 });
  await expect(page.locator("#composer")).toBeVisible();
  const desktopComposer = await page.locator("#composer").boundingBox();
  expect(desktopComposer).not.toBeNull();
  expect(desktopComposer!.y + desktopComposer!.height).toBeLessThanOrEqual(480);

  await page.getByRole("button", { name: "Toggle utility panel" }).click();
  await expect(page.locator(".app-shell")).toHaveClass(/utility-closed/);
  await page.getByRole("button", { name: "Toggle utility panel" }).click();
  await expect(page.locator(".app-shell")).not.toHaveClass(/utility-closed/);

  await page.getByRole("button", { name: "Toggle navigation" }).click();
  await expect(page.locator(".app-shell")).toHaveClass(/navigation-closed/);
  await expect(page.locator(".drawer-backdrop")).toHaveCount(0);
  await page.getByRole("button", { name: "Toggle navigation" }).click();
  await expect(page.locator(".app-shell")).not.toHaveClass(/navigation-closed/);

  await page.setViewportSize({ width: 390, height: 844 });
  const initialMobileTranscript = await page.locator("#transcript-scroll").boundingBox();
  expect(initialMobileTranscript).not.toBeNull();
  expect(initialMobileTranscript!.width).toBeGreaterThan(370);
  await expect(page.locator(".utility")).not.toHaveClass(/drawer-open/);
  await expect(page.locator(".drawer-backdrop")).toHaveCount(0);

  await page.getByRole("button", { name: "Toggle navigation" }).click();
  await expect(page.locator(".navigation")).toHaveClass(/drawer-open/);
  await expect(page.locator(".drawer-backdrop")).toBeVisible();
  await swipe(page, { x: 380, y: 420 }, { x: 240, y: 425 });
  await expect(page.locator(".drawer-backdrop")).toHaveCount(0);
  await expect(page.locator(".navigation")).not.toHaveClass(/drawer-open/);

  await swipe(page, { x: 380, y: 520 }, { x: 220, y: 525 });
  await expect(page.locator(".utility")).toHaveClass(/drawer-open/);
  await swipe(page, { x: 10, y: 420 }, { x: 150, y: 425 });
  await expect(page.locator(".utility")).not.toHaveClass(/drawer-open/);
  await expect(page.locator(".drawer-backdrop")).toHaveCount(0);

  const composerInput = page.locator("#composer textarea");
  await composerInput.scrollIntoViewIfNeeded();
  const composerInputBox = await composerInput.boundingBox();
  expect(composerInputBox).not.toBeNull();
  await swipe(
    page,
    {
      x: composerInputBox!.x + composerInputBox!.width - 20,
      y: composerInputBox!.y + composerInputBox!.height / 2
    },
    {
      x: composerInputBox!.x + composerInputBox!.width - 160,
      y: composerInputBox!.y + composerInputBox!.height / 2 + 5
    }
  );
  await expect(page.locator(".drawer-backdrop")).toHaveCount(0);

  await page.getByRole("button", { name: "Toggle utility panel" }).click();
  await expect(page.locator(".utility")).toHaveClass(/drawer-open/);
  await expect(page.locator(".drawer-backdrop")).toBeVisible();
  await page.locator(".drawer-backdrop").click({ position: { x: 10, y: 420 } });
  await expect(page.locator(".drawer-backdrop")).toHaveCount(0);
  await expect(page.locator(".utility")).not.toHaveClass(/drawer-open/);
  await expect(page.locator("#composer")).toBeVisible();
  const mobileComposer = await page.locator("#composer").boundingBox();
  expect(mobileComposer).not.toBeNull();
  expect(mobileComposer!.y + mobileComposer!.height).toBeLessThanOrEqual(844);
});

test("composer history navigation, stash, and rewind prefill", async ({ page }) => {
  const pageErrors: Error[] = [];
  page.on("pageerror", (error) => pageErrors.push(error));
  const snapshots: Record<string, unknown> = {
    "/api/workspaces/events": {
      workspaces: [{
        workspace_id: "workspace-1",
        name: "workspace",
        path: "/tmp/workspace"
      }]
    },
    "/api/workspaces/workspace-1/controller/events": {
      message: "snapshot",
      state: {
        lifecycle: "running",
        threads: [{
          id: 1,
          parent_thread_id: null,
          display_name: "Web Thread",
          provider: "fake",
          model: "test-model",
          reasoning_effort: "medium"
        }],
        thread_statuses: [{ thread_id: 1, status: "idle" }],
        providers: [{
          id: "fake",
          lifecycle: { status: "logged_in", account: null },
          models: [{
            provider: "fake",
            id: "test-model",
            display_name: "Test Model",
            description: "No-provider test model",
            default_reasoning_effort: "medium",
            supported_reasoning_efforts: ["low", "medium", "high"]
          }],
          rate_limits: null
        }],
        runners: []
      }
    },
    "/api/workspaces/workspace-1/threads/1/events": {
      message: "snapshot",
      state: {
        metadata: {
          id: 1,
          parent_thread_id: null,
          display_name: "Web Thread",
          provider: "fake",
          model: "test-model",
          reasoning_effort: "medium"
        },
        events: [
          { sequence: 1, kind: "user_message", payload: { content: "Original prompt" } },
          {
            sequence: 2,
            kind: "model_request",
            payload: { kind: "response", context_window: 128000 }
          },
          {
            sequence: 3,
            kind: "assistant_message",
            payload: { content: "The answer", phase: "final_answer" }
          }
        ],
        active_turn: null,
        last_outcome: null,
        checkpoints: [],
        processes: []
      }
    }
  };
  await page.addInitScript(() => {
    localStorage.setItem(
      "atra:sent-history",
      JSON.stringify(["First prompt", "Second prompt"])
    );
  });
  await installWebClientMocks(page, snapshots);

  let sentCommand: unknown;
  await page.route("**/api/workspaces/workspace-1/commands", async (route) => {
    sentCommand = route.request().postDataJSON();
    const result = (sentCommand as { method?: string }).method === "skill_list"
      ? { result: "skills_listed", skills: ["code-review", "setup-atra-workspace"] }
      : { result: "accepted" };
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify(result)
    });
  });

  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Choose a Workspace" })).toBeVisible();
  await page.locator(".workspace-thread-row .navigation-link").click();
  await expect(page.getByText("Original prompt")).toBeVisible();
  const composer = page.getByLabel("Message");
  await expect(composer).toBeVisible();

  await page.getByLabel("Composer actions").click();
  await page.getByRole("button", { name: "History", exact: true }).click();
  await expect(page.getByRole("button", { name: "Second prompt", exact: true })).toBeVisible();
  await page.getByRole("button", { name: "First prompt", exact: true }).click();
  await expect(composer).toHaveValue("First prompt");
  await expect(page.getByRole("button", { name: "Second prompt", exact: true })).not.toBeVisible();
  await expect.poll(() => page.evaluate(() =>
    localStorage.getItem("atra:draft:workspace-1:1")
  )).toBe("First prompt");

  await page.getByLabel("Composer actions").click();
  await page.getByRole("button", { name: "History", exact: true }).click();
  await expect(page.getByRole("button", { name: "Second prompt", exact: true })).toBeVisible();
  await page.getByText("Original prompt").click();
  await expect(page.getByRole("button", { name: "Second prompt", exact: true })).not.toBeVisible();

  await composer.press("Alt+ArrowUp");
  await expect(composer).toHaveValue("Second prompt");
  await expect.poll(() => page.evaluate(() =>
    localStorage.getItem("atra:draft:workspace-1:1")
  )).toBe("Second prompt");
  await composer.press("Alt+ArrowDown");
  await expect(composer).toHaveValue("First prompt");
  await composer.press("Alt+ArrowDown");
  await expect(composer).toHaveValue("First prompt");

  await composer.fill("A😀B");
  await composer.evaluate((element: HTMLTextAreaElement) => {
    element.setSelectionRange(3, 3);
  });
  await page.getByLabel("Composer actions").click();
  await page.getByRole("button", { name: "Skills", exact: true }).click();
  await expect(page.getByRole("button", { name: "$code-review", exact: true })).toBeVisible();
  await page.getByRole("button", { name: "$code-review", exact: true }).click();
  await expect(composer).toHaveValue("A😀$code-review B");
  await expect(composer).toBeFocused();
  await expect.poll(() => page.evaluate(() =>
    localStorage.getItem("atra:draft:workspace-1:1")
  )).toBe("A😀$code-review B");

  await composer.fill("Stashed draft");
  await composer.evaluate((element: HTMLTextAreaElement) => {
    element.setSelectionRange(element.value.length, element.value.length);
  });
  await composer.press("Control+c");
  await expect(composer).toHaveValue("");
  await expect.poll(() => page.evaluate(() =>
    localStorage.getItem("atra:sent-history")
  )).toContain("Stashed draft");

  await page.locator(".turn-actions").getByRole("button", { name: "Rewind" }).first().click();
  await expect(page.getByRole("heading", { name: "Replace Thread history?" })).toBeVisible();
  await page.getByRole("dialog").getByRole("button", { name: "Rewind" }).click();
  await expect(composer).toHaveValue("Original prompt");
  expect(sentCommand).toEqual({
    method: "thread_replace_history",
    thread_id: 1,
    target: { kind: "message", checkpoint_id: null, sequence: 1 }
  });

  await page.evaluate(() => {
    localStorage.setItem(
      "atra:sent-history",
      JSON.stringify(Array.from({ length: 20 }, (_, index) => `Prompt ${index}`))
    );
  });
  await composer.fill("x");
  await page.getByLabel("Composer actions").click();
  await page.getByRole("button", { name: "History", exact: true }).click();
  const menu = page.locator(".composer-menu-list");
  await expect(menu).toBeVisible();
  const menuSize = await menu.evaluate((element) => ({
    clientHeight: element.clientHeight,
    scrollHeight: element.scrollHeight
  }));
  expect(menuSize.scrollHeight).toBeGreaterThan(menuSize.clientHeight);

  expect(pageErrors).toEqual([]);
});
