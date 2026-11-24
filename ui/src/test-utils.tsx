import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { render, type RenderResult } from "@testing-library/react";
import { MemoryRouter } from "react-router-dom";
import { vi } from "vitest";
import App from "./App";

export function makeQueryClient() {
  return new QueryClient({
    defaultOptions: {
      queries: { retry: false, refetchOnWindowFocus: false },
      mutations: { retry: false },
    },
  });
}

export function renderApp(
  path = "/today",
): RenderResult & { client: QueryClient } {
  const client = makeQueryClient();
  const view = render(
    <QueryClientProvider client={client}>
      <MemoryRouter initialEntries={[path]}>
        <App />
      </MemoryRouter>
    </QueryClientProvider>,
  );
  return { ...view, client };
}

type InvokeHandler = (command: string, args?: Record<string, unknown>) => unknown;

/** 模拟 Tauri 事件通道：`transformCallback` + `plugin:event|listen/unlisten`。 */
const eventCallbacks = new Map<number, (message: unknown) => void>();
const eventSubscriptions = new Map<string, number>();
let nextCallbackId = 1;

function resetEvents() {
  eventCallbacks.clear();
  eventSubscriptions.clear();
  nextCallbackId = 1;
}

export function installBridge(handler: InvokeHandler) {
  resetEvents();
  const invoke = vi.fn(async (command: string, args?: Record<string, unknown>) => {
    if (command === "plugin:event|listen") {
      const event = args?.event;
      const callbackId = args?.handler;
      if (typeof event === "string" && typeof callbackId === "number") {
        eventSubscriptions.set(event, callbackId);
        return `event-${event}`;
      }
      throw new Error("bad event listen args");
    }
    if (command === "plugin:event|unlisten") {
      const event = args?.event;
      if (typeof event === "string") eventSubscriptions.delete(event);
      return null;
    }
    return handler(command, args);
  });
  window.__TAURI_INTERNALS__ = {
    invoke,
    transformCallback: (callback: (message: unknown) => void) => {
      const id = nextCallbackId++;
      eventCallbacks.set(id, callback);
      return id;
    },
  };
  return invoke;
}

export function removeBridge() {
  delete window.__TAURI_INTERNALS__;
  resetEvents();
}

export function hasTauriListener(event: string): boolean {
  return eventSubscriptions.has(event);
}

/** 向已注册的 Tauri 事件回调投递载荷（模拟 `app.emit`）。 */
export function emitTauriEvent(event: string, payload: unknown): boolean {
  const callbackId = eventSubscriptions.get(event);
  if (callbackId === undefined) return false;
  const callback = eventCallbacks.get(callbackId);
  if (!callback) return false;
  callback({ event, id: callbackId, payload });
  return true;
}
