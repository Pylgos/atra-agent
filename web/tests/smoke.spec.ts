import { expect, test } from "./support/test";

test("embedded Web Client uses the real event stream and stays responsive", async ({ page }) => {
  await page.goto("/");
  await expect.poll(() => page.evaluate(() => !("__atraEventSources" in window))).toBe(true);
  await expect(page.getByRole("heading", { name: "Atra" })).toBeVisible();
  await expect(page.getByRole("navigation", { name: "Workspaces" })).toBeVisible();
  await page.setViewportSize({ width: 1024, height: 768 });
  await expect(page.locator(".app-shell")).toHaveCSS("display", "block");
  await expect(page.locator(".navigation")).toHaveCSS("position", "fixed");
  await page.setViewportSize({ width: 390, height: 844 });
  await expect(page.getByRole("heading", { name: "Choose a Workspace" })).toBeVisible();
  await expect(page.locator(".app-shell")).toHaveCSS("display", "block");
});

test("dark theme keeps foreground text legible", async ({ page, mockEventSources }) => {
  await page.addInitScript(() => {
    localStorage.setItem("atra:theme", "dark");
  });
  await mockEventSources({
    "/api/workspaces/events": { workspaces: [] }
  });

  await page.goto("/");
  await expect(page.locator(".app-shell")).toHaveCSS("background-color", "rgb(15, 20, 26)");
  await expect(page.locator(".app-shell")).toHaveCSS("color", "rgb(243, 246, 250)");
  await expect(page.locator(".navigation")).toHaveCSS("background-color", "rgb(21, 27, 35)");
});

test("Web Push can be enabled and tested from application settings", async ({ page }) => {
  await page.addInitScript(() => {
    let registered = false;
    let subscribed = false;
    const subscription = {
      endpoint: "https://push.example.test/subscription",
      toJSON: () => ({ keys: { auth: "auth", p256dh: "p256dh" } }),
      unsubscribe: async () => {
        subscribed = false;
        return true;
      }
    };
    const registration = {
      pushManager: {
        getSubscription: async () => subscribed ? subscription : null,
        subscribe: async () => {
          subscribed = true;
          return subscription;
        }
      }
    };
    Object.defineProperty(window, "Notification", {
      configurable: true,
      value: class {
        static permission = "default";
        static requestPermission() { return Promise.resolve("granted"); }
      }
    });
    Object.defineProperty(window, "PushManager", {
      configurable: true,
      value: class {}
    });
    Object.defineProperty(navigator, "serviceWorker", {
      configurable: true,
      value: {
        getRegistration: async () => registered ? registration : undefined,
        register: async () => {
          registered = true;
          return registration;
        },
        ready: Promise.resolve(registration)
      }
    });
  });
  const requests: string[] = [];
  await page.route("**/api/push/**", async (route) => {
    const request = route.request();
    requests.push(`${request.method()} ${new URL(request.url()).pathname}`);
    if (request.method() === "GET") {
      await route.fulfill({
        contentType: "application/json",
        body: JSON.stringify({ public_key: "test-public-key" })
      });
    } else {
      await route.fulfill({ status: 204 });
    }
  });

  await page.goto("/");
  await page.getByText("Application settings", { exact: true }).click();
  const toggle = page.getByRole("checkbox", { name: "Web Push notifications" });
  await expect(toggle).toBeEnabled();
  await toggle.check();
  await expect(page.getByText("This browser is subscribed.")).toBeVisible();
  expect(requests).toContain("PUT /api/push/subscription");

  await page.getByRole("button", { name: "Send test notification" }).click();
  await expect(page.getByText("Test notification sent.")).toBeVisible();
  expect(requests).toContain("POST /api/push/test");
});

test("blocked Web Push permission is explained without requesting again", async ({ page }) => {
  await page.addInitScript(() => {
    Object.defineProperty(window, "Notification", {
      configurable: true,
      value: class {
        static permission = "denied";
        static requestPermission() {
          throw new Error("permission must not be requested after denial");
        }
      }
    });
    Object.defineProperty(window, "PushManager", {
      configurable: true,
      value: class {}
    });
    Object.defineProperty(navigator, "serviceWorker", {
      configurable: true,
      value: {
        getRegistration: async () => undefined
      }
    });
  });

  await page.goto("/");
  await page.getByText("Application settings", { exact: true }).click();
  await expect(
    page.getByText("Notifications are blocked in browser settings.")
  ).toBeVisible();
  await expect(
    page.getByRole("checkbox", { name: "Web Push notifications" })
  ).toBeDisabled();
});
