import { QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type {
  ArchiveStatus,
  EgressView,
  RuntimeConfigView,
} from "../lib/ipc";
import { installBridge, makeQueryClient } from "../test-utils";
import { ArchivePanel } from "./ArchivePanel";

const IDLE: ArchiveStatus = {
  running: false,
  scope: "当前账号全部可访问群聊与私聊；首次最近 168 小时，后续增量，含可下载附件",
  lastSuccessMs: null,
  conversations: 0,
  messages: 0,
  attachments: 0,
  downloaded: 0,
  failed: 0,
  error: null,
  autoJudge: false,
  profile: null,
  periodicCapture: false,
};

const READY_RUNTIME: RuntimeConfigView = {
  me: { userId: "me01", displayName: "我" },
  judge: { mode: "cli", modelId: "vendor/model-x" },
  egress: { policy: "full", approvedModel: "vendor/model-x", judgeMode: "cli" },
  firstPull: { lookbackHours: 24, minHours: 1, maxHours: 168 },
  trustedExecutables: ["dws", "omp"],
};

const READY_EGRESS: EgressView = {
  policy: "full",
  approvedModel: "vendor/model-x",
  judgeMode: "cli",
};

type Handlers = Record<string, (args?: Record<string, unknown>) => unknown>;

function bridgeWith(handlers: Handlers = {}) {
  return installBridge((command, args) => {
    const handler = handlers[command];
    if (handler) return handler(args);
    if (command === "runtime_config_get") return READY_RUNTIME;
    if (command === "egress_get") return READY_EGRESS;
    if (command === "archive_status") return IDLE;
    throw { code: "IPC_ERROR", message: `command ${command} not found`, retryable: false };
  });
}

async function renderPanel() {
  const view = render(
    <QueryClientProvider client={makeQueryClient()}>
      <ArchivePanel />
    </QueryClientProvider>,
  );
  await waitFor(() =>
    expect(
      screen.queryByText("正在读取归档状态…") ??
        screen.queryByText(/归档状态读取失败|未连接桌面端/),
    ).not.toBeNull(),
  );
  return view;
}

describe("全可访问会话同步", () => {
  it("浏览器预览不读取状态，按钮禁用，不显示假成功", async () => {
    await renderPanel();

    expect(screen.getByText(/未连接桌面端/)).toBeTruthy();
    expect(
      screen.getByRole("button", { name: "立即同步全可访问会话" }),
    ).toHaveProperty("disabled", true);
    expect(screen.getByLabelText("允许自动判读新同步内容")).toHaveProperty(
      "disabled",
      true,
    );
    expect(screen.queryByText(/最近一次成功/)).toBeNull();
    expect(screen.queryByText(/已登记/)).toBeNull();
  });

  it("区分登记数与下载成功数，并原样显示归档错误", async () => {
    bridgeWith({
      archive_status: () => ({
        ...IDLE,
        lastSuccessMs: 1763700000000,
        conversations: 12,
        messages: 340,
        attachments: 9,
        downloaded: 5,
        failed: 3,
        error: "归档器退出码 1；查看归档失败记录",
      }),
    });
    await renderPanel();

    expect(await screen.findByText("登记会话")).toBeTruthy();
    expect(screen.getByText("12")).toBeTruthy();
    expect(screen.getByText("340")).toBeTruthy();
    expect(screen.getByText("登记附件")).toBeTruthy();
    expect(screen.getByText("9")).toBeTruthy();
    expect(screen.getByText("下载成功")).toBeTruthy();
    expect(screen.getByText("5")).toBeTruthy();
    expect(screen.getByText("下载失败")).toBeTruthy();
    expect(screen.getByText("3")).toBeTruthy();
    expect(screen.getByText(/归档器退出码 1/)).toBeTruthy();
    expect(screen.getByText(/3 个附件下载失败/)).toBeTruthy();
    expect(screen.getByText(/登记 12 个会话 \/ 340 条消息 \/ 9 个附件/)).toBeTruthy();
    expect(screen.getByText(/附件下载成功 5 个，失败 3 个/)).toBeTruthy();
  });

  it("同步中轮询，完成前不报告成功，完成后展示真实结果", async () => {
    let status: ArchiveStatus = IDLE;
    let statusCallsWhileRunning = 0;
    const completed: ArchiveStatus = {
      ...IDLE,
      lastSuccessMs: 1763700123000,
      conversations: 3,
      messages: 21,
      attachments: 4,
      downloaded: 3,
      failed: 1,
    };
    const invoke = bridgeWith({
      archive_status: () => {
        if (!status.running) return status;
        statusCallsWhileRunning += 1;
        if (statusCallsWhileRunning >= 2) status = completed;
        return status;
      },
      archive_sync_now: () => {
        status = { ...IDLE, running: true };
        return null;
      },
    });
    await renderPanel();
    await screen.findByText("登记会话");

    fireEvent.click(screen.getByRole("button", { name: "立即同步全可访问会话" }));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("archive_sync_now", {}),
    );
    await waitFor(() => {
      const summary = document.querySelector("p[aria-live='polite']");
      expect(summary?.textContent).toContain("正在同步：每 1.5 秒读取一次真实状态");
      expect(summary?.textContent).not.toContain("附件下载成功 3 个");
    });

    expect(
      await screen.findByText(/最近一次成功/, { exact: false }, { timeout: 5000 }),
    ).toBeTruthy();
    expect(
      screen.getByText(
        /登记 3 个会话 \/ 21 条消息 \/ 4 个附件；其中附件下载成功 3 个，失败 1 个/,
      ),
    ).toBeTruthy();
    expect(
      invoke.mock.calls.filter(([command]) => command === "archive_status").length,
    ).toBeGreaterThanOrEqual(2);
  }, 10000);

  it("自动判读缺少精确模型或出域授权时禁用并解释原因", async () => {
    bridgeWith({
      runtime_config_get: () => ({
        ...READY_RUNTIME,
        judge: { mode: "mock", modelId: null },
        egress: { policy: "none", approvedModel: null, judgeMode: "mock" },
      }),
      egress_get: () => ({ policy: "none", approvedModel: null, judgeMode: "mock" }),
    });
    await renderPanel();

    const toggle = (await screen.findByLabelText(
      "允许自动判读新同步内容",
    )) as HTMLInputElement;
    expect(toggle.disabled).toBe(true);
    expect(screen.getByText(/未配置精确 provider\/model id/)).toBeTruthy();
    expect(screen.getByText(/判读模式为 mock/)).toBeTruthy();
    expect(screen.getByText(/出域策略为 none/)).toBeTruthy();
    expect(screen.getByText(/判读只选取必要的上下文/)).toBeTruthy();
    expect(screen.getByText(/附件抽取内容/)).toBeTruthy();
  });

  it("精确模型与出域授权齐备后，开关走 IPC 并以刷新后的真实状态为准", async () => {
    let status: ArchiveStatus = IDLE;
    const invoke = bridgeWith({
      archive_status: () => status,
      archive_auto_set: (args) => {
        status = { ...IDLE, autoJudge: args?.enabled === true };
        return null;
      },
    });
    await renderPanel();

    const toggle = (await screen.findByLabelText(
      "允许自动判读新同步内容",
    )) as HTMLInputElement;
    await waitFor(() => expect(toggle.disabled).toBe(false));
    expect(toggle.checked).toBe(false);
    fireEvent.click(toggle);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("archive_auto_set", { enabled: true }),
    );
    await waitFor(() => expect(toggle.checked).toBe(true));
  });

  it("关闭自动判读不要求模型或出域授权", async () => {
    const invoke = bridgeWith({
      archive_status: () => ({ ...IDLE, autoJudge: true }),
      runtime_config_get: () => ({
        ...READY_RUNTIME,
        judge: { mode: "mock", modelId: null },
        egress: { policy: "none", approvedModel: null, judgeMode: "mock" },
      }),
      egress_get: () => ({ policy: "none", approvedModel: null, judgeMode: "mock" }),
      archive_auto_set: () => null,
    });
    await renderPanel();

    const toggle = (await screen.findByLabelText(
      "允许自动判读新同步内容",
    )) as HTMLInputElement;
    await waitFor(() => expect(toggle.checked).toBe(true));
    expect(toggle.disabled).toBe(false);
    fireEvent.click(toggle);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("archive_auto_set", { enabled: false }),
    );
  });

  it("模型因身份不符停止时显示原因和未扫描记录，不冒认整轮完成", async () => {
    bridgeWith({
      archive_judge_now: () => ({ status: "stopped", scanned: 12, more: true, calls: [{status: "blocked", errorCode: "AGENT_MODEL_NOT_APPROVED"}] }),
    });
    await renderPanel();

    const judgeButton = await screen.findByRole("button", {
      name: "立即判读已同步内容",
    });
    await waitFor(() => expect(judgeButton).toHaveProperty("disabled", false));
    fireEvent.click(judgeButton);

    expect(await screen.findByText(/本轮提前停止/)).toBeTruthy();
    expect(screen.getByText(/AGENT_MODEL_NOT_APPROVED/)).toBeTruthy();
    expect(screen.getByText(/仍有未扫描记录/)).toBeTruthy();
  });

  it("只有 redacted 出域批准时不放行自动判读与立即判读", async () => {
    const invoke = bridgeWith({
      runtime_config_get: () => ({
        ...READY_RUNTIME,
        egress: { policy: "redacted", approvedModel: "vendor/model-x", judgeMode: "cli" },
      }),
      egress_get: () => ({
        policy: "redacted",
        approvedModel: "vendor/model-x",
        judgeMode: "cli",
      }),
    });
    await renderPanel();

    const toggle = (await screen.findByLabelText(
      "允许自动判读新同步内容",
    )) as HTMLInputElement;
    expect(toggle.disabled).toBe(true);
    expect(
      screen.getByText(/出域策略为 redacted：自动判读需要对该模型的 full 出域批准/),
    ).toBeTruthy();
    expect(
      screen.getByRole("button", { name: "立即判读已同步内容" }),
    ).toHaveProperty("disabled", true);
    expect(
      invoke.mock.calls.some(([command]) => command === "archive_judge_now"),
    ).toBe(false);
  });

  it("结构不符时报 IPC_BAD_SHAPE，不把半截数据当空状态", async () => {
    bridgeWith({ archive_status: () => ({ running: false }) });
    await renderPanel();

    expect(
      await screen.findByText(/归档状态读取失败（IPC_BAD_SHAPE）/),
    ).toBeTruthy();
    expect(screen.queryByText("登记会话")).toBeNull();
  });
});
