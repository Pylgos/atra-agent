import { expect, test } from "@playwright/test";

test("critical Thread workflow uses streamed snapshots and forwards commands", async ({ page }) => {
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
            kind: "assistant_message",
            payload: {
              content: "# Result\n<script>bad()</script>\n\n**Safe answer**"
            }
          }
        ],
        active_turn: null,
        last_outcome: null,
        checkpoints: [],
        processes: []
      }
    }
  };

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
    }

    Object.defineProperty(window, "EventSource", {
      configurable: true,
      value: FakeEventSource
    });
  }, messages);

  let sentCommand: unknown;
  await page.route("**/api/workspaces/workspace-1/commands", async (route) => {
    sentCommand = route.request().postDataJSON();
    await route.fulfill({
      contentType: "application/json",
      body: JSON.stringify({ result: "accepted" })
    });
  });

  await page.goto("/");
  await page.getByRole("button", { name: /workspace/ }).click();
  await page.getByRole("button", { name: /Web Thread/ }).click();

  await expect(page.getByText("Existing prompt")).toBeVisible();
  await expect(page.getByRole("heading", { name: "Result" })).toBeVisible();
  await expect(page.locator("#transcript script")).toHaveCount(0);
  await expect(page.getByText("<script>bad()</script>")).toBeVisible();

  await page.getByLabel("Message").fill("Sent from browser");
  await page.getByRole("button", { name: "Send" }).click();
  await expect(page.getByLabel("Message")).toHaveValue("");
  expect(sentCommand).toEqual({
    method: "thread_send",
    thread_id: 1,
    message: "Sent from browser",
    allow_questions: true
  });
});
