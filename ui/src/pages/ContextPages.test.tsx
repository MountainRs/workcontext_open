import { fireEvent, screen, waitFor } from "@testing-library/react";
import { describe, expect, it } from "vitest";
import { installBridge, renderApp } from "../test-utils";
import type { Scope } from "../lib/context";
import { formatDateTime } from "../lib/format";

function contextBridge(commands: Record<string, (payload: Record<string, unknown>) => unknown>) {
  return installBridge((command, payload) => {
    if (command === "db_info") return { dbPath: "test.sqlite3", schemaVersion: 1 };
    if (command === "sync_status") return [];
    if (command === "context_local_available" && !commands[command]) return false;
    return commands[command]?.(payload ?? {});
  });
}

describe("桌面工作上下文", () => {
  it("浏览器预览禁用补录并区分未连接与没有记录", () => {
    renderApp("/records");
    expect(screen.getByText("未连接 Tauri 桌面端")).toBeTruthy();
    expect((screen.getByRole("button", { name: "新增记录" }) as HTMLButtonElement).disabled).toBe(true);
    expect(screen.queryByText("暂无工作记录。")).toBeNull();
  });

  it("保存未归类经验并进入原文和资料详情", async () => {
    const content = "排查后重新生成本地配置，复测成功。";
    const detail = { record: { id: "r-1", title: "恢复配置", content, scopes: [], createdAt: 1000, occurredAt: null, kind: "experience" }, original: content, materials: ["复测.txt"] };
    const invoke = contextBridge({ context_scopes: () => [], context_records: () => ({ items: [], total: 0 }), context_record_add: () => detail, context_record_get: () => detail });
    renderApp("/records");
    await screen.findByText("暂无工作记录。");
    fireEvent.click(screen.getByRole("button", { name: "新增记录" }));
    fireEvent.change(screen.getByLabelText("标题"), { target: { value: "恢复配置" } });
    fireEvent.change(screen.getByLabelText("记录原文"), { target: { value: content } });
    fireEvent.change(screen.getByLabelText("记录类型"), { target: { value: "experience" } });
    fireEvent.change(screen.getByLabelText(/资料引用/), { target: { value: "复测.txt" } });
    fireEvent.click(screen.getByRole("button", { name: "保存记录" }));
    expect(await screen.findByRole("heading", { name: "恢复配置" })).toBeTruthy();
    expect(screen.getByText(content)).toBeTruthy();
    expect(screen.getByText("复测.txt")).toBeTruthy();
    expect(invoke).toHaveBeenCalledWith("context_record_add", { args: { title: "恢复配置", content, kind: "experience", scopeIds: [], materials: ["复测.txt"] } });
  });

  it("列表标明时间来源，详情可核对发生时间和录入时间", async () => {
    const createdAt = Date.UTC(2026, 10, 21);
    const occurredAt = Date.UTC(2026, 10, 1);
    const record = { id: "history", title: "补录进展", content: "现场记录", scopes: [], kind: "progress", createdAt, occurredAt };
    const undated = { ...record, id: "undated", title: "未填发生时间", occurredAt: null };
    contextBridge({ context_scopes: () => [], context_records: () => ({ items: [undated, record], total: 2 }),
      context_record_get: () => ({ record, original: record.content, materials: [] }) });
    renderApp("/records");
    const link = await screen.findByRole("link", { name: "补录进展" });
    expect(link.closest("li")?.textContent).toContain(`发生时间 ${formatDateTime(occurredAt)}`);
    expect(screen.getByRole("link", { name: "未填发生时间" }).closest("li")?.textContent).toContain(`录入时间 ${formatDateTime(createdAt)}`);
    fireEvent.click(link);
    await screen.findByRole("heading", { name: "补录进展" });
    expect(screen.getByText(`发生时间 ${formatDateTime(occurredAt)}`)).toBeTruthy();
    expect(screen.getByText(`录入时间 ${formatDateTime(createdAt)}`, { exact: false })).toBeTruthy();
  });

  it("绑定后可切换本机工作区，打开原文并返回，两个工作区分别缓存", async () => {
    const record = (local: boolean) => ({ id: "shared", title: local ? "本机经验" : "账号记录", content: local ? "本机输入" : "账号输入", scopes: [], createdAt: 1, occurredAt: null, kind: "event" });
    const invoke = contextBridge({
      context_local_available: () => true,
      context_scopes: () => [],
      context_records: p => ({ items: [record(p.localOnly === true)], total: 1 }),
      context_record_get: p => { const row = record(p.localOnly === true); return { record: row, original: row.content, materials: [] }; },
    });
    const { client } = renderApp("/records");
    fireEvent.click(await screen.findByRole("link", { name: "账号记录" }));
    expect(await screen.findByText("账号输入")).toBeTruthy();
    fireEvent.click(screen.getByRole("link", { name: "返回工作记录" }));
    fireEvent.click(screen.getByRole("button", { name: "新增记录" }));
    fireEvent.change(screen.getByLabelText("标题"), { target: { value: "账号草稿" } });
    fireEvent.change(await screen.findByLabelText("工作区"), { target: { value: "local" } });
    const link = await screen.findByRole("link", { name: "本机经验" });
    expect(screen.queryByLabelText("标题")).toBeNull();
    expect(screen.queryByRole("link", { name: "账号记录" })).toBeNull();
    expect(link.getAttribute("href")).toBe("/records/shared?local=1");
    fireEvent.click(link);
    expect(await screen.findByText("本机输入")).toBeTruthy();
    expect(invoke).toHaveBeenCalledWith("context_record_get", { id: "shared", localOnly: true });
    expect(invoke).toHaveBeenCalledWith("context_records", { args: expect.anything(), localOnly: true });
    expect(invoke).toHaveBeenCalledWith("context_scopes", { localOnly: true });
    expect(client.getQueriesData({ queryKey: ["context_record_get", "shared"] }).map(([, data]) => data)).toEqual(expect.arrayContaining([
      expect.objectContaining({ original: "本机输入" }), expect.objectContaining({ original: "账号输入" }),
    ]));
    fireEvent.click(screen.getByRole("link", { name: "返回工作记录" }));
    expect((await screen.findByLabelText("工作区") as HTMLSelectElement).value).toBe("local");
  });

  it("按稳定 ID 查看工作记录与交付，改名带上初始版本", async () => {
    let project: Scope = { id: "p-7", kind: "project", title: "第一版", goal: "交付复测", background: "验收前准备", version: 3 };
    const invoke = contextBridge({
      context_scopes: () => [project],
      context_records: () => ({ items: [{ id: "r", title: "验收决定", content: "采用分批验收", kind: "decision", occurredAt: null, createdAt: 1, scopes: [] }], total: 1 }),
      context_scope_items: () => [{ id: "delivery-2", title: "第二批交付", lifecycle: "active", attention: "me", nextAction: "预约复测", conclusion: "首批验收通过" }],
      context_scope_save: () => { project = { ...project, title: "新名称", version: 4 }; return project; },
    });
    renderApp("/projects/p-7");
    expect((await screen.findByRole("link", { name: "第二批交付" })).getAttribute("href")).toBe("/items/delivery-2");
    expect(await screen.findByText("采用分批验收")).toBeTruthy();
    expect(screen.getByText(/预约复测/)).toBeTruthy();
    expect(screen.getByText(/首批验收通过/)).toBeTruthy();
    expect(invoke).toHaveBeenCalledWith("context_records", { args: expect.objectContaining({ scopeId: "p-7" }) });
    fireEvent.click(screen.getByRole("button", { name: "编辑项目" }));
    fireEvent.change(screen.getByLabelText("名称"), { target: { value: "新名称" } });
    fireEvent.click(screen.getByRole("button", { name: "保存" }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("context_scope_save", { args: { id: "p-7", kind: "project", version: 3, title: "新名称", goal: "交付复测", background: "验收前准备" } }));
    expect(await screen.findByRole("heading", { name: "新名称" })).toBeTruthy();
  });

  it("固定修订正文、归因与空范围，并保留来源原文", async () => {
    const original = "重启节点后仍失败";
    const record = { id: "note", title: "模拟排查", content: original, scopes: [], kind: "experience", createdAt: 1, occurredAt: null,
      sourceRef: "input-note", version: 2, origin: "agent", attribution: "reported", derived: [] };
    const correction = { content: "恢复时间仍待复核", attribution: "inference", scopeIds: [] };
    const detail = { record, original, materials: [], correction: { content: null, attribution: null, scopeIds: null } };
    const invoke = contextBridge({ context_record_get: () => detail, context_scopes: () => [],
      context_record_correct: () => ({ ...detail, correction, record: { ...record, content: correction.content, attribution: "inference", version: 3 } }) });
    renderApp("/records/note");
    fireEvent.click(await screen.findByRole("button", { name: "纠正记录" }));
    fireEvent.click(screen.getByLabelText("固定正文与归因"));
    fireEvent.change(screen.getByLabelText("修订正文"), { target: { value: correction.content } });
    fireEvent.change(screen.getByLabelText("正文归因"), { target: { value: "inference" } });
    fireEvent.click(screen.getByLabelText(/固定范围/));
    await screen.findByRole("group", { name: "项目或领域" });
    fireEvent.click(screen.getByRole("button", { name: "保存修订" }));
    await screen.findByRole("heading", { name: "当前正文" });
    expect(screen.getByText(original)).toBeTruthy();
    expect(screen.getByText(correction.content)).toBeTruthy();
    expect(invoke).toHaveBeenCalledWith("context_record_correct", { args: { id: "note", version: 2, ...correction } });
  });

  it("区分获准来源的摘录、推断和问题，再次读取失效时清除页面摘录", async () => {
    const sourceRef = "archive1-synthetic";
    const scope = { id: "scope-demo", kind: "project", title: "模拟交付", version: 1, goal: "", background: "" };
    const record = { id: "archived", title: "模拟报告", content: "恢复时间待核实", createdAt: 1, occurredAt: null, scopes: [scope],
      kind: "progress", sourceRef, origin: "archive", attribution: "inference", producer: null, scopeBasis: "organizer", version: 3,
      derived: [{ title: "原文整理", sourceRef, version: 6, scopeId: scope.id,
        classification: { facts: ["模拟服务恢复"], inferences: ["可能与重试有关"], openQuestions: ["是否能复现？"] } }] };
    let allowed = true;
    const invoke = contextBridge({ context_scopes: () => [scope], context_records: () => ({ items: allowed ? [record] : [], total: allowed ? 1 : 0 }),
      context_record_get: () => {
        if (!allowed) return Promise.reject("记录不可访问");
        return { record, original: "", materials: [], reportedResult: null, inference: null, openQuestions: [],
          correction: { content: record.content, attribution: "inference", scopeIds: null } };
      } });
    const { client } = renderApp("/records");
    await screen.findByRole("option", { name: /模拟交付/ });
    fireEvent.change(screen.getByLabelText("项目或领域"), { target: { value: scope.id } });
    fireEvent.change(screen.getByRole("searchbox"), { target: { value: "复现" } });
    fireEvent.click(await screen.findByRole("link", { name: record.title }));
    expect(invoke).toHaveBeenCalledWith("context_records", { args: { scopeId: scope.id, query: "复现", offset: 0 } });
    expect(await screen.findByText("模拟服务恢复")).toBeTruthy();
    expect(screen.getByText("可能与重试有关")).toBeTruthy();
    expect(screen.getByText("是否能复现？")).toBeTruthy();
    expect(screen.getByText("建议范围")).toBeTruthy();
    expect(screen.queryByRole("heading", { name: "记录原文" })).toBeNull();
    allowed = false;
    await client.invalidateQueries({ queryKey: ["context_record_get"] });
    await screen.findByText(/^读取记录失败（/);
    expect(screen.queryByText(record.title)).toBeNull();
    expect(screen.queryByText(record.content)).toBeNull();
    expect(screen.queryByText("模拟服务恢复")).toBeNull();
    fireEvent.click(screen.getByRole("link", { name: "返回工作记录" }));
    expect(await screen.findByText("没有匹配的工作记录。")).toBeTruthy();
    expect((screen.getByRole("searchbox") as HTMLInputElement).value).toBe("复现");
    expect((screen.getByLabelText("项目或领域") as HTMLSelectElement).value).toBe(scope.id);
  });

  it("请求来源时重新核对权限并显示脱敏消息与附件", async () => {
    const record = { id: "archive", title: "归档进展", content: "进展摘录", origin: "archive", sourceRef: "archive1-example", createdAt: 100, scopes: [] };
    const invoke = contextBridge({ context_record_get: payload => ({ record, original: "", materials: [],
      ...(payload.includeSource === true && { archiveSource: { body: "合成归档消息", occurredAt: 100, attachments: ["合成附件摘录"] } }) }) });
    renderApp("/records/archive");
    const open = await screen.findByRole("button", { name: "查看来源详情" });
    expect(screen.queryByText("合成归档消息")).toBeNull();
    fireEvent.click(open);
    expect(await screen.findByText("合成归档消息")).toBeTruthy();
    expect(screen.getByText("合成附件摘录")).toBeTruthy();
    expect(invoke).toHaveBeenCalledWith("context_record_get", { id: record.id, includeSource: true });
  });

  it("按目标区分同名项目，按项目查问题并返回原搜索", async () => {
    const scopes = ["北区", "南区"].map((goal, i) => ({ id: `twin-${i}`, title: "验收", goal, background: "", version: 1, kind: "project" }));
    const invoke = contextBridge({ context_scopes: () => scopes, context_scope_items: () => [], context_records: () => ({ items: [], total: 0 }) });
    renderApp("/projects");
    fireEvent.change(await screen.findByLabelText("搜索项目或领域"), { target: { value: "验收" } });
    const links = await screen.findAllByRole("link", { name: "验收" });
    expect(links[1].closest("li")?.textContent).toContain("南区");
    fireEvent.click(links[1]);
    fireEvent.click(await screen.findByRole("button", { name: "未决问题" }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("context_records", { args: expect.objectContaining({ scopeId: "twin-1", section: "questions" }) }));
    fireEvent.click(screen.getByRole("link", { name: "返回重点项目" }));
    expect((await screen.findByLabelText("搜索项目或领域") as HTMLInputElement).value).toBe("验收");
  });
});
