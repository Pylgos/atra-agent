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

function changedFile(path = "src/lib.rs", lineCount = 1) {
  return {
    change: {
      status: "modified",
      path: { encoding: "utf8", value: path }
    },
    additions: lineCount,
    deletions: 0,
    mode_change: path === "src/lib.rs" ? { old: "100644", new: "100755" } : null,
    kind: { kind: "text" },
    truncated: false,
    hunks: [{
      header: `@@ -1 +1,${lineCount} @@`,
      old_start: 1,
      old_lines: 1,
      new_start: 1,
      new_lines: lineCount,
      truncated: false,
      lines: Array.from({ length: lineCount }, (_, index) => ({
        kind: "addition",
        content: index === 0 ? "let value = 1;" : `let value_${index} = ${index};`,
        old_line: null,
        new_line: index + 1,
        no_newline_at_eof: false
      }))
    }]
  };
}

async function openChanges(
  page: Page,
  mockEventSources: MockEventSources,
  files = [changedFile()]
) {
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
            files
          }
        };
    await route.fulfill({ contentType: "application/json", body: JSON.stringify(response) });
  });

  await page.goto("/");
  await page.locator(".workspace-thread-row .navigation-link").click();
  await page.getByRole("tab", { name: "Changes" }).click();
  await expect(page.locator(".diff-file", {
    hasText: files[0].change.path.value
  })).toBeVisible();
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

  await expect(page.locator(".diff-file")).toHaveClass(/line-wrap/);
  await expect.poll(() => page.evaluate(() =>
    localStorage.getItem("atra:diff:line-wrap")
  )).toBe("wrap");
});

test("Changes extends line backgrounds across the scrollable diff width", async ({
  page,
  mockEventSources
}) => {
  const file = changedFile("src/wide.rs", 2);
  file.hunks[0].lines[0].content = "short";
  file.hunks[0].lines[1].content = "x".repeat(300);
  await openChanges(page, mockEventSources, [file]);

  const diffLines = page.locator(".diff-lines");
  const shortLine = page.locator(".diff-code").first();
  await expect.poll(() => diffLines.evaluate((element) =>
    element.scrollWidth > element.clientWidth
  )).toBe(true);
  await expect.poll(async () => {
    const scrollWidth = await diffLines.evaluate((element) => element.scrollWidth);
    const lineWidth = await shortLine.evaluate(
      (element) => element.getBoundingClientRect().width
    );
    return Math.abs(scrollWidth - lineWidth);
  }).toBeLessThan(1);
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

test("Changes highlights source and unmounts diff bodies outside the viewport", async ({
  page,
  mockEventSources
}) => {
  const files = Array.from(
    { length: 4 },
    (_, index) => changedFile(`src/file_${index}.rs`, 120)
  );
  files[0].mode_change = { old: "100644", new: "100755" };
  files[0].hunks[0].lines[0].no_newline_at_eof = true;
  files[1].hunks[0].truncated = true;
  files[2].truncated = true;
  await openChanges(page, mockEventSources, files);

  const first = page.locator(".diff-file").first();
  const last = page.locator(".diff-file").last();
  await expect(first.locator(".diff-body")).toHaveCount(1);
  await expect(first.locator(".shiki-token", { hasText: "let" }).first()).toContainText("let");
  await expect.poll(() => page.locator(".diff-body").count()).toBeLessThan(files.length);
  const utility = page.locator(".utility-content");
  const initialHeight = await utility.evaluate((element) => element.scrollHeight);

  await last.scrollIntoViewIfNeeded();
  await expect(last.locator(".diff-body")).toHaveCount(1);
  await expect(first.locator(".diff-body")).toHaveCount(0);
  const finalHeight = await utility.evaluate((element) => element.scrollHeight);

  expect(Math.abs(finalHeight - initialHeight)).toBeLessThanOrEqual(1);
});

test("Changes reuses measured spacer heights while wrapping lines", async ({
  page,
  mockEventSources
}) => {
  const files = [
    changedFile("src/first.rs", 80),
    changedFile("src/last.rs", 80)
  ];
  await openChanges(page, mockEventSources, files);
  await page.getByLabel("Wrap lines").check();

  const first = page.locator(".diff-file").first();
  const last = page.locator(".diff-file").last();
  await expect(first.locator(".diff-body")).toHaveCount(1);
  const measuredHeight = await first.locator(".diff-body").evaluate(
    (element) => element.getBoundingClientRect().height
  );

  await last.scrollIntoViewIfNeeded();
  await expect(last.locator(".diff-body")).toHaveCount(1);
  await page.locator(".utility-content").evaluate((element) => {
    element.scrollTop = element.scrollHeight;
  });
  await expect(first.locator(".diff-body")).toHaveCount(0);
  const spacerHeight = await first.locator(".diff-virtual-spacer").evaluate(
    (element) => element.getBoundingClientRect().height
  );

  expect(Math.abs(spacerHeight - measuredHeight)).toBeLessThanOrEqual(1);
});

test("Changes refreshes when the window regains focus", async ({
  page,
  mockEventSources
}) => {
  const requests = await openChanges(page, mockEventSources);
  const pageErrors: Error[] = [];
  page.on("pageerror", (error) => pageErrors.push(error));
  const beforeFocus = requests.length;

  await page.evaluate(() => window.dispatchEvent(new Event("focus")));

  await expect.poll(() => requests.length).toBeGreaterThan(beforeFocus);
  expect(pageErrors).toEqual([]);
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
