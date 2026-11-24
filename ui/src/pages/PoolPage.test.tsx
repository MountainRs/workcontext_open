import { fireEvent, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { Snapshot } from "../lib/ipc";
import { installBridge, renderApp } from "../test-utils";

const SNAPSHOTS: Snapshot[] = [
  {
    id: "snap_01",
    route: "/items/wi_01",
    targetType: "work_item",
    targetId: "wi_01",
    title: "误把同事任务算到我头上",
    description: "群里点名的是同事，却生成了我的事项",
    expected: "不建事项，或落到待确认",
    kind: "wrong_owner",
    severity: "high",
    redaction: "masked",
    appVersion: "0.1.3",
    schemaVersion: 1,
    status: "open",
    fixedInRelease: null,
    createdAt: 1763700000000,
  },
  {
    id: "snap_02",
    route: "/pool",
    targetType: null,
    targetId: null,
    title: "导出按钮状态",
    description: "未选中时按钮仍可点击",
    expected: "禁用并说明原因",
    kind: "ui",
    severity: "low",
    redaction: "masked",
    appVersion: "0.1.3",
    schemaVersion: 1,
    status: "resolved",
    fixedInRelease: "0.1.4",
    createdAt: 1763600000000,
  },
];

function bridgeWith(
  overrides: Record<string, (args?: Record<string, unknown>) => unknown> = {},
) {
  return installBridge((command, args) => {
    if (command === "db_info") return { dbPath: "/tmp/app.db", schemaVersion: 1 };
    if (command === "sync_status") return [];
    const handler = overrides[command];
    if (handler) return handler(args);
    throw { code: "IPC_ERROR", message: `command ${command} not found`, retryable: false };
  });
}

describe("迭代池创建与导出", () => {
  it("浏览器预览禁用写操作与导出，不展示假列表", () => {
    renderApp("/pool");
    expect(screen.getByText("未连接 Tauri 桌面端")).toBeTruthy();
    expect(screen.getByRole("button", { name: "导出 Markdown" })).toHaveProperty(
      "disabled",
      true,
    );
    expect(screen.getByRole("button", { name: "写入迭代池" })).toHaveProperty(
      "disabled",
      true,
    );
    expect(screen.queryByText("迭代池是空的")).toBeNull();
  });

  it("读取失败明确报错，不显示空池", async () => {
    bridgeWith({
      snapshot_list: () => {
        throw { code: "IPC_ERROR", message: "snapshot_list 未实现", retryable: false };
      },
    });
    renderApp("/pool");
    expect(await screen.findByText(/迭代池读取失败（IPC_ERROR）/)).toBeTruthy();
    expect(screen.queryByText("迭代池是空的")).toBeNull();
  });

  it("创建快照发送真实描述并刷新列表", async () => {
    const invoke = bridgeWith({
      snapshot_list: () => SNAPSHOTS,
      snapshot_create: () => ({
        id: "snap_03",
        title: "新问题",
        createdAt: 1763700010000,
      }),
    });
    renderApp("/pool");
    await screen.findByText("误把同事任务算到我头上");

    fireEvent.change(screen.getByLabelText("哪里错（必填）"), {
      target: { value: "来源标签丢失了会话名" },
    });
    fireEvent.change(screen.getByLabelText("期望的正确结果"), {
      target: { value: "保留群名与发送者" },
    });
    fireEvent.click(screen.getByRole("button", { name: "写入迭代池" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "snapshot_create",
        expect.objectContaining({
          route: "/pool",
          description: "来源标签丢失了会话名",
          expected: "保留群名与发送者",
        }),
      ),
    );
    expect(await screen.findByText("已记录：新问题")).toBeTruthy();
  });

  it("导出先选中快照，再传格式；取消不算成功", async () => {
    const invoke = bridgeWith({
      snapshot_list: () => SNAPSHOTS,
      snapshot_export: () => ({ status: "cancelled" }),
    });
    renderApp("/pool");
    await screen.findByText("误把同事任务算到我头上");

    const md = screen.getByRole("button", { name: "导出 Markdown" });
    expect(md).toHaveProperty("disabled", true);

    fireEvent.click(screen.getByLabelText("选择 误把同事任务算到我头上"));
    expect(md).toHaveProperty("disabled", false);
    fireEvent.click(md);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("snapshot_export", {
        ids: ["snap_01"],
        format: "md",
        includeBody: false,
      }),
    );
    expect(await screen.findByText("已取消，未写入文件。")).toBeTruthy();
  });

  it("包含正文必须输入确认词，且导出参数如实反映", async () => {
    const invoke = bridgeWith({
      snapshot_list: () => SNAPSHOTS,
      snapshot_export: () => ({ status: "saved", path: "/tmp/out.json" }),
    });
    renderApp("/pool");
    await screen.findByText("误把同事任务算到我头上");

    fireEvent.click(screen.getByLabelText("选择 误把同事任务算到我头上"));
    fireEvent.click(screen.getByLabelText("包含原始正文（敏感，默认关闭）"));
    const json = screen.getByRole("button", { name: "导出 JSON" });
    expect(json).toHaveProperty("disabled", true);

    fireEvent.change(screen.getByLabelText("输入正文以确认导出含正文"), {
      target: { value: "正文" },
    });
    expect(json).toHaveProperty("disabled", false);
    fireEvent.click(json);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("snapshot_export", {
        ids: ["snap_01"],
        format: "json",
        includeBody: true,
      }),
    );
    expect(await screen.findByText("/tmp/out.json", { exact: false })).toBeTruthy();
  });
});
