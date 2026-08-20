import { expect, test as base, type Page } from "@playwright/test";
import { installEventSourceSnapshots } from "./event-source";

export type EventSourceSnapshots = Record<string, unknown>;
export type MockEventSources = (snapshots: EventSourceSnapshots) => Promise<void>;

export const test = base.extend<{ mockEventSources: MockEventSources }>({
  mockEventSources: async ({ page }, use) => {
    await use((snapshots) => installEventSourceSnapshots(page, snapshots));
  }
});

export { expect };
export type { Page };
