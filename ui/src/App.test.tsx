import { fireEvent, screen, waitFor, within } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { ItemDetail, TodayView } from "./lib/ipc";
import { installBridge, renderApp } from "./test-utils";

const UNKNOWN_TODAY: TodayView = {
  groups: { mine: [], waiting: [], confirm: [] },
  freshness: { state: "unknown", lastSuccessAt: null, windowStart: null, windowEnd: null },
};

const FRESH_TODAY: TodayView = {
  groups: { mine: [], waiting: [], confirm: [] },
  freshness: {
    state: "fresh",
    lastSuccessAt: 1763700000000,
    windowStart: null,
    windowEnd: null,
  },
};

function bridgeWith(overrides: Record<string, (args?: Record<string, unknown>) => unknown>) {
  return installBridge((command, args) => {
    if (command === "db_info") {
      return { dbPath: "/tmp/workcontext.sqlite3", schemaVersion: 1 };
    }
    if (command === "sync_status") return [];
    const handler = overrides[command];
    if (handler) return handler(args);
    throw {
      code: "IPC_ERROR",
      message: `command ${command} not found`,
      retryable: false,
    };
  });
}

const DETAIL: ItemDetail = {
  item: {
    id: "wi_01",
    title: "确认 OA 业务复测结果",
    lifecycle: "active",
    attention: "report_back",
    nextAction: "向交办人回报复测结果",
    followupType: "none",
    followupAt: null,
    followupNote: null,
    threadId: null,
    confidence: 78,
    awaitingConfirmation: false,
    closedAt: null,
    closeReason: null,
    resolutionEvidence: null,
    lastEvidenceAt: 1763700000000,
    createdAt: 1763600000000,
    updatedAt: 1763700005000,
    lockedFields: [],
  },
  timeline: [
    {
      id: "ev_01",
      eventType: "created",
      actor: "agent",
      at: 1763600000000,
      note: null,
    },
  ],
  evidence: [
    {
      id: "sr_01",
      kind: "im_message",
      label: "OA 项目群 · 张伟",
      occurredAt: 1763700000000,
      senderName: "张伟",
      summary: "业务方已复测，问题不再出现",
      rawState: "available",
      linkType: "origin",
      state: "confirmed",
    },
  ],
  corrections: [],
  sourceFreshness: [{ sourceId: "src_01", lastSuccessAt: 1763700000000 }],
};

describe("三页导航与今日", () => {
  it("浏览器预览不假装有 Tauri，也不展示假空态", () => {
    renderApp("/today");
    expect(screen.getByRole("heading", { level: 1, name: "今日" })).toBeTruthy();
    expect(screen.getByText("未连接 Tauri 桌面端")).toBeTruthy();
    expect(screen.queryByText("目前无需要主动处理的事项")).toBeNull();
    expect(screen.getByRole("status").textContent).toContain("未连接桌面端");
  });

  it("已连接但尚未同步时，区分「没有记录」与「没有事情」", async () => {
    bridgeWith({ today_list: () => UNKNOWN_TODAY });
    renderApp("/today");

    expect(await screen.findByText("本地事项库尚未同步。")).toBeTruthy();
    expect(screen.queryByText(/已同步/)).toBeNull();
  });

  it("渲染真实今日卡片，并进入事项详情", async () => {
    const invoke = bridgeWith({
      today_list: () => ({
        groups: {
          mine: [
            {
              id: "wi_01",
              title: "确认 OA 业务复测结果",
              attention: "report_back",
              nextAction: "向交办人回报复测结果",
              whyHere: "你承诺过今天回报",
              lastEvidenceAt: 1763700000000,
              sourceRef: { id: "sr_01", kind: "im_message", label: "OA 项目群 · 张伟" },
            },
          ],
          waiting: [],
          confirm: [],
        },
        freshness: FRESH_TODAY.freshness,
      }),
      item_get: () => DETAIL,
    });

    renderApp("/today");
    const link = await screen.findByRole("link", { name: "确认 OA 业务复测结果" });
    expect(screen.getByText("向交办人回报复测结果")).toBeTruthy();
    expect(screen.queryByText("你承诺过今天回报")).toBeNull();
    expect(screen.queryByText("OA 项目群 · 张伟")).toBeNull();

    fireEvent.click(link);
    expect(
      await screen.findByRole("heading", { level: 1, name: "确认 OA 业务复测结果" }),
    ).toBeTruthy();
    expect(screen.getByText("立即纠错")).toBeTruthy();
    expect(invoke).toHaveBeenCalledWith("item_get", { id: "wi_01" });
  });

  it("等待项缺少成功同步时间时不渲染卡片", async () => {
    bridgeWith({
      today_list: () => ({
        groups: {
          mine: [],
          waiting: [
            {
              id: "wi_wait",
              title: "不该显示的等待项",
              attention: "waiting_other",
              nextAction: "等对方回复",
              whyHere: "你发出过请求",
              lastEvidenceAt: 1763700000000,
              sourceRef: { id: "sr_01", kind: "im_message", label: "私聊 · 李四" },
            },
          ],
          confirm: [],
        },
        freshness: UNKNOWN_TODAY.freshness,
      }),
    });

    renderApp("/today");
    expect(await screen.findByText("无法显示等待项")).toBeTruthy();
    expect(screen.queryByText("不该显示的等待项")).toBeNull();
  });

  it("IPC 失败明确报错并允许重试，不落成空列表", async () => {
    let calls = 0;
    const invoke = bridgeWith({
      today_list: () => {
        calls += 1;
        throw {
          code: "IPC_ERROR",
          message: "command today_list not found",
          retryable: false,
        };
      },
    });

    renderApp("/today");
    expect(await screen.findByText(/今日读取失败（IPC_ERROR）/)).toBeTruthy();
    expect(screen.getByText("command today_list not found")).toBeTruthy();
    expect(screen.queryByText("本地事项库尚未同步。")).toBeNull();

    fireEvent.click(screen.getByRole("button", { name: "重试" }));
    await waitFor(() => expect(calls).toBeGreaterThanOrEqual(2));
    expect(invoke).toHaveBeenCalledWith("today_list", {});
  });

  it("默认打开概览，离线时保留补充入口并禁用写入", async () => {
    renderApp("/");
    expect(await screen.findByRole("heading", {name: "工作概览"})).toBeTruthy();
    expect(screen.getByRole("button", {name: /补充一条工作信息/})).toHaveProperty("disabled",true);
  });

  it("工作空间与系统分组保留跟进事项的旧页面状态", async () => {
    bridgeWith({ today_list: () => UNKNOWN_TODAY });
    renderApp("/today");
    const nav = screen.getByRole("navigation", { name: "主导航" });
    const links = within(nav).getAllByRole("link");
    expect(links.map((link) => link.getAttribute("href"))).toEqual([
      "/overview", "/projects", "/records", "/follow-up", "/search",
      "/skills", "/sources", "/pool", "/settings",
    ]);
    const active = within(nav).getByRole("link", { name: /跟进事项/ });
    expect(active.getAttribute("aria-current")).toBe("page");
  });

  it("桌面快捷键直接切页，输入框保留自己的按键", async () => {
    bridgeWith({ today_list: () => UNKNOWN_TODAY });
    renderApp("/today");
    const modifier = navigator.platform.startsWith("Mac") ? { metaKey: true } : { ctrlKey: true };
    fireEvent.keyDown(window, { key: "2", ...modifier });
    expect(await screen.findByRole("heading", { level: 1, name: "全部事项" })).toBeTruthy();
    fireEvent.keyDown(screen.getByRole("searchbox"), { key: "1", ...modifier });
    expect(screen.getByRole("heading", { level: 1, name: "全部事项" })).toBeTruthy();
    fireEvent.keyDown(window, { key: "1", ...modifier });
    expect(await screen.findByRole("heading", { level: 1, name: "今日" })).toBeTruthy();
  });
});

describe("全部事项", () => {
  it("未同步与已同步的空结果用不同文案", async () => {
    bridgeWith({
      items_query: () => ({
        items: [],
        total: 0,
        freshness: UNKNOWN_TODAY.freshness,
      }),
    });
    renderApp("/items");
    expect(
      await screen.findByText("尚未同步：本地没有任何事项记录"),
    ).toBeTruthy();
    expect(screen.queryByText(/条已同步/)).toBeNull();
  });

  it("筛选与文字查询传给 items_query，匹配项进入详情", async () => {
    const invoke = bridgeWith({
      items_query: (payload) => {
        const query = (payload?.args as Record<string, unknown> | undefined)?.query;
        return {
          items:
            query === "OA"
              ? [
                  {
                    id: "wi_01",
                    title: "确认 OA 业务复测结果",
                    lifecycle: "active",
                    attention: "me",
                    nextAction: "回报结果",
                    sourceLabel: "OA 项目群 · 张伟",
                    lastEvidenceAt: 1763700000000,
                    updatedAt: 1763700005000,
                    awaitingConfirmation: false,
                  },
                ]
              : [],
          total: query === "OA" ? 1 : 0,
          freshness: FRESH_TODAY.freshness,
        };
      },
      item_get: () => DETAIL,
    });

    renderApp("/items");
    const group = screen.getByRole("group", { name: "按状态筛选" });
    fireEvent.click(within(group).getByRole("button", { name: "轮到我" }));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("items_query", {
        args: expect.objectContaining({ filter: "me" }),
      }),
    );

    const input = screen.getByLabelText("事项文字查询") as HTMLInputElement;
    fireEvent.change(input, { target: { value: "OA" } });
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("items_query", {
        args: expect.objectContaining({ filter: "me", query: "OA" }),
      }),
    );

    fireEvent.click(
      await screen.findByRole("link", { name: "确认 OA 业务复测结果" }),
    );
    expect(
      await screen.findByRole("heading", { level: 1, name: "确认 OA 业务复测结果" }),
    ).toBeTruthy();
  });
});
