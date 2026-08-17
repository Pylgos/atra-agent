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
              phase: "commentary"
            }
          },
          {
            sequence: 4,
            kind: "assistant_message",
            payload: {
              content: "# Result\n<script>bad()</script>\n\n**Safe answer**"
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
  await page.getByRole("button", { name: /Web Thread/ }).click();

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
  await expect(page.getByRole("button", { name: /Load .* previous turns/ })).toHaveCount(0);
  await expect(page.locator(".activity-list")).toHaveCount(0);
  await page.locator(".activity-summary").click();
  await expect(page.locator(".activity-row")).toHaveCount(1);
  await expect(page.locator(".activity-prose")).toHaveCount(0);
  await page.getByRole("button", { name: /Commentary/ }).click();
  await expect(page.getByText("Checking the existing result.")).toBeVisible();
  await page.getByRole("button", { name: /Commentary/ }).click();
  await expect(page.locator(".activity-prose")).toHaveCount(0);
  await page.locator(".activity-summary").click();
  await expect(page.locator(".activity-row")).toHaveCount(0);
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
    source.emit({
      message: "operation",
      operation: {
        operation: "active_item_added",
        item: {
          id: 9,
          data: { kind: "assistant", content: "Streaming" }
        }
      }
    });
  });
  await expect(page.getByRole("button", { name: /Assistant response/ })).toBeVisible();
  await expect(page.getByText("Streaming", { exact: true })).toBeVisible();
  const activitySummary = page.locator(".activity-summary").last();
  await expect(activitySummary).toHaveAttribute("aria-expanded", "true");
  await activitySummary.click();
  await expect(activitySummary).toHaveAttribute("aria-expanded", "false");
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/threads/1/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "active_text_appended",
        id: 9,
        content: " update"
      }
    });
  });
  await expect(activitySummary).toHaveAttribute("aria-expanded", "false");
  await expect(page.getByText("Streaming update", { exact: true })).toHaveCount(0);
  await activitySummary.click();
  await expect(page.getByText("Streaming update", { exact: true })).toBeVisible();

  await page.locator(".turn-card").last().evaluate((element) => {
    (element as HTMLElement).style.minHeight = "1600px";
  });
  await page.locator("#transcript-scroll").evaluate((element) => {
    element.scrollTop = element.scrollHeight;
    element.dispatchEvent(new Event("scroll"));
    element.scrollTop = 0;
    element.dispatchEvent(new Event("scroll"));
  });
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/threads/1/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "active_text_appended",
        id: 9,
        content: " again"
      }
    });
  });
  await expect(page.getByRole("button", { name: "Latest" })).toBeVisible();
  await page.getByRole("button", { name: "Latest" }).click();
  await expect.poll(() => page.locator("#transcript-scroll").evaluate((element) =>
    element.scrollHeight - element.scrollTop - element.clientHeight
  )).toBeLessThanOrEqual(80);
  await expect(page.getByText("Streaming update again", { exact: true })).toBeVisible();
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

  await page.setViewportSize({ width: 1024, height: 480 });
  await expect(page.locator("#composer")).toBeVisible();
  const desktopComposer = await page.locator("#composer").boundingBox();
  expect(desktopComposer).not.toBeNull();
  expect(desktopComposer!.y + desktopComposer!.height).toBeLessThanOrEqual(480);

  await page.getByRole("button", { name: "Toggle utility panel" }).click();
  await expect(page.locator(".utility")).toHaveCount(0);
  await page.getByRole("button", { name: "Toggle utility panel" }).click();
  await expect(page.locator(".utility")).toHaveCount(1);

  await page.getByRole("button", { name: "Toggle navigation" }).click();
  await expect(page.locator(".app-shell")).toHaveClass(/navigation-closed/);
  await expect(page.locator(".drawer-backdrop")).toHaveCount(0);
  await page.getByRole("button", { name: "Toggle navigation" }).click();
  await expect(page.locator(".app-shell")).not.toHaveClass(/navigation-closed/);

  await page.setViewportSize({ width: 390, height: 844 });
  await page.getByRole("button", { name: "Toggle navigation" }).click();
  await expect(page.locator(".navigation")).toHaveClass(/drawer-open/);
  await expect(page.locator(".drawer-backdrop")).toBeVisible();
  await page.locator(".drawer-backdrop").click({ position: { x: 380, y: 420 } });
  await expect(page.locator(".drawer-backdrop")).toHaveCount(0);
  await expect(page.locator(".navigation")).not.toHaveClass(/drawer-open/);

  await page.getByRole("button", { name: "Toggle utility panel" }).click();
  await expect(page.locator(".utility")).toHaveClass(/drawer-open/);
  await expect(page.locator(".drawer-backdrop")).toBeVisible();
  await page.locator(".drawer-backdrop").click({ position: { x: 10, y: 420 } });
  await expect(page.locator(".drawer-backdrop")).toHaveCount(0);
  await expect(page.locator(".utility")).toHaveCount(0);
  await expect(page.locator("#composer")).toBeVisible();
  const mobileComposer = await page.locator("#composer").boundingBox();
  expect(mobileComposer).not.toBeNull();
  expect(mobileComposer!.y + mobileComposer!.height).toBeLessThanOrEqual(844);
});
