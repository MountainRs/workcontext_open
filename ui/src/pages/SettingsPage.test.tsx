import { fireEvent, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { formatDateTime } from "../lib/format";
import type { Diagnostics, RuntimeConfigView } from "../lib/ipc";
import { installBridge, renderApp } from "../test-utils";

const DIAGNOSTICS: Diagnostics = {
  app: {
    version: "0.1.3",
    platform: "macos-universal",
    schemaVersion: 1,
    supportedSchemaVersion: 1,
  },
  db: {
    path: "/tmp/app.db",
    integrity: "ok",
    integrityDetail: null,
    sizeBytes: 4096,
    counts: { sourceRecord: 12, workItem: 3, workEvent: 9, feedbackSnapshot: 2 },
  },
  backups: {
    count: 1,
    last: { path: "/tmp/backups/b1/app.db", createdAt: 1763700000000, sizeBytes: 2048 },
  },
  sources: [],
  judge: {
    windowDays: 7,
    ok: 5,
    rejected: 1,
    failed: 0,
    timeout: 0,
    rejectionRules: [{ rule: "V03_OWNERSHIP_ANCHOR", count: 1 }],
  },
};

const BACKUPS = [
  {
    path: "/tmp/backups/b1/app.db",
    createdAt: 1763700000000,
    sizeBytes: 2048,
    schemaVersion: 1,
    appVersion: "0.1.3",
    sha256Verified: true,
  },
];

const RUNTIME: RuntimeConfigView = {
  me: { userId: "", displayName: null },
  judge: { mode: "mock", modelId: null },
  egress: { policy: "none", approvedModel: null, judgeMode: "mock" },
  firstPull: { lookbackHours: 24, minHours: 1, maxHours: 168 },
  trustedExecutables: ["dws", "omp"],
};

function bridgeWith(
  overrides: Record<string, (args?: Record<string, unknown>) => unknown> = {},
) {
  return installBridge((command, args) => {
    if (command === "db_info") return { dbPath: "/tmp/app.db", schemaVersion: 1 };
    if (command === "sync_status") return [];
    if (command === "source_list") return [];
    const handler = overrides[command];
    if (handler) return handler(args);
    if (command === "runtime_config_get") return RUNTIME;
    if (command === "backup_list") return BACKUPS;
    if (command === "diagnostics") return DIAGNOSTICS;
    throw { code: "IPC_ERROR", message: `command ${command} not found`, retryable: false };
  });
}

describe("工作上下文自动整理", () => {
  it("在设置页授权、整理、重试，并在账号变更后撤回原授权", async () => {
    const model = "workbuddy/deepseek-v4.1-flash";
    const profile = "synthetic:desk";
    const queue = { pending: 3, claimed: 0, retrying: 1, organized: 7, noRecord: 2, checkpoint: 17,
      oldestPendingAt: 1763700000000, lastOrganizedAt: 1763700100000, nextRetryAt: 1763700200000, lastError: "model_timeout" };
    let state = { profile, model, credentialsAvailable: true, consentProfile: null as string | null, enabled: false, running: false,
      blocked: null as string | null, error: null, queue, lastCheckedAt: 1763700300000 };
    const invoke = bridgeWith({ organizer_status: () => state,
      organizer_set_consent: args => state = { ...state, enabled: args?.enabled === true, consentProfile: args?.enabled ? profile : null },
      organizer_run: () => state,
      organizer_retry: () => state = { ...state, blocked: "授权账号已变更", profile: "synthetic:other" },
    });
    renderApp("/settings");
    const toggle = await screen.findByRole("switch", { name: "工作上下文自动整理" });
    expect(await screen.findByText("已接收归档变更序号 17")).toBeTruthy();
    expect(screen.getByText(`整理模型：${model}（本地网关 127.0.0.1:3425）`)).toBeTruthy();
    expect(screen.getByText(/待整理 3 · 已认领 0 · 待重试 1 · 已整理 7 · 无需建档 2/)).toBeTruthy();
    const run = screen.getByRole("button", { name: "立即整理" });
    expect(run).toHaveProperty("disabled", true);
    fireEvent.click(toggle);
    await waitFor(() => expect(run).toHaveProperty("disabled", false));
    expect(invoke).toHaveBeenCalledWith("organizer_set_consent", { profile, model, enabled: true });
    fireEvent.click(run);
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("organizer_run", { profile, model }));
    const retry = screen.getByRole("button", { name: "立即重试" });
    await waitFor(() => expect(retry).toHaveProperty("disabled", false));
    fireEvent.click(retry);
    await screen.findByText("授权账号已变更");
    expect(invoke).toHaveBeenCalledWith("organizer_retry", { profile, model });
    expect(run).toHaveProperty("disabled", true);
    fireEvent.click(toggle);
    await waitFor(() => expect(toggle.getAttribute("aria-checked")).toBe("false"));
    expect(invoke).toHaveBeenCalledWith("organizer_set_consent", { profile, model, enabled: false });
  });
});

describe("会话清单设置", () => {
  it("单次整理不打开自动授权，无凭据时仍能撤销已开启的自动整理", async () => {
    const state = { profile: "demo:me", report: null, running: false, canRun: true, autoEnabled: false };
    const invoke = bridgeWith({
      ownership_view: () => ({ ...state }),
      ownership_refresh: () => undefined,
      ownership_set_auto: args => {
        state.autoEnabled = args?.enabled === true;
        state.canRun = false;
      },
    });
    renderApp("/settings#ownership-settings");
    expect(document.activeElement).toBe(screen.getByRole("heading", { name: "会话清单" }));
    const toggle = await screen.findByRole("switch", { name: "自动整理" });
    await waitFor(() => expect(toggle).toHaveProperty("disabled", false));
    fireEvent.click(screen.getByRole("button", { name: "仅整理一次" }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("ownership_refresh", { profile: state.profile }));
    await waitFor(() => expect(toggle).toHaveProperty("disabled", false));
    expect(toggle.getAttribute("aria-checked")).toBe("false");
    expect(invoke.mock.calls.some(([command]) => command === "ownership_set_auto")).toBe(false);
    fireEvent.click(toggle);
    await waitFor(() => expect(toggle.getAttribute("aria-checked")).toBe("true"));
    expect(screen.getByRole("button", { name: "仅整理一次" })).toHaveProperty("disabled", true);
    fireEvent.click(toggle);
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("ownership_set_auto", { profile: state.profile, enabled: false }));
    await waitFor(() => expect(toggle.getAttribute("aria-checked")).toBe("false"));
  });

  it.each(["立即重试", "关闭自动整理"])("显示已保存的重试时间，%s 后收起提示", async (action) => {
    const retryAt = Date.UTC(2026, 10, 23, 12, 30);
    const state = {
      profile: "org:me", report: { cutoff: "2026-11-22", cards: [] }, running: false,
      canRun: true, autoEnabled: true, autoRetryAt: retryAt as number | null,
    };
    const invoke = bridgeWith({
      ownership_view: () => ({ ...state }),
      ownership_refresh: () => { state.autoRetryAt = null; },
      ownership_set_auto: () => { state.autoEnabled = false; },
    });
    renderApp("/settings#ownership-settings");
    const hint = await screen.findByText(/自动重试时间/);
    expect(hint.textContent).toContain(formatDateTime(retryAt));
    expect(hint.textContent).toContain("上次成功的清单仍可查看");
    const retry = screen.getByRole("button", { name: "立即重试" });
    if (action === "立即重试") {
      fireEvent.click(retry);
      await waitFor(() => expect(invoke).toHaveBeenCalledWith("ownership_refresh", { profile: state.profile }));
      expect(invoke.mock.calls.some(([command]) => command === "ownership_set_auto")).toBe(false);
    } else {
      fireEvent.click(screen.getByRole("switch", { name: "自动整理" }));
      await waitFor(() => expect(invoke).toHaveBeenCalledWith("ownership_set_auto", { profile: state.profile, enabled: false }));
    }
    await screen.findByRole("button", { name: "仅整理一次" });
    expect(screen.queryByText(/自动重试时间/)).toBeNull();
  });
});

describe("设置：备份恢复与诊断", () => {
  it("浏览器预览不读备份、不启用破坏性操作", () => {
    renderApp("/settings");
    expect(screen.getByText("未连接 Tauri 桌面端")).toBeTruthy();
    expect(screen.getByRole("button", { name: "立即备份" })).toHaveProperty(
      "disabled",
      true,
    );
    expect(
      screen.queryByRole("button", { name: "从所选备份恢复" }),
    ).toBeNull();
  });

  it("立即备份调用 IPC 并显示真实结果", async () => {
    const invoke = bridgeWith({
      backup_now: () => ({
        path: "/tmp/backups/b2/app.db",
        createdAt: 1763700010000,
        sizeBytes: 4096,
        schemaVersion: 1,
        appVersion: "0.1.3",
      }),
    });
    renderApp("/settings");
    await screen.findByText("/tmp/backups/b1/app.db", { exact: false });

    fireEvent.click(screen.getByRole("button", { name: "立即备份" }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("backup_now", {}));
    expect(await screen.findByText(/已创建备份：\/tmp\/backups\/b2\/app\.db/)).toBeTruthy();
  });

  it("恢复需要选中备份 + 输入「恢复」，取消时不声称成功", async () => {
    const invoke = bridgeWith({
      restore_from: () => ({ status: "cancelled" }),
    });
    renderApp("/settings");
    await screen.findByText("/tmp/backups/b1/app.db", { exact: false });

    const restoreButton = screen.getByRole("button", { name: "从所选备份恢复" });
    expect(restoreButton).toHaveProperty("disabled", true);

    fireEvent.click(screen.getByRole("radio"));
    expect(restoreButton).toHaveProperty("disabled", true);

    fireEvent.change(screen.getByLabelText("输入恢复以确认"), {
      target: { value: "恢复" },
    });
    expect(restoreButton).toHaveProperty("disabled", false);
    fireEvent.click(restoreButton);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("restore_from", {
        path: "/tmp/backups/b1/app.db",
        confirmToken: "恢复",
      }),
    );
    expect(
      await screen.findByText("已取消（原生确认框未确认），当前数据库未改动。"),
    ).toBeTruthy();
    expect(screen.queryByText(/已恢复/)).toBeNull();
  });

  it("诊断展示真实计数与拒绝规则，导出走 IPC", async () => {
    const invoke = bridgeWith({
      diagnostics_export: () => ({ status: "saved", path: "/tmp/diag.json" }),
    });
    renderApp("/settings");

    expect(await screen.findByText("ok")).toBeTruthy();
    expect(screen.getByText(/来源 12 \/ 事项 3 \/ 事件 9 \/ 快照 2/)).toBeTruthy();
    expect(screen.getByText("V03_OWNERSHIP_ANCHOR")).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "导出诊断包" }));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("diagnostics_export", {}),
    );
    expect(await screen.findByText("诊断包已导出：/tmp/diag.json")).toBeTruthy();
  });

  it("备份列表失败明确报错，不显示「没有备份」", async () => {
    bridgeWith({
      backup_list: () => {
        throw { code: "DB_CORRUPT", message: "数据库损坏", retryable: false };
      },
    });
    renderApp("/settings");
    expect(await screen.findByText(/备份列表读取失败（DB_CORRUPT）/)).toBeTruthy();
    expect(screen.queryByText(/本地还没有备份/)).toBeNull();
  });

  it("全可访问会话同步面板挂载在设置页，展示 IPC 真实状态", async () => {
    bridgeWith({
      archive_status: () => ({
        running: false,
        scope: "当前账号全部可访问群聊与私聊",
        lastSuccessMs: 1763700000000,
        conversations: 2,
        messages: 7,
        attachments: 1,
        downloaded: 1,
        failed: 0,
        error: null,
        autoJudge: false,
        profile: null,
        periodicCapture: false,
      }),
    });
    renderApp("/sources");

    expect(await screen.findByText("全可访问会话同步")).toBeTruthy();
    expect(await screen.findByText("当前账号全部可访问群聊与私聊", { exact: false })).toBeTruthy();
    expect(screen.getByRole("button", { name: "立即同步全可访问会话" })).toHaveProperty(
      "disabled",
      false,
    );
  });
});

describe("运行时配置：本人身份 / 判读模型 / 首次拉取回溯", () => {
  it("展示当前事实；无变更时保存禁用", async () => {
    bridgeWith();
    renderApp("/settings");

    const id = (await screen.findByLabelText("本人 ID")) as HTMLInputElement;
    await waitFor(() => expect(id.value).toBe(""));
    expect(screen.getByText("dws / omp")).toBeTruthy();
    expect(screen.getByRole("button", { name: "保存运行时配置" })).toHaveProperty(
      "disabled",
      true,
    );
    expect(screen.getByText(/结束边界开放/)).toBeTruthy();
  });

  it("只提交变化的五个键，保存后显示后端返回值", async () => {
    const invoke = bridgeWith({
      runtime_configure: () => ({
        me: { userId: "me01", displayName: "我" },
        judge: { mode: "cli", modelId: "vendor/model-x" },
        egress: { policy: "none", approvedModel: null, judgeMode: "cli" },
        firstPull: { lookbackHours: 48, minHours: 1, maxHours: 168 },
        trustedExecutables: ["dws", "omp"],
      }),
    });
    renderApp("/settings");
    await screen.findByLabelText("本人 ID");

    fireEvent.change(screen.getByLabelText("本人 ID"), {
      target: { value: " me01 " },
    });
    fireEvent.change(screen.getByLabelText("显示名"), {
      target: { value: "我" },
    });
    fireEvent.change(screen.getByLabelText("判读模式"), {
      target: { value: "cli" },
    });
    fireEvent.change(screen.getByLabelText("精确模型 ID"), {
      target: { value: "vendor/model-x" },
    });
    fireEvent.change(screen.getByLabelText("首次拉取回溯"), {
      target: { value: "48" },
    });

    const save = screen.getByRole("button", { name: "保存运行时配置" });
    expect(save).toHaveProperty("disabled", false);
    fireEvent.click(save);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("runtime_configure", {
        args: {
          meUserId: "me01",
          meDisplayName: "我",
          judgeMode: "cli",
          modelId: "vendor/model-x",
          firstPullLookbackHours: 48,
        },
      }),
    );
    expect(await screen.findByText(/已保存运行时配置：本人 me01（我）/)).toBeTruthy();
    expect(screen.getByText(/回溯 48 小时（下次授权时生效）/)).toBeTruthy();
  });

  it("换模型后出域批准关闭并明确提示", async () => {
    let view: RuntimeConfigView = {
      me: { userId: "me01", displayName: "我" },
      judge: { mode: "cli", modelId: "vendor/model-a" },
      egress: { policy: "full", approvedModel: "vendor/model-a", judgeMode: "cli" },
      firstPull: { lookbackHours: 24, minHours: 1, maxHours: 168 },
      trustedExecutables: ["dws", "omp"],
    };
    const invoke = bridgeWith({
      runtime_config_get: () => view,
      runtime_configure: () => {
        view = {
          ...view,
          judge: { mode: "cli", modelId: "vendor/model-b" },
          egress: { policy: "none", approvedModel: null, judgeMode: "cli" },
        };
        return view;
      },
    });
    renderApp("/settings");
    await waitFor(() =>
      expect((screen.getByLabelText("精确模型 ID") as HTMLInputElement).value).toBe(
        "vendor/model-a",
      ),
    );
    expect(screen.getByText(/full · 已批准 vendor\/model-a/)).toBeTruthy();

    fireEvent.change(screen.getByLabelText("精确模型 ID"), {
      target: { value: "vendor/model-b" },
    });
    fireEvent.click(screen.getByRole("button", { name: "保存运行时配置" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("runtime_configure", {
        args: { modelId: "vendor/model-b" },
      }),
    );
    expect(
      await screen.findByText(/出域批准已随模型变更关闭（策略回到 none）/),
    ).toBeTruthy();
  });

  it("cli 缺模型或回溯越界时拒绝保存", async () => {
    const invoke = bridgeWith();
    renderApp("/settings");
    await screen.findByLabelText("判读模式");

    fireEvent.change(screen.getByLabelText("判读模式"), {
      target: { value: "cli" },
    });
    expect(screen.getByRole("button", { name: "保存运行时配置" })).toHaveProperty(
      "disabled",
      true,
    );
    expect(screen.getByText(/cli 判读需要先填写精确模型 ID/)).toBeTruthy();

    fireEvent.change(screen.getByLabelText("精确模型 ID"), {
      target: { value: "vendor/model-x" },
    });
    fireEvent.change(screen.getByLabelText("首次拉取回溯"), {
      target: { value: "169" },
    });
    expect(screen.getByRole("button", { name: "保存运行时配置" })).toHaveProperty(
      "disabled",
      true,
    );
    expect(screen.getByText(/须为 1–168 的整数小时/)).toBeTruthy();

    fireEvent.change(screen.getByLabelText("首次拉取回溯"), {
      target: { value: "24" },
    });
    fireEvent.change(screen.getByLabelText("精确模型 ID"), {
      target: { value: "no-slash" },
    });
    expect(screen.getByRole("button", { name: "保存运行时配置" })).toHaveProperty(
      "disabled",
      true,
    );
    expect(screen.getByText(/模型须为精确的 provider\/model id/)).toBeTruthy();
    expect(
      invoke.mock.calls.some(([command]) => command === "runtime_configure"),
    ).toBe(false);
  });

  it("运行时配置读取失败明确报错，不回退假默认值", async () => {
    bridgeWith({
      runtime_config_get: () => {
        throw {
          code: "INVALID_ARGUMENT",
          message: "运行时配置损坏",
          retryable: false,
        };
      },
    });
    renderApp("/settings");

    expect(
      await screen.findByText(/运行时配置读取失败（INVALID_ARGUMENT）/),
    ).toBeTruthy();
    expect(screen.queryByLabelText("本人 ID")).toBeNull();
  });
});
