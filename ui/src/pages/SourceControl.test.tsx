import { fireEvent, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import type { Diagnostics, EgressView, RuntimeConfigView, SourceView } from "../lib/ipc";
import {
  emitTauriEvent,
  hasTauriListener,
  installBridge,
  renderApp,
  removeBridge,
} from "../test-utils";

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
    counts: { sourceRecord: 0, workItem: 0, workEvent: 0, feedbackSnapshot: 0 },
  },
  backups: { count: 0, last: null },
  sources: [],
  judge: {
    windowDays: 7,
    ok: 0,
    rejected: 0,
    failed: 0,
    timeout: 0,
    rejectionRules: [],
  },
};

const SOURCE: SourceView = {
  id: "src_1",
  kind: "dws_im",
  displayName: "OA 项目群",
  scopeSummary: "2 个会话",
  enabled: false,
  authorizedAt: null,
  revokedAt: null,
  createdAt: 1763700000000,
};

const EGRESS_NONE: EgressView = {
  policy: "none",
  approvedModel: null,
  judgeMode: "mock",
};

const RUNTIME: RuntimeConfigView = {
  me: { userId: "me01", displayName: "我" },
  judge: { mode: "mock", modelId: null },
  egress: EGRESS_NONE,
  firstPull: { lookbackHours: 24, minHours: 1, maxHours: 168 },
  trustedExecutables: ["dws", "omp"],
};

type Overrides = {
  sources?: SourceView[];
  syncStatus?: unknown[];
  egress?: EgressView;
  runtime?: RuntimeConfigView;
  commands?: Record<string, (args?: Record<string, unknown>) => unknown>;
};

function bridgeWith(overrides: Overrides = {}) {
  return installBridge((command, args) => {
    if (command === "db_info") return { dbPath: "/tmp/app.db", schemaVersion: 1 };
    if (command === "sync_status") return overrides.syncStatus ?? [];
    if (command === "source_list") return overrides.sources ?? [];
    if (command === "backup_list") return [];
    if (command === "diagnostics") return DIAGNOSTICS;
    if (command === "egress_get") return overrides.egress ?? EGRESS_NONE;
    const custom = overrides.commands?.[command];
    if (custom) return custom(args);
    if (command === "runtime_config_get") return overrides.runtime ?? RUNTIME;
    throw { code: "IPC_ERROR", message: `command ${command} not found`, retryable: false };
  });
}

function source(overrides: Partial<SourceView> = {}): SourceView {
  return { ...SOURCE, ...overrides };
}

afterEach(() => removeBridge());

describe("来源控制：注册 / 授权 / 撤权 / 手动同步", () => {
  it("注册来源原样发送白名单，并明确提示尚未授权", async () => {
    const invoke = bridgeWith({
      commands: {
        source_register: () => source({ id: "corp-user" }),
      },
    });
    renderApp("/sources");

    fireEvent.change(screen.getByLabelText("来源显示名称"), {
      target: { value: "OA 项目群" },
    });
    fireEvent.change(screen.getByLabelText("dws profile"), {
      target: { value: "corp:user" },
    });
    fireEvent.change(screen.getByLabelText("白名单会话 ID"), {
      target: { value: "cid_a\ncid_b" },
    });
    fireEvent.click(screen.getByRole("button", { name: "注册来源" }));

    await waitFor(() => {
      const call = invoke.mock.calls.find(([command]) => command === "source_register");
      expect(call).toBeTruthy();
      expect(call?.[1]).toMatchObject({
        displayName: "OA 项目群",
        profile: "corp:user",
        scope: { conversations: ["cid_a", "cid_b"], people: [] },
      });
    });
    expect(await screen.findByText(/尚未授权/)).toBeTruthy();
    expect(screen.queryByText(/已注册「OA 项目群」.*已授权/)).toBeNull();
  });

  it("注册按钮在缺少白名单或 profile 时保持禁用", async () => {
    bridgeWith();
    renderApp("/sources");

    fireEvent.change(screen.getByLabelText("来源显示名称"), {
      target: { value: "OA 项目群" },
    });
    const button = screen.getByRole("button", { name: "注册来源" });
    expect(button).toHaveProperty("disabled", true);

    fireEvent.change(screen.getByLabelText("dws profile"), {
      target: { value: "corp:user" },
    });
    expect(button).toHaveProperty("disabled", true);

    fireEvent.change(screen.getByLabelText("白名单账户 ID"), {
      target: { value: "staff_9" },
    });
    expect(button).toHaveProperty("disabled", false);
  });

  it("授权需要二次点击，且只发送 enabled=true", async () => {
    const invoke = bridgeWith({
      sources: [source()],
      commands: {
        source_set_enabled: () => source({ enabled: true, authorizedAt: 1763700100000 }),
      },
    });
    renderApp("/sources");
    await screen.findByText("OA 项目群", { exact: false });

    fireEvent.click(screen.getByRole("button", { name: "授权" }));
    expect(
      invoke.mock.calls.some(([command]) => command === "source_set_enabled"),
    ).toBe(false);
    expect(screen.getByRole("button", { name: "确认授权" })).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "确认授权" }));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("source_set_enabled", {
        id: "src_1",
        enabled: true,
      }),
    );
    expect(await screen.findByText(/已授权「OA 项目群」/)).toBeTruthy();
  });

  it("撤权需要二次点击，并发送 enabled=false", async () => {
    const invoke = bridgeWith({
      sources: [source({ enabled: true, authorizedAt: 1763700000000 })],
      commands: {
        source_set_enabled: () => source({ enabled: false, revokedAt: 1763700200000 }),
      },
    });
    renderApp("/sources");
    await screen.findByText("OA 项目群", { exact: false });

    fireEvent.click(screen.getByRole("button", { name: "撤权" }));
    expect(
      invoke.mock.calls.some(([command]) => command === "source_set_enabled"),
    ).toBe(false);

    fireEvent.click(screen.getByRole("button", { name: "确认撤权" }));
    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("source_set_enabled", {
        id: "src_1",
        enabled: false,
      }),
    );
    expect(await screen.findByText(/已撤权「OA 项目群」/)).toBeTruthy();
  });

  it("手动同步显示真实 jobId 与 sync://progress 结果", async () => {
    const invoke = bridgeWith({
      sources: [source({ enabled: true, authorizedAt: 1763700000000 })],
      commands: {
        sync_now: () => ({ jobId: "job_1", sources: ["src_1"] }),
      },
    });
    renderApp("/sources");
    await screen.findByText("OA 项目群", { exact: false });
    await waitFor(() => expect(hasTauriListener("sync://progress")).toBe(true));

    fireEvent.click(screen.getByRole("button", { name: /手动同步全部已授权来源/ }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("sync_now", {}));
    expect((await screen.findAllByText(/job_1/)).length).toBeGreaterThan(0);

    emitTauriEvent("sync://progress", {
      jobId: "job_1",
      phase: "source",
      sourceId: "src_1",
      outcome: {
        sourceId: "src_1",
        runId: "run_1",
        status: "ok",
        pages: 2,
        fetched: 5,
        outOfScope: 1,
        inserted: 2,
        unchanged: 2,
        updated: 1,
        cursorAdvanced: true,
        error: null,
      },
      error: null,
    });

    const results = await screen.findByText(/新增 2/);
    const item = results.closest("li");
    expect(item?.textContent).toContain("OA 项目群");
    expect(item?.textContent).toContain("拉取 5");
    expect(item?.textContent).toContain("白名单外 1");
    expect(item?.textContent).toContain("游标已推进");

    emitTauriEvent("sync://progress", {
      jobId: "job_1",
      phase: "done",
      sourceId: null,
      outcome: null,
      error: null,
    });
    expect(await screen.findByText(/整轮结束/)).toBeTruthy();
  });

  it("未授权来源不能触发「同步此来源」", async () => {
    bridgeWith({ sources: [source()] });
    renderApp("/sources");
    expect((await screen.findAllByText(/OA 项目群/)).length).toBeGreaterThan(0);
    const row = screen.getByRole("button", { name: "同步此来源" });
    expect(row).toHaveProperty("disabled", true);
  });
});

describe("判读与出域：默认 none，切换需确认", () => {
  it("默认 none 时保存按钮禁用，不自动出域", async () => {
    bridgeWith();
    renderApp("/settings");

    const policy = (await screen.findByLabelText("目标出域策略")) as HTMLSelectElement;
    expect(policy.value).toBe("none");
    expect(screen.getByRole("button", { name: "保存出域策略" })).toHaveProperty(
      "disabled",
      true,
    );
    expect(screen.getByText(/默认 `none`/)).toBeTruthy();
  });

  it("切换到 redacted 需要已批准模型 + 逐字确认词", async () => {
    const invoke = bridgeWith({
      commands: {
        egress_set: () => ({
          policy: "redacted",
          approvedModel: "workbuddy/deepseek-v4.1-flash",
          judgeMode: "mock",
        }),
      },
    });
    renderApp("/settings");
    await screen.findByLabelText("目标出域策略");

    fireEvent.change(screen.getByLabelText("目标出域策略"), {
      target: { value: "redacted" },
    });
    const save = screen.getByRole("button", { name: "保存出域策略" });
    expect(save).toHaveProperty("disabled", true);

    fireEvent.change(screen.getByLabelText("已批准模型"), {
      target: { value: "workbuddy/deepseek-v4.1-flash" },
    });
    fireEvent.change(screen.getByLabelText("出域策略确认词"), {
      target: { value: "redacte" },
    });
    expect(save).toHaveProperty("disabled", true);

    fireEvent.change(screen.getByLabelText("出域策略确认词"), {
      target: { value: "redacted" },
    });
    expect(save).toHaveProperty("disabled", false);
    fireEvent.click(save);

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("egress_set", {
        policy: "redacted",
        approvedModel: "workbuddy/deepseek-v4.1-flash",
        confirmToken: "redacted",
      }),
    );
    expect(await screen.findByText(/已写入出域策略：redacted/)).toBeTruthy();
  });

  it("收紧回 none 不需要模型，确认词仍逐字校验", async () => {
    const invoke = bridgeWith({
      egress: {
        policy: "redacted",
        approvedModel: "workbuddy/deepseek-v4.1-flash",
        judgeMode: "mock",
      },
      commands: {
        egress_set: () => EGRESS_NONE,
      },
    });
    renderApp("/settings");
    const policy = (await screen.findByLabelText("目标出域策略")) as HTMLSelectElement;
    await waitFor(() => expect(policy.value).toBe("redacted"));

    fireEvent.change(screen.getByLabelText("目标出域策略"), {
      target: { value: "none" },
    });
    fireEvent.change(screen.getByLabelText("出域策略确认词"), {
      target: { value: "none" },
    });
    fireEvent.click(screen.getByRole("button", { name: "保存出域策略" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("egress_set", {
        policy: "none",
        approvedModel: undefined,
        confirmToken: "none",
      }),
    );
  });

  it("judge://done 展示真实批次计数与出域审计", async () => {
    const invoke = bridgeWith({
      commands: {
        judge_now: () => ({ jobId: "judge_1" }),
      },
    });
    renderApp("/settings");
    await waitFor(() => expect(hasTauriListener("judge://done")).toBe(true));

    fireEvent.click(screen.getByRole("button", { name: "判读待处理记录" }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("judge_now", {}));
    expect((await screen.findAllByText(/judge_1/)).length).toBeGreaterThan(0);

    emitTauriEvent("judge://done", {
      jobId: "judge_1",
      requeued: null,
      summary: {
        batches: [{ status: "ok" }, { status: "blocked" }, { status: "rejected" }],
        pendingLeft: 3,
        skippedMeta: 1,
      },
      egress: { blockedBatches: 1, allowedBatches: 0 },
      error: null,
    });

    const card = (await screen.findByText(/批次 3/)).closest("article");
    await waitFor(() => expect(card).toBeTruthy());
    const text = card?.textContent ?? "";
    expect(text).toContain("通过 1");
    expect(text).toContain("拒绝 1");
    expect(text).toContain("出域拦截 1");
    expect(text).toContain("仍待判读 3");
    expect(text).toContain("缺元数据跳过 1");
    expect(card?.textContent).toContain("拦截批次 1");
    expect(card?.textContent).toContain("放行批次 0");
  });
});

describe("浏览器预览：来源控制不发请求", () => {
  it("注册、同步、授权按钮全部禁用且不出域面板请求", () => {
    removeBridge();
    renderApp("/sources");
    expect(screen.getByRole("button", { name: "注册来源" })).toHaveProperty(
      "disabled",
      true,
    );
    expect(
      screen.getByRole("button", { name: /手动同步全部已授权来源/ }),
    ).toHaveProperty("disabled", true);
    expect(screen.queryByLabelText("目标出域策略")).toBeNull();
  });
});

describe("授权前：真实回溯与开放结束边界", () => {
  it("展示运行时配置的真实回溯小时与开放上界，确认授权时再次声明", async () => {
    bridgeWith({
      sources: [source()],
      runtime: {
        ...RUNTIME,
        firstPull: { lookbackHours: 48, minHours: 1, maxHours: 168 },
      },
      commands: {
        source_set_enabled: () => source({ enabled: true, authorizedAt: 1763700100000 }),
      },
    });
    renderApp("/sources");
    await screen.findAllByText("OA 项目群", { exact: false });

    expect(await screen.findByText(/48（允许 1–168）/)).toBeTruthy();
    expect(
      screen.getByText(/开放（未闭合；每次同步推进到当前时刻）/),
    ).toBeTruthy();
    expect(screen.getByText(/不探测账号、不读取白名单外的企业数据/)).toBeTruthy();

    fireEvent.click(screen.getByRole("button", { name: "授权" }));
    const warning = screen.getByText(/首次拉取窗口为 \[/);
    expect(warning.textContent).toContain("开放)");
    expect(warning.textContent).toContain("不探测账号");
  });

  it("未读到首次拉取回溯时不允许授权", async () => {
    bridgeWith({
      sources: [source()],
      commands: {
        runtime_config_get: () => {
          throw {
            code: "IPC_ERROR",
            message: "runtime unavailable",
            retryable: false,
          };
        },
      },
    });
    renderApp("/sources");
    await screen.findAllByText("OA 项目群", { exact: false });

    expect(
      await screen.findByText(/运行时配置读取失败，无法显示真实回溯/),
    ).toBeTruthy();
    expect(screen.getByRole("button", { name: "授权" })).toHaveProperty(
      "disabled",
      true,
    );
  });
});
