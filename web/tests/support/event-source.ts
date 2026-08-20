import type { Page } from "@playwright/test";

export async function installEventSourceSnapshots(
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
            this.emit(snapshot);
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
  }, snapshots);
}
