import { test, expect } from "@playwright/test";

test("embedded Web Client is responsive and reports its connection", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Atra" })).toBeVisible();
  await expect(page.getByRole("navigation", { name: "Workspaces" })).toBeVisible();
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(page.getByRole("heading", { name: "Choose a Workspace" })).toBeVisible();
  await expect(page.locator(".app-shell")).toHaveCSS("display", "block");
});

test("dark theme keeps foreground text legible", async ({ page }) => {
  await page.addInitScript(() => {
    localStorage.setItem("atra:theme", "dark");
    class FakeEventSource {
      static readonly OPEN = 1;
      readonly readyState = FakeEventSource.OPEN;
      onmessage: ((event: MessageEvent) => void) | null = null;
      onopen: ((event: Event) => void) | null = null;
      onerror: ((event: Event) => void) | null = null;

      constructor(url: string) {
        if (url.endsWith("/api/workspaces/events")) {
          setTimeout(() => {
            this.onmessage?.({
              data: JSON.stringify({ workspaces: [] })
            } as MessageEvent);
            this.onopen?.(new Event("open"));
          }, 0);
        }
      }

      close() {}
    }
    Object.defineProperty(window, "EventSource", {
      configurable: true,
      value: FakeEventSource
    });
  });

  await page.goto("/");
  await expect(page.locator(".app-shell")).toHaveCSS("background-color", "rgb(15, 20, 26)");
  await expect(page.locator(".app-shell")).toHaveCSS("color", "rgb(243, 246, 250)");
  await expect(page.locator(".navigation")).toHaveCSS("background-color", "rgb(21, 27, 35)");
});
