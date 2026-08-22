import { expect, test } from "./support/test";

const workspace = { workspace_id: "workspace-1", name: "workspace", path: "/tmp/workspace" };
const thread = { id: 1, parent_thread_id: null, display_name: "Web Thread", provider: "fake", model: "test-model", reasoning_effort: "medium" };

test("utility keeps showing reasoning after the activity finalizes", async ({ page, mockEventSources }) => {
  const messages: Record<string, unknown> = {
    "/api/workspaces/events": {
      workspaces: [workspace]
    },
    "/api/workspaces/workspace-1/controller/events": {
      message: "snapshot",
      state: {
        lifecycle: "running",
        threads: [thread],
        thread_statuses: [{ thread_id: 1, status: "running" }],
        providers: [],
        runners: []
      }
    },
    "/api/workspaces/workspace-1/threads/1/events": {
      message: "snapshot",
      state: {
        metadata: thread,
        events: [{ sequence: 1, kind: "user_message", payload: { content: "Why?" } }],
        active_turn: {
          phase: "running",
          items: [
            { id: 5, data: { kind: "reasoning", content: "Because the sky is blue" } }
          ],
          pending_interaction: null,
          retry: null
        },
        last_outcome: null,
        checkpoints: [],
        processes: []
      }
    }
  };
  await mockEventSources(messages);

  await page.goto("/");
  await page.locator(".workspace-thread-row .navigation-link").click();

  const reasoningRow = page.locator(".main-thread button.activity-reasoning");
  await expect(reasoningRow).toContainText("Because the sky is blue");

  // Selecting the in-progress reasoning opens the utility with the streamed content.
  await reasoningRow.click();
  await expect(page.locator(".utility .reasoning-detail")).toContainText("Because the sky is blue");

  // The reasoning completes: the active item is finalized into a reasoning event.
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/threads/1/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "active_item_finalized",
        active_id: 5,
        event: {
          sequence: 2,
          kind: "reasoning",
          payload: { summary: "Because the sky is blue" }
        }
      }
    });
  });

  // The utility keeps showing the reasoning once it has finalized.
  await expect(page.locator(".utility .reasoning-detail")).toContainText("Because the sky is blue");
  await expect(reasoningRow).toContainText("Because the sky is blue");
});
