import { fireEvent, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import type { ItemDetail } from "../lib/ipc";
import { installBridge, renderApp } from "../test-utils";

const BASE_DETAIL: ItemDetail = {
  item: {
    id: "wi_01",
    title: "确认 OA 业务复测结果",
    lifecycle: "active",
    attention: "me",
    nextAction: "向交办人回报复测结果",
    followupType: "none",
    followupAt: null,
    followupNote: null,
    threadId: "th_01",
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
  timeline: [],
  evidence: [],
  corrections: [],
  sourceFreshness: [],
};

function bridge(detail: ItemDetail) {
  return installBridge((command, args) => {
    const payload = (args?.args ?? {}) as Record<string, unknown>;
    if (command === "db_info") return { dbPath: "/tmp/app.db", schemaVersion: 1 };
    if (command === "sync_status") return [];
    if (command === "item_get") return detail;
    if (command === "item_correct") {
      return {
        ...detail,
        item: { ...detail.item, lifecycle: "ignored" },
        corrections: [
          {
            at: 1763700009000,
            actor: "human",
            reason: String(payload.reason),
            note: null,
            fields: ["belongsToMe"],
          },
        ],
      };
    }
    if (command === "item_close") return { ...detail, item: { ...detail.item, lifecycle: "closed" } };
    if (command === "item_set_followup") {
      return {
        ...detail,
        item: {
          ...detail.item,
          followupType: String(payload.type) as ItemDetail["item"]["followupType"],
          followupAt: (payload.at as number | null) ?? null,
        },
      };
    }
    if (command === "snapshot_create") {
      return { id: "snap_01", title: "详情页问题", createdAt: 1763700010000 };
    }
    throw { code: "IPC_ERROR", message: `command ${command} not found`, retryable: false };
  });
}

describe("事项详情 / 纠错 / 收尾 / 跟进", () => {
  it("把未收尾的模型提议与来源摘录分开展示", async () => {
    bridge({
      ...BASE_DETAIL,
      item: { ...BASE_DETAIL.item, resolutionEvidence: "artifact_accepted" },
      evidence: [{ id: "src1", kind: "im_message", label: "合成来源", occurredAt: 1,
        senderName: null, summary: "验收通过。", rawState: "available", linkType: "reference", state: "confirmed" }],
    });
    renderApp("/items/wi_01");
    expect(await screen.findByText("收尾提议")).toBeTruthy();
    expect(screen.getByText(/未确认收尾/)).toBeTruthy();
    expect(screen.getByText("来源摘录或摘要：验收通过。")).toBeTruthy();
  });

  it("「不是我的事」一次点击即调用 item_correct，并带可选备注", async () => {
    const invoke = bridge(BASE_DETAIL);
    renderApp("/items/wi_01");

    expect(
      await screen.findByRole("heading", { level: 1, name: "确认 OA 业务复测结果" }),
    ).toBeTruthy();
    fireEvent.change(screen.getByLabelText("补充一句（可选）"), {
      target: { value: "群里点名的是同事" },
    });
    fireEvent.click(screen.getByRole("button", { name: "不是我的事" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("item_correct", {
        args: {
          id: "wi_01",
          patch: { belongsToMe: false },
          reason: "not_mine",
          note: "群里点名的是同事",
        },
      }),
    );
    expect(await screen.findByText(/已纠正：不是我的事/)).toBeTruthy();
  });

  it("收尾只提供白名单证据，并把选择传给 item_close", async () => {
    const invoke = bridge(BASE_DETAIL);
    renderApp("/items/wi_01");

    fireEvent.click(await screen.findByRole("button", { name: "已结束" }));
    expect(screen.getByLabelText("对方明确表示已完成")).toBeTruthy();
    expect(screen.getByLabelText("交付物被接收方确认")).toBeTruthy();
    expect(screen.getByLabelText("我人工确认已完成")).toBeTruthy();
    expect(screen.queryByLabelText(/仅「好的」/)).toBeNull();

    fireEvent.click(screen.getByLabelText("我人工确认已完成"));
    fireEvent.click(screen.getByRole("button", { name: "确认收尾" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("item_close", {
        args: {
          id: "wi_01",
          evidenceType: "human_confirmed",
        },
      }),
    );
    expect(await screen.findByText("已收尾，并写入收尾证据事件。")).toBeTruthy();
  });

  it("收尾被后端拒绝时显示证据不足，不显示成功", async () => {
    installBridge((command) => {
      if (command === "db_info") return { dbPath: "/tmp/app.db", schemaVersion: 1 };
      if (command === "sync_status") return [];
      if (command === "item_get") return BASE_DETAIL;
      if (command === "item_close") {
        throw {
          code: "VALIDATION_REJECTED",
          message: "「好的」不构成收尾证据，未改变事项状态。",
          retryable: false,
        };
      }
      throw "unexpected";
    });
    renderApp("/items/wi_01");

    fireEvent.click(await screen.findByRole("button", { name: "已结束" }));
    fireEvent.click(screen.getByLabelText("对方明确表示已完成"));
    fireEvent.click(screen.getByRole("button", { name: "确认收尾" }));

    expect(await screen.findByText(/操作失败（VALIDATION_REJECTED）/)).toBeTruthy();
    expect(screen.queryByText("已收尾，并写入收尾证据事件。")).toBeNull();
  });

  it("人工跟进日期转成 UTC 毫秒；事件类型缺说明时不发请求", async () => {
    const invoke = bridge(BASE_DETAIL);
    renderApp("/items/wi_01");
    await screen.findByRole("heading", { level: 1, name: "确认 OA 业务复测结果" });

    const type = screen.getByLabelText("跟进类型") as HTMLSelectElement;
    fireEvent.change(type, { target: { value: "manual_time" } });
    fireEvent.change(screen.getByLabelText("日期时间"), {
      target: { value: "2026-11-25T09:30" },
    });
    fireEvent.click(screen.getByRole("button", { name: "保存跟进" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith("item_set_followup", {
        args: {
          id: "wi_01",
          type: "manual_time",
          at: new Date("2026-11-25T09:30").getTime(),
          note: null,
        },
      }),
    );

    fireEvent.change(type, { target: { value: "event" } });
    fireEvent.click(screen.getByRole("button", { name: "保存跟进" }));
    expect(await screen.findByText("请填写要等待的事件。")).toBeTruthy();
    const followupCalls = invoke.mock.calls.filter(
      ([command]) => command === "item_set_followup",
    );
    expect(followupCalls).toHaveLength(1);
  });

  it("从详情记录问题，带上当前事项引用", async () => {
    const invoke = bridge(BASE_DETAIL);
    renderApp("/items/wi_01");

    fireEvent.click(await screen.findByRole("button", { name: "记录问题" }));
    expect(screen.getByText("/items/wi_01")).toBeTruthy();
    fireEvent.change(screen.getByLabelText("哪里错（必填）"), {
      target: { value: "详情页来源标签显示不全" },
    });
    fireEvent.click(screen.getByRole("button", { name: "写入迭代池" }));

    await waitFor(() =>
      expect(invoke).toHaveBeenCalledWith(
        "snapshot_create",
        expect.objectContaining({
          route: "/items/wi_01",
          targetType: "work_item",
          targetId: "wi_01",
          description: "详情页来源标签显示不全",
        }),
      ),
    );
    expect(await screen.findByText(/已记录到迭代池/)).toBeTruthy();
  });

  it("历史建议可归入项目，保留草稿并以最新版本重试", async () => {
    const delivery: NonNullable<ItemDetail["delivery"]> = {
      version: "1", present: false, formal: false, confirmation: "suggested", state: "triage",
      project: null, work: "核对第一版图纸", conclusion: "待核对", related: [],
      evidenceDetails: [
        { time: "2026-11-18 12:00", conversation: "工艺群", kind: "消息", excerpt: "发来修改版" },
        { time: "2026-11-17 09:00", conversation: "工艺群", kind: "消息", excerpt: "发来第一版" },
      ],
    };
    const invoke = installBridge((command, args) => {
      if (command === "db_info") return { dbPath: "/tmp/app.db", schemaVersion: 1 };
      if (command === "sync_status") return [];
      if (command === "item_get") return { ...BASE_DETAIL, delivery: { ...delivery } };
      if (command === "ownership_set_project") {
        if (args?.version === "1") {
          delivery.version = "2";
          throw new Error("事项版本已更新");
        }
        delivery.project = String(args?.project);
        delivery.related = [{ id: "other/id", title: "交付第二版" }];
      }
      return null;
    });
    renderApp("/items/wi_01");
    expect(await screen.findByText("历史事项")).toBeTruthy();
    expect(screen.getByText("系统建议")).toBeTruthy();
    expect(screen.getByText("待核对")).toBeTruthy();
    expect(screen.getByText("发来第一版").compareDocumentPosition(screen.getByText("发来修改版")) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();
    for (const name of ["我来处理", "编辑结论", "记录问题", "保存跟进"]) {
      expect(screen.queryByRole("button", { name })).toBeNull();
    }
    fireEvent.click(screen.getByRole("button", { name: "编辑项目" }));
    fireEvent.change(screen.getByLabelText(/项目名称/), { target: { value: " 工艺核对 " } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));
    expect(await screen.findByText(/保存失败，草稿已保留/)).toBeTruthy();
    expect((screen.getByLabelText(/项目名称/) as HTMLInputElement).value).toBe(" 工艺核对 ");
    fireEvent.click(screen.getByRole("button", { name: "保存" }));
    expect((await screen.findByRole("link", { name: "交付第二版" })).getAttribute("href")).toBe("/items/other%2Fid");
    expect(invoke).toHaveBeenCalledWith("ownership_set_project", { id: "wi_01", version: "2", project: "工艺核对" });
    expect(screen.getByText("系统建议")).toBeTruthy();
  });

  it.each([
    { project: "发布", threadId: "old-thread" },
    { project: null, threadId: "old-thread" },
    { project: "报告建议", threadId: null },
    { project: null, threadId: null },
  ])("保留 $project / $threadId 时不写入，明确解除时使用原版本", async ({ project, threadId }) => {
    const detail: ItemDetail = {
      ...BASE_DETAIL, item: { ...BASE_DETAIL.item, threadId },
      delivery: { version: "7", formal: true, present: true, confirmation: "suggested", state: "mine",
        project, scopeId: null, work: "复测交付", conclusion: null, related: [], evidenceDetails: [] },
    };
    const invoke = installBridge(command => {
      if (command === "item_get") return detail;
      if (command === "db_info") return { dbPath: "/tmp/app.db", schemaVersion: 6 };
      if (command === "sync_status" || command === "context_scopes") return [];
      return null;
    });
    renderApp("/items/wi_01");
    const openPicker = async () => {
      fireEvent.click(await screen.findByRole("button", { name: "关联已有项目" }));
      const save = screen.getByRole("button", { name: "保存关联" }) as HTMLButtonElement;
      await waitFor(() => expect(save.disabled).toBe(false));
      return save;
    };
    const save = await openPicker();
    expect((screen.getByLabelText("选择已有项目") as HTMLSelectElement).value).toBe("keep");
    fireEvent.click(save);
    fireEvent.click(await screen.findByRole("button", { name: "编辑项目" }));
    fireEvent.click(screen.getByRole("button", { name: "保存" }));
    const unlink = await openPicker();
    expect(invoke.mock.calls.filter(([cmd]) => cmd === "context_item_scope_set" || cmd === "ownership_set_project")).toHaveLength(0);
    fireEvent.change(screen.getByLabelText("选择已有项目"), { target: { value: "unlink" } });
    fireEvent.click(unlink);
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("context_item_scope_set", { id: "wi_01", version: "7", scopeId: null }));
  });

  it("可按稳定 ID 选择同名项目并解除关联", async () => {
    const delivery: NonNullable<ItemDetail["delivery"]> = {
      version: "1", present: true, formal: false, confirmation: "suggested", state: "mine",
      project: "工艺", scopeId: null, work: "核对图纸", conclusion: null, related: [], evidenceDetails: [],
    };
    const invoke = installBridge((command, args) => {
      if (command === "db_info") return { dbPath: "/tmp/app.db", schemaVersion: 6 };
      if (command === "sync_status") return [];
      if (command === "item_get") return { ...BASE_DETAIL, delivery: { ...delivery } };
      if (command === "context_scopes") return ["a", "b", "c"].map(id => ({
        id, title: "工艺", kind: id === "c" ? "domain" : "project", goal: "", background: "", version: 1,
      }));
      if (command === "context_item_scope_set") {
        if (args?.version === "1") {
          delivery.scopeId = "a";
          delivery.version = "2";
          throw new Error("事项版本已更新");
        }
        delivery.scopeId = args?.scopeId as string | null;
        delivery.project = delivery.scopeId ? "工艺" : null;
        delivery.version = String(Number(delivery.version) + 1);
      }
      return null;
    });
    renderApp("/items/wi_01");
    fireEvent.click(await screen.findByRole("button", { name: "关联已有项目" }));
    await screen.findByRole("option", { name: "工艺 · b" });
    expect(screen.queryByRole("option", { name: "工艺 · c" })).toBeNull();
    fireEvent.change(screen.getByRole("combobox", { name: "选择已有项目" }), { target: { value: "scope:b" } });
    fireEvent.click(screen.getByRole("button", { name: "保存关联" }));
    await screen.findByText("事项已更新，请取消后重新选择项目。");
    expect((screen.getByRole("button", { name: "保存关联" }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.click(screen.getByRole("button", { name: "取消" }));
    fireEvent.click(screen.getByRole("button", { name: "关联已有项目" }));
    const picker = screen.getByRole("combobox", { name: "选择已有项目" }) as HTMLSelectElement;
    expect(picker.value).toBe("scope:a");
    fireEvent.change(picker, { target: { value: "scope:b" } });
    fireEvent.click(screen.getByRole("button", { name: "保存关联" }));
    await screen.findByText("已关联项目");
    expect(invoke).toHaveBeenCalledWith("context_item_scope_set", { id: "wi_01", version: "2", scopeId: "b" });
    fireEvent.click(await screen.findByRole("button", { name: "关联已有项目" }));
    expect((screen.getByLabelText("选择已有项目") as HTMLSelectElement).value).toBe("scope:b");
    fireEvent.click(screen.getByRole("button", { name: "保存关联" }));
    fireEvent.click(await screen.findByRole("button", { name: "编辑项目" }));
    fireEvent.click(screen.getByRole("button", { name: "保存" }));
    expect(invoke.mock.calls.filter(([cmd]) => cmd === "context_item_scope_set")).toHaveLength(2);
    expect(invoke.mock.calls.filter(([cmd]) => cmd === "ownership_set_project")).toHaveLength(0);
    fireEvent.click(await screen.findByRole("button", { name: "关联已有项目" }));
    fireEvent.change(screen.getByRole("combobox", { name: "选择已有项目" }), { target: { value: "unlink" } });
    fireEvent.click(screen.getByRole("button", { name: "保存关联" }));
    await screen.findByText("未归入项目");
    expect(invoke).toHaveBeenCalledWith("context_item_scope_set", { id: "wi_01", version: "3", scopeId: null });
  });

  it("在建议详情保存结论并确认本人处理，正式操作随事项状态更新", async () => {
    const delivery: NonNullable<ItemDetail["delivery"]> = {
      version: "1", present: true, formal: false, confirmation: "suggested", state: "mine",
      project: null, work: "复测结果", conclusion: null, related: [], evidenceDetails: [],
    };
    const invoke = installBridge((command, args) => {
      if (command === "db_info") return { dbPath: "/tmp/app.db", schemaVersion: 1 };
      if (command === "sync_status") return [];
      if (command === "item_get") return { ...BASE_DETAIL, delivery: { ...delivery } };
      if (command === "ownership_set_conclusion") {
        delivery.conclusion = String(args?.note);
        delivery.confirmation = "confirmed";
        delivery.version = "2";
      }
      if (command === "ownership_move") delivery.formal = true;
      return null;
    });
    renderApp("/items/wi_01");
    fireEvent.click(await screen.findByRole("button", { name: "编辑结论" }));
    fireEvent.change(screen.getByLabelText("我的结论"), { target: { value: "复测通过" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));
    expect(await screen.findByText("复测通过")).toBeTruthy();
    expect(invoke).toHaveBeenCalledWith("ownership_set_conclusion", { id: "wi_01", version: "1", note: "复测通过", resolved: false });
    fireEvent.click(await screen.findByRole("button", { name: "我来处理" }));
    expect((screen.getByLabelText("接下来你要做什么？") as HTMLInputElement).value).toBe(BASE_DETAIL.item.nextAction);
    fireEvent.click(screen.getByRole("button", { name: "保存" }));
    expect(await screen.findByRole("button", { name: "记录问题" })).toBeTruthy();
    expect(invoke).toHaveBeenCalledWith("ownership_move", { id: "wi_01", version: "2", target: "mine", action: BASE_DETAIL.item.nextAction });
  });

  it.each([["done", "重新打开"], ["ignored", "恢复跟进"]] as const)("归档的正式事项 %s 可凭新行动恢复", async (state, label) => {
    const delivery: NonNullable<ItemDetail["delivery"]> = {
      version: "9", present: false, formal: true, confirmation: "confirmed", state,
      project: null, work: "复测交付", conclusion: null, related: [], evidenceDetails: [],
    };
    const invoke = installBridge((command) => {
      if (command === "db_info") return { dbPath: "/tmp/app.db", schemaVersion: 1 };
      if (command === "sync_status") return [];
      if (command === "item_get") return { ...BASE_DETAIL, delivery: { ...delivery } };
      if (command === "ownership_move") delivery.state = "mine";
      return null;
    });
    renderApp("/items/wi_01");
    fireEvent.click(await screen.findByRole("button", { name: label }));
    expect((screen.getByRole("button", { name: "保存" }) as HTMLButtonElement).disabled).toBe(true);
    fireEvent.change(screen.getByLabelText("接下来你要做什么？"), { target: { value: "按新要求重新复测" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));
    expect(await screen.findByRole("button", { name: "等对方处理" })).toBeTruthy();
    expect(invoke).toHaveBeenCalledWith("ownership_move", { id: "wi_01", version: "9", target: "mine", action: "按新要求重新复测" });
  });
});
