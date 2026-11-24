import { fireEvent, screen, waitFor } from "@testing-library/react";
import { expect, it } from "vitest";
import { installBridge, renderApp } from "../test-utils";
import type { ItemsQueryArgs } from "../lib/ipc";

it("可翻到第 101 条历史建议，再从首页搜索全部记录", async () => {
  const invoke = installBridge((command, payload) => {
    if (command === "db_info") return { dbPath: "/tmp/app.db", schemaVersion: 1 };
    if (command === "sync_status") return [];
    if (command !== "items_query") return null;
    const args = payload?.args as ItemsQueryArgs;
    const historical = args.offset === 100 || Boolean(args.query);
    return {
      items: [{
        id: historical ? "flow-101" : "flow-1", title: historical ? "首版工艺图" : "交付新版",
        lifecycle: "candidate", attention: "unclear", historical, project: "图纸交付",
        confirmation: historical ? "suggested" : "confirmed", lastEvidenceAt: null,
        updatedAt: null, nextAction: null, sourceLabel: null, awaitingConfirmation: false,
      }],
      total: args.query ? 1 : 101,
      freshness: { state: "unknown", lastSuccessAt: null },
    };
  });
  renderApp("/items");
  fireEvent.click(await screen.findByRole("button", { name: "下一页" }));
  expect((await screen.findByRole("link", { name: "首版工艺图" })).getAttribute("href")).toBe("/items/flow-101");
  expect(screen.getByText("历史事项")).toBeTruthy();
  expect(screen.getByText("系统建议")).toBeTruthy();
  expect(screen.getByText("图纸交付")).toBeTruthy();
  fireEvent.change(screen.getByLabelText("事项文字查询"), { target: { value: "首版" } });
  await waitFor(() => expect(invoke).toHaveBeenCalledWith("items_query", {
    args: expect.objectContaining({ query: "首版", offset: 0 }),
  }));
});
