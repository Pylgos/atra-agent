import { test, expect } from "@playwright/test";

test("embedded Web Client is responsive and reports its connection", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("heading", { name: "Atra" })).toBeVisible();
  await expect(page.getByRole("navigation", { name: "Workspaces" })).toBeVisible();
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(page.getByRole("heading", { name: "Choose a Workspace" })).toBeVisible();
});
