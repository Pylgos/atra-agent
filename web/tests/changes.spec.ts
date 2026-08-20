import { expect, test, type MockEventSources, type Page } from "./support/test";

function snapshots() {
  const thread = {
    id: 1,
    parent_thread_id: null,
    display_name: "Web Thread",
    provider: "fake",
    model: "test-model",
    reasoning_effort: "medium"
  };
  return {
    "/api/workspaces/events": {
      workspaces: [{ workspace_id: "workspace-1", name: "workspace", path: "/repo" }]
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
            description: null,
            default_reasoning_effort: "medium",
            supported_reasoning_efforts: ["medium"]
          }],
          rate_limits: null
        }],
        runners: [{
          runner: { name: "sandbox", description: "test", approval: "allow" },
          lifecycle: { status: "running" }
        }]
      }
    },
    "/api/workspaces/workspace-1/threads/1/events": {
      message: "snapshot",
      state: {
        metadata: thread,
        events: [],
        active_turn: null,
        last_outcome: null,
        checkpoints: [],
        processes: []
      }
    }
  };
}

async function openChanges(page: Page, mockEventSources: MockEventSources) {
  await mockEventSources(snapshots());
  const requests: any[] = [];
  await page.route("**/api/workspaces/workspace-1/queries", async (route) => {
    const envelope = route.request().postDataJSON();
    const request = { runner: envelope.runner, ...envelope.request };
    requests.push(request);
    const response = request.query === "repository_info"
      ? {
          status: "success",
          result: {
            result: "repository_info",
            root: "/repo",
            head: { state: "branch", name: "main", commit: "abc" },
            inferred_base: "main",
            base_candidates: ["main"]
          }
        }
      : {
          status: "success",
          result: {
            result: "git_diff",
            scope: request.scope,
            additions: 1,
            deletions: 0,
            truncated: false,
            files: [{
              change: {
                status: "modified",
                path: { encoding: "utf8", value: "src/lib.rs" }
              },
              additions: 1,
              deletions: 0,
              mode_change: { old: "100644", new: "100755" },
              kind: { kind: "text" },
              truncated: false,
              hunks: [{
                header: "@@ -1 +1 @@",
                old_start: 1,
                old_lines: 1,
                new_start: 1,
                new_lines: 1,
                truncated: false,
                lines: [{
                  kind: "addition",
                  content: "let value = 1;",
                  old_line: null,
                  new_line: 1,
                  no_newline_at_eof: false
                }]
              }]
            }]
          }
        };
    await route.fulfill({ contentType: "application/json", body: JSON.stringify(response) });
  });

  await page.goto("/");
  await page.locator(".workspace-thread-row .navigation-link").click();
  await page.getByRole("tab", { name: "Changes" }).click();
  await expect(page.locator(".github-diff-file", { hasText: "src/lib.rs" })).toBeVisible();
  return requests;
}

test("Changes caches loaded scopes", async ({ page, mockEventSources }) => {
  const requests = await openChanges(page, mockEventSources);
  expect(requests.filter((request) => request.query === "git_diff")).toHaveLength(1);

  await page.getByRole("tab", { name: /Staged/ }).click();
  await page.getByRole("tab", { name: /Unstaged/ }).click();

  expect(requests.filter((request) => request.query === "git_diff")).toHaveLength(2);
});

test("Changes persists base and whitespace query preferences", async ({ page, mockEventSources }) => {
  const requests = await openChanges(page, mockEventSources);

  await expect(page.getByLabel("Base")).toHaveCount(0);
  await page.getByRole("tab", { name: /Base/ }).click();
  await expect(page.getByLabel("Base")).toHaveValue("main");
  await expect.poll(() => page.evaluate(() => {
    const saved = localStorage.getItem("atra:changes:workspace-1:1");
    return saved === null ? null : JSON.parse(saved).base;
  })).toBe("main");

  await page.getByRole("tab", { name: /Unstaged/ }).click();
  await page.getByLabel("Hide whitespace").check();
  await expect.poll(() => requests.some((request) =>
    request.query === "git_diff"
      && request.scope === "unstaged"
      && request.ignore_whitespace === true
  )).toBe(true);

  const beforeStaleScope = requests.length;
  await page.getByRole("tab", { name: /Staged/ }).click();
  await expect.poll(() => requests.length).toBeGreaterThan(beforeStaleScope);
  await expect.poll(() => requests.some((request) =>
    request.query === "git_diff"
      && request.scope === "staged"
      && request.ignore_whitespace === true
  )).toBe(true);
});

test("Changes persists line wrapping independently of query state", async ({ page, mockEventSources }) => {
  await openChanges(page, mockEventSources);

  await page.getByLabel("Wrap lines").check();

  await expect(page.locator(".github-diff-file")).toHaveClass(/line-wrap/);
  await expect.poll(() => page.evaluate(() =>
    localStorage.getItem("atra:diff:line-wrap")
  )).toBe("wrap");
});

test("Changes keeps file navigation stable while expanding context", async ({ page, mockEventSources }) => {
  const requests = await openChanges(page, mockEventSources);
  await expect(page.locator(".diff-line-content")).toContainText("let value = 1;");
  await expect(page.locator(".diff-message")).toContainText(
    "File mode changed: 100644 → 100755"
  );

  await page.getByRole("button", { name: "Expand utility panel" }).click();
  await page.locator(".file-index summary").click();
  await page.locator(".file-index a").click();
  await expect.poll(() => page.evaluate(() => location.hash))
    .toBe("#change-file-6368616e676573-7372632f6c69622e7273");

  await page.getByRole("button", { name: "Refresh" }).click();
  await expect.poll(() => page.evaluate(() => location.hash))
    .toBe("#change-file-6368616e676573-7372632f6c69622e7273");

  await page.getByRole("button", { name: "20 more lines" }).click();
  await expect.poll(() => requests.some((request) =>
    request.query === "git_diff"
      && request.path === "src/lib.rs"
      && request.context_lines === 23
  )).toBe(true);

  await page.getByRole("button", { name: "Expand all" }).click();
  await expect.poll(() => requests.some((request) =>
    request.query === "git_diff"
      && request.path === "src/lib.rs"
      && request.context_lines === 4294967295
  )).toBe(true);
});

test("Changes refreshes after a Runner tool completes", async ({ page, mockEventSources }) => {
  const requests = await openChanges(page, mockEventSources);
  const beforeCompletion = requests.length;

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
          id: 1,
          data: {
            kind: "runner_tool",
            call_id: "call-1",
            operation_index: 1,
            runner: "sandbox",
            update: {
              kind: "command_output",
              content: "",
              omitted_bytes: 0,
              timer: { elapsed_ms: 1, remaining_ms: 0, paused: false }
            }
          }
        }
      }
    });
  });
  await page.waitForTimeout(50);
  await page.evaluate(() => {
    const source = (window as any).__atraEventSources.get(
      "/api/workspaces/workspace-1/threads/1/events"
    );
    source.emit({
      message: "operation",
      operation: {
        operation: "active_runner_updated",
        id: 1,
        update: {
          kind: "completed",
          artifact: {
            kind: "runner_operation",
            data: {
              operation: 1,
              runner: "sandbox",
              label: "Command",
              result: "",
              artifacts: []
            }
          }
        }
      }
    });
  });

  await expect.poll(() => requests.length).toBeGreaterThan(beforeCompletion);
});
