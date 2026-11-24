import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { StatusBar } from "./StatusBar";
import { RealtimeBridge } from "./RealtimeBridge";
import {
  emitTauriEvent,
  hasTauriListener,
  installBridge,
  removeBridge,
} from "../test-utils";

function renderStatus(withEvents = false) {
  const client = new QueryClient({
    defaultOptions: { queries: { retry: false } },
  });
  return render(
    <QueryClientProvider client={client}>
      {withEvents ? <RealtimeBridge /> : null}
      <StatusBar />
    </QueryClientProvider>,
  );
}

describe("StatusBar", () => {
  it("浏览器预览明确说未连接，而不是「离线」", () => {
    removeBridge();
    renderStatus();
    const status = screen.getByRole("status");
    expect(status.getAttribute("aria-live")).toBe("polite");
    expect(status.textContent).toContain("未连接桌面端");
    expect(status.textContent).not.toContain("离线");
    expect(status.textContent).not.toContain("已读");
    expect(status.textContent).not.toContain("回执");
  });

  it("没有数据源时显示尚未同步，无最后成功时间", async () => {
    installBridge((command) => {
      if (command === "db_info") return { dbPath: "/tmp/a.db", schemaVersion: 1 };
      if (command === "sync_status") return [];
      throw "unexpected";
    });
    renderStatus();
    await waitFor(() =>
      expect(screen.getByRole("status").textContent).toContain("尚未同步"),
    );
    expect(screen.getByRole("status").textContent).toContain("未配置数据源");
  });

  it("有最后成功时间时显示真实时间，不声称已读", async () => {
    installBridge((command) => {
      if (command === "db_info") return { dbPath: "/tmp/a.db", schemaVersion: 1 };
      if (command === "sync_status") {
        return [
          {
            sourceId: "src_01",
            displayName: "OA 项目群",
            state: "idle",
            lastAttemptAt: 1763700000000,
            lastSuccessAt: 1763700000000,
            consecutiveFailures: 0,
            lastErrorCode: null,
            lastErrorMsg: null,
            windowStart: null,
            windowEnd: null,
            cursorAt: 1763700000000,
          },
        ];
      }
      throw "unexpected";
    });
    renderStatus();
    await waitFor(() =>
      expect(screen.getByRole("status").textContent).toContain("最后成功"),
    );
    const text = screen.getByRole("status").textContent ?? "";
    expect(text).not.toContain("已读");
    expect(text).not.toContain("回执");
  });

  it("同步状态读取失败时给出错误码，不伪造空状态", async () => {
    installBridge((command) => {
      if (command === "db_info") return { dbPath: "/tmp/a.db", schemaVersion: 1 };
      if (command === "sync_status") {
        throw { code: "IPC_ERROR", message: "sync_status 未实现", retryable: false };
      }
      throw "unexpected";
    });
    renderStatus();
    await waitFor(() =>
      expect(screen.getByRole("status").textContent).toContain(
        "同步状态不可读 · IPC_ERROR",
      ),
    );
  });
});

describe("StatusBar 真实事件进度", () => {
  it("手动同步后只显示收到的 sync://progress 结果", async () => {
    const invoke = installBridge((command) => {
      if (command === "db_info") return { dbPath: "/tmp/a.db", schemaVersion: 1 };
      if (command === "sync_status") {
        return [
          {
            sourceId: "src_1",
            displayName: "OA 项目群",
            state: "idle",
            lastAttemptAt: 1763700000000,
            lastSuccessAt: 1763700000000,
            consecutiveFailures: 0,
            lastErrorCode: null,
            lastErrorMsg: null,
            windowStart: null,
            windowEnd: null,
            cursorAt: 1763700000000,
          },
        ];
      }
      if (command === "source_list") {
        return [
          {
            id: "src_1",
            kind: "dws_im",
            displayName: "OA 项目群",
            scopeSummary: "1 个会话",
            enabled: true,
            authorizedAt: 1763700000000,
            revokedAt: null,
            createdAt: 1763700000000,
          },
        ];
      }
      if (command === "sync_now") return { jobId: "job_s", sources: ["src_1"] };
      throw "unexpected";
    });
    renderStatus(true);

    const button = screen.getByRole("button", { name: "立即同步" });
    await waitFor(() => expect(button).toHaveProperty("disabled", false));
    await waitFor(() => expect(hasTauriListener("sync://progress")).toBe(true));

    fireEvent.click(button);
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("sync_now", {}));
    await waitFor(() =>
      expect(screen.getByRole("status").textContent).toContain("同步中 · 已收到 0/1"),
    );

    emitTauriEvent("sync://progress", {
      jobId: "job_s",
      phase: "source",
      sourceId: "src_1",
      outcome: {
        sourceId: "src_1",
        runId: "run_1",
        status: "ok",
        pages: 1,
        fetched: 1,
        outOfScope: 0,
        inserted: 1,
        unchanged: 0,
        updated: 0,
        cursorAdvanced: true,
        error: null,
      },
      error: null,
    });
    await waitFor(() =>
      expect(screen.getByRole("status").textContent).toContain("同步中 · 已收到 1/1"),
    );

    emitTauriEvent("sync://progress", {
      jobId: "job_s",
      phase: "done",
      sourceId: null,
      outcome: null,
      error: null,
    });
    await waitFor(() =>
      expect(screen.getByRole("status").textContent).toContain(
        "上次手动同步 1/1 个来源完成",
      ),
    );
    expect(screen.getByRole("status").textContent).not.toContain("离线");
  });
});
