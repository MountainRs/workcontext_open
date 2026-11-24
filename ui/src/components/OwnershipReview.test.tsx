import { act, fireEvent, screen, waitFor, within } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { OWNERSHIP_AUTO_POLL_MS } from "./OwnershipReview";
import { installBridge, renderApp } from "../test-utils";

const initial = {
  id: "wi_review", version: "v1", state: "mine", work: "核实样图的工序",
  action: "核对三张样图与工艺清单。", when: "",
  sourceSummary: { time: "周一", people: "工艺负责人", place: "评审群", event: "项目组请你核实三张样图。" },
};
const localItem = {
  id: initial.id, title: initial.work, nextAction: initial.action, attention: "me", whyHere: "",
  lastEvidenceAt: null, sourceRef: { id: "sr_review", kind: "manual_note", label: "手工记录" },
};
const CREDENTIAL_ERROR = "请设置 WORKCONTEXT_REVIEW_API_KEY 并重启桌面端";

type Card = typeof initial;
type View = {
  report: { cutoff: string; cards: Card[] | null } | null;
  running: boolean;
  progress: string | null;
  error: string | null;
  canRun: boolean;
  autoEnabled: boolean;
  profile: string | null;
};

function installReview(
  getCard: () => Card,
  handle: (cmd: string, args?: Record<string, unknown>) => unknown,
  canRun = false,
  view: Partial<View> | (() => Partial<View>) = {},
) {
  return installBridge((cmd, args) => {
    if (cmd === "db_info") return { dbPath: "/tmp/test.db", schemaVersion: 1 };
    if (cmd === "sync_status") return [];
    if (cmd === "ownership_view") return {
      report: { cutoff: "", cards: [{ ...getCard() }] },
      running: false, progress: null, error: null, canRun, autoEnabled: false, profile: "org:user",
      ...(typeof view === "function" ? view() : view),
    };
    if (cmd === "today_list") return {
      groups: { mine: [localItem, { ...localItem, id: "wi_other", title: "另一件本地事项", nextAction: "发出评审邀请" }], waiting: [], confirm: [] },
      freshness: { state: "fresh", lastSuccessAt: 1763700000000, windowStart: null, windowEnd: null },
    };
    return handle(cmd, args);
  });
}

const button = (name: string | RegExp) => screen.getByRole("button", { name });
/** 假时钟下推进时间并等查询、变更全部落定；不用 findBy/waitFor，它们不认 vitest 的假时钟。 */
const elapse = async (ms = 50) => {
  await act(async () => {
    await vi.advanceTimersByTimeAsync(ms);
  });
};
const precedes = (first: Element, second: Element) =>
  expect(first.compareDocumentPosition(second) & Node.DOCUMENT_POSITION_FOLLOWING).toBeTruthy();

describe("事项分组浏览", () => {
  it("把待核实的问题单列，旧报告的普通动作仍可直接处理", async () => {
    const verify = { ...initial, needsVerification: true };
    const direct = { ...initial, id: "wi_send", work: "交付样图", action: "把确认过的样图发给项目组。" };
    installReview(() => initial, cmd => { throw new Error(cmd); }, false, {
      report: { cutoff: "", cards: [verify, direct] },
    });
    renderApp();
    const directTitle = await screen.findByRole("heading", { name: direct.work });
    const checkSection = screen.getByRole("heading", { name: /先核实/ });
    precedes(screen.getByRole("heading", { name: /直接处理/ }), directTitle);
    precedes(directTitle, checkSection);
    precedes(checkSection, screen.getByRole("heading", { name: verify.work }));
    expect(screen.getByRole("radio", { name: "我来办 2" }).getAttribute("aria-checked")).toBe("true");
  });

  it("方向键按分组顺序环绕，Home 和 End 跳到两端，Tab 只停在当前分组", async () => {
    installReview(() => ({ ...initial, state: "waiting" }), cmd => { throw new Error(cmd); });
    renderApp();
    const filters = within(await screen.findByRole("radiogroup", { name: "按事项归属浏览" }));
    const radios = filters.getAllByRole("radio");
    expect(filters.getByRole("radio", { checked: true })).toBe(radios[1]);
    radios[1].focus();
    for (const [key, next] of [
      ["ArrowRight", 2], ["ArrowDown", 0], ["ArrowRight", 1], ["ArrowRight", 2],
      ["ArrowLeft", 1], ["ArrowUp", 0], ["Home", 0], ["End", 2],
    ] as const) {
      expect(fireEvent.keyDown(document.activeElement!, { key })).toBe(false);
      expect(document.activeElement).toBe(radios[next]);
      expect(filters.getByRole("radio", { checked: true })).toBe(radios[next]);
      expect(radios.filter(radio => radio.tabIndex === 0)).toEqual([radios[next]]);
    }
    expect(radios).toHaveLength(3);
    expect(screen.getByText("目前没有需要你认领的事项。")).toBeTruthy();
    expect(fireEvent.keyDown(radios[2], { key: "Tab" })).toBe(true);
    expect(fireEvent.keyDown(radios[2], { key: "Tab", shiftKey: true })).toBe(true);
    fireEvent.click(radios[1]);
    expect(filters.getByRole("radio", { checked: true })).toBe(radios[1]);
    expect(screen.getByRole("heading", { name: initial.work })).toBeTruthy();
  });

  it("编辑框保留光标导航，切换分组再回来时保留未提交的交接说明", async () => {
    const invoke = installReview(() => initial, cmd => { throw new Error(cmd); });
    renderApp();
    fireEvent.click(await screen.findByRole("button", { name: /^更多：/ }));
    fireEvent.click(await screen.findByRole("button", { name: "已交给对方" }));
    const input = screen.getByRole("textbox") as HTMLInputElement;
    const mine = screen.getByRole("radio", { name: "我来办 1" });
    fireEvent.change(input, { target: { value: "等工艺负责人核对" } });
    for (const key of ["ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Home", "End"]) {
      expect(fireEvent.keyDown(input, { key })).toBe(true);
      expect(document.activeElement).toBe(input);
      expect(mine.getAttribute("aria-checked")).toBe("true");
    }
    mine.focus();
    fireEvent.keyDown(mine, { key: "ArrowRight" });
    expect(screen.queryByRole("textbox")).toBeNull();
    expect(screen.getByText("目前没有在等对方的事项。")).toBeTruthy();
    fireEvent.keyDown(screen.getByRole("radio", { checked: true }), { key: "Home" });
    expect(document.activeElement).toBe(mine);
    expect((screen.getByRole("textbox") as HTMLInputElement).value).toBe("等工艺负责人核对");
    expect(invoke.mock.calls.some(([cmd]) => cmd === "ownership_move")).toBe(false);
  });
});

describe("会话依据与结论", () => {
  it("只结束选中的交付，保留同一群里后来提出的等待事项", async () => {
    const first = { ...initial, state: "waiting", work: "20份流程图", conclusion: "", evidenceDetails: [
      { time: "2026-11-10 09:15:00", conversation: "群聊：工艺评审", kind: "群聊", excerpt: "会上已核对20份流程图。" },
    ] };
    const later = { ...initial, id: "wi_later", state: "waiting", work: "20款手工线工序差异表" };
    let card = first;
    const invoke = installReview(() => card, (cmd, args) => {
      if (cmd !== "ownership_set_conclusion") throw new Error(cmd);
      card = { ...card, version: "v2", state: args?.resolved ? "done" : card.state, conclusion: String(args?.note) };
    }, false, () => ({ report: { cutoff: "", cards: [card, later] } }));
    const { client } = renderApp();
    client.setQueryData(["items", "all", ""], []);
    client.setQueryData(["item", first.id], {});
    fireEvent.click(await screen.findByRole("button", { name: `更多：${first.work}` }));
    expect(screen.getByText("2026年11月10日 09:15 · 群聊：工艺评审")).toBeTruthy();
    fireEvent.click(button("补充结论"));
    const field = screen.getByRole("textbox", { name: "我的结论" });
    expect(document.activeElement).toBe(field);
    expect(screen.getByText(first.evidenceDetails[0].excerpt)).toBeTruthy();
    fireEvent.change(field, { target: { value: " 会上已确认 " } });
    fireEvent.click(button("不再等对方，移至已完成"));
    await screen.findByText("已保存结论，「20份流程图」移至已完成。");
    expect(invoke).toHaveBeenCalledWith("ownership_set_conclusion", { id: first.id, version: "v1", note: "会上已确认", resolved: true });
    expect(screen.getByRole("radio", { name: "等对方 1", checked: true })).toBeTruthy();
    expect(screen.getByRole("heading", { name: later.work })).toBeTruthy();
    expect(client.getQueryState(["items", "all", ""])?.isInvalidated).toBe(true);
    expect(client.getQueryState(["item", first.id])?.isInvalidated).toBe(true);
    expect(screen.queryByRole("heading", { name: first.work })).toBeNull();
    expect(screen.getByRole("link", { name: "查看事项详情" }).getAttribute("href")).toBe(`/items/${first.id}`);
  });

  it("没有原文时也能记结论，版本冲突保留草稿供重试", async () => {
    let card = initial;
    let rejectSave!: (reason: string) => void;
    const pendingSave = new Promise<never>((_, reject) => { rejectSave = reject; });
    const invoke = installReview(() => card, (cmd, args) => {
      if (cmd !== "ownership_set_conclusion") throw new Error(cmd);
      if (args?.version === "v1") return pendingSave;
    });
    renderApp();
    fireEvent.click(await screen.findByRole("button", { name: /^更多：/ }));
    expect(screen.getByText("这份清单没有保留原文摘录。")).toBeTruthy();
    fireEvent.click(button("补充结论"));
    fireEvent.change(screen.getByRole("textbox"), { target: { value: "保留的草稿" } });
    fireEvent.click(button("仅保存结论"));
    fireEvent.click(button("仅保存结论"));
    await screen.findByText("正在保存…");
    await waitFor(() => expect(invoke.mock.calls.filter(([cmd]) => cmd === "ownership_set_conclusion")).toHaveLength(1));
    card = { ...card, version: "v2" };
    rejectSave("事项已被修改");
    await screen.findByText(/保存失败.*事项已被修改/);
    expect((screen.getByRole("textbox") as HTMLTextAreaElement).value).toBe("保留的草稿");
    fireEvent.click(button("仅保存结论"));
    await screen.findByText(`已保存「${card.work}」的结论。`);
    expect(invoke).toHaveBeenCalledWith("ownership_set_conclusion", { id: card.id, version: "v2", note: "保留的草稿", resolved: false });
  });
});

describe("会话事项流转", () => {
  it("交接后卡片原位离场，去目标分组查看时高亮；撤销时回到原卡；本地同 ID 只出现一次", async () => {
    let card = initial;
    const invoke = installReview(() => card, (cmd, args) => {
      if (cmd === "ownership_move") {
        card = { ...card, version: "v2", state: "waiting", action: String(args?.action) };
        return { undoToken: "undo-handoff" };
      }
      if (cmd === "ownership_undo") { card = { ...initial, version: "v3" }; return; }
      throw new Error(cmd);
    });
    const { client } = renderApp();
    client.setQueryData(["items", "all", ""], []);
    client.setQueryData(["item", initial.id], {});
    const details = await screen.findByRole("button", { name: /^更多：/ });
    expect(await screen.findByRole("link", { name: "另一件本地事项" })).toBeTruthy();
    expect(screen.getByRole("link", { name: card.work }).getAttribute("href")).toBe(`/items/${card.id}`);
    expect(screen.queryByText(/截至/)).toBeNull();
    expect(details.getAttribute("aria-expanded")).toBe("false");
    expect(screen.queryByRole("button", { name: "已交给对方" })).toBeNull();
    fireEvent.click(details);
    expect(details.getAttribute("aria-expanded")).toBe("true");
    expect(screen.getByText(initial.sourceSummary.event)).toBeTruthy();
    const filters = within(screen.getByRole("radiogroup", { name: "按事项归属浏览" }));
    expect(filters.getAllByRole("radio").map(node => node.textContent)).toEqual(["我来办 1", "等对方 0", "待我认领 0"]);
    fireEvent.click(button("已交给对方"));
    const input = screen.getByRole("textbox", { name: "在等谁做什么？" });
    expect(document.activeElement).toBe(input);
    fireEvent.change(input, { target: { value: "  " } });
    expect((button("保存") as HTMLButtonElement).disabled).toBe(true);
    fireEvent.change(input, { target: { value: "  等工艺负责人核对工序  " } });
    fireEvent.click(button("保存"));
    await screen.findByRole("button", { name: "查看等对方" });
    // 移走的卡片先在原位置收起离场：只是给视觉的快照，不再暴露给辅助技术。
    const leaving = document.querySelector<HTMLElement>(".ownership-leave");
    expect(leaving?.getAttribute("aria-hidden")).toBe("true");
    expect(leaving?.textContent).toContain("已移到等对方");
    fireEvent.animationEnd(leaving as HTMLElement);
    expect(document.querySelector(".ownership-leave")).toBeNull();
    expect(filters.getAllByRole("radio").map(node => node.textContent)).toEqual(["我来办 0", "等对方 1", "待我认领 0"]);
    fireEvent.click(button("查看等对方"));
    expect(screen.getByText("等工艺负责人核对工序")).toBeTruthy();
    expect(document.activeElement).toBe(screen.getByRole("link", { name: initial.work }));
    expect(screen.getByRole("heading", { name: initial.work }).closest("article")?.classList.contains("is-restored")).toBe(true);
    expect(invoke).toHaveBeenCalledWith("ownership_move", { id: initial.id, version: "v1", target: "waiting", action: "等工艺负责人核对工序" });
    expect(client.getQueryState(["item", initial.id])?.isInvalidated).toBe(true);
    expect(client.getQueryState(["items", "all", ""])?.isInvalidated).toBe(true);
    fireEvent.click(button("撤销"));
    expect(await screen.findByText(initial.action)).toBeTruthy();
    expect(invoke).toHaveBeenCalledWith("ownership_undo", { token: "undo-handoff" });
    expect(screen.queryByRole("button", { name: "撤销" })).toBeNull();
    expect(screen.getByRole("heading", { name: initial.work }).closest("article")?.classList.contains("is-restored")).toBe(true);
    expect(invoke.mock.calls.filter(([cmd]) => cmd === "today_list")).toHaveLength(3);
    expect(invoke.mock.calls.some(([cmd]) => cmd === "ownership_refresh" || cmd === "ownership_set_auto")).toBe(false);
  });

  it.each([
    ["mine", "办完了", "done", "已完成", false],
    ["waiting", "轮到我了", "mine", "我来办", true],
    ["waiting", "事情已结束", "done", "已完成", false],
    ["triage", "我来办", "mine", "我来办", false],
    ["triage", "我在等对方", "waiting", "等对方", true],
    ["triage", "不用我跟", "ignored", "已忽略", false],
  ] as const)("从 %s 点击「%s」写入 %s", async (from, label, target, group, edit) => {
    let card = { ...initial, state: from as string };
    const invoke = installReview(() => card, (cmd, args) => {
      if (cmd === "ownership_undo") { card = { ...initial, state: from, version: "v3" }; return; }
      if (cmd !== "ownership_move") throw new Error(cmd);
      card = { ...card, version: "v2", state: target, action: String(args?.action ?? card.action) };
      return { undoToken: "undo-1" };
    });
    renderApp();
    if (["事情已结束", "我在等对方", "不用我跟"].includes(label)) {
      fireEvent.click(await screen.findByRole("button", { name: /^更多：/ }));
    }
    fireEvent.click(await screen.findByRole("button", { name: label }));
    const action = edit ? "核对第二张样图" : undefined;
    if (edit) {
      fireEvent.click(button("取消"));
      expect(document.activeElement).toBe(button(label));
      fireEvent.click(button(label));
      fireEvent.change(screen.getByRole("textbox"), { target: { value: action } });
      fireEvent.click(button("保存"));
    }
    if (target === "done" || target === "ignored") {
      await screen.findByRole("button", { name: "撤销" });
      expect(screen.getByText(group, { selector: '[role="status"]' })).toBeTruthy();
      expect(screen.queryByRole("link", { name: "查看事项详情" })).toBeNull();
      expect(screen.queryByRole("heading", { name: card.work })).toBeNull();
      expect(screen.getByText(/当前清单没有需要跟进的会话事项/)).toBeTruthy();
      fireEvent.click(button("撤销"));
      await waitFor(() => expect(document.activeElement).toBe(screen.getByRole("link", { name: initial.work })));
    } else {
      fireEvent.click(await screen.findByRole("button", { name: `查看${group}` }));
      expect(screen.getByRole("heading", { name: card.work })).toBeTruthy();
    }
    expect(invoke).toHaveBeenCalledWith("ownership_move", { id: card.id, version: "v1", target, action });
  });

  it("连续办完两项时，最后一次操作拥有完整的五秒撤销时间", async () => {
    vi.useFakeTimers();
    try {
      let cards = [initial, { ...initial, id: "second", work: "复核第二项" }];
      installReview(() => initial, (cmd, args) => {
        if (cmd !== "ownership_move") throw new Error(cmd);
        cards = cards.map(card => card.id === args?.id ? { ...card, state: "done" } : card);
        return { undoToken: args?.id };
      }, false, () => ({ report: { cutoff: "", cards } }));
      renderApp();
      await elapse();
      fireEvent.click(screen.getAllByRole("button", { name: "办完了" })[0]);
      await elapse();
      expect(screen.getByRole("img", { name: "五秒内可撤销" })).toBeTruthy();
      await elapse(4000);
      fireEvent.click(button("办完了"));
      await elapse();
      await elapse(4999);
      expect(button("撤销")).toBeTruthy();
      await elapse(1);
      expect(screen.queryByRole("img", { name: "五秒内可撤销" })).toBeNull();
      expect(screen.queryByRole("button", { name: "撤销" })).toBeNull();
      expect(screen.queryByText("已完成", { selector: '[role="status"]' })).toBeNull();
    } finally {
      vi.useRealTimers();
    }
  });

  it("写入等待期间禁止重复操作，冲突后重新读取卡片版本", async () => {
    let card = initial;
    let rejectMove: ((message: string) => void) | undefined;
    const invoke = installReview(() => card, (cmd) => {
      if (cmd !== "ownership_move") throw new Error(cmd);
      return new Promise<never>((_, reject) => { rejectMove = reject; });
    });
    renderApp();
    fireEvent.click(await screen.findByRole("button", { name: "办完了" }));
    await screen.findByText("正在保存…");
    expect((button("办完了") as HTMLButtonElement).disabled).toBe(true);
    card = { ...initial, version: "v3" };
    rejectMove?.("事项已被修改");
    expect(await screen.findByText(/保存失败.*事项已被修改/)).toBeTruthy();
    expect(document.querySelector(".ownership-leave")).toBeNull();
    expect(screen.queryByRole("button", { name: "撤销" })).toBeNull();
    fireEvent.click(button("办完了"));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("ownership_move", { id: card.id, version: "v3", target: "done", action: undefined }));
    rejectMove?.("写入失败");
    await screen.findByText(/保存失败.*写入失败/);
  });

  it("编辑框里的 Esc 关闭编辑并把焦点还给按钮；输入法组字中的 Esc 不丢草稿", async () => {
    const invoke = installReview(() => initial, cmd => { throw new Error(cmd); });
    renderApp();
    fireEvent.click(await screen.findByRole("button", { name: /^更多：/ }));
    fireEvent.click(await screen.findByRole("button", { name: "已交给对方" }));
    const input = screen.getByRole("textbox", { name: "在等谁做什么？" });
    fireEvent.change(input, { target: { value: "等工艺负责人核对" } });
    fireEvent.keyDown(input, { key: "Escape", isComposing: true });
    expect(screen.getByRole("textbox", { name: "在等谁做什么？" })).toBe(input);
    fireEvent.keyDown(input, { key: "Escape" });
    expect(screen.queryByRole("textbox")).toBeNull();
    expect(document.activeElement).toBe(button("已交给对方"));
    expect(invoke.mock.calls.some(([cmd]) => cmd === "ownership_move")).toBe(false);
  });
});

describe("开启自动整理与不可用指引", () => {
  it("空清单直接给出开启入口，写明模型与近 30 天范围；点击后才写入开关", async () => {
    const invoke = installReview(() => initial, cmd => { if (cmd !== "ownership_set_auto") throw new Error(cmd); }, true, { report: null });
    renderApp();
    await screen.findByText("还没有当前账号的清单。");
    expect(invoke.mock.calls.some(([cmd]) => cmd === "ownership_set_auto")).toBe(false);
    expect(screen.queryByText("更新清单")).toBeNull();
    expect(screen.queryByRole("button", { name: /整理一次/ })).toBeNull();
    const consent = screen.getByText(/近 30 天.*workbuddy\/deepseek/);
    expect(button("开启自动整理").getAttribute("aria-describedby")).toBe(consent.id);
    fireEvent.click(button("开启自动整理"));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("ownership_set_auto", { profile: "org:user", enabled: true }));
    expect(invoke.mock.calls.some(([cmd]) => cmd === "ownership_refresh")).toBe(false);
  });

  it("已有清单但未开启时，开启入口排在卡片之后；今日页不重复提供单次整理", async () => {
    const invoke = installReview(() => initial, cmd => { throw new Error(cmd); }, true);
    renderApp();
    const item = await screen.findByRole("heading", { name: initial.work });
    const enable = button("开启自动整理");
    precedes(item, enable);
    expect(enable.getAttribute("aria-describedby")).toBe("ownership-consent");
    expect(screen.queryByRole("button", { name: /整理一次/ })).toBeNull();
    expect(invoke.mock.calls.some(([cmd]) => cmd === "ownership_refresh")).toBe(false);
  });

  it("自动整理已开启时只报告状态，不再出现任何整理入口", async () => {
    installReview(() => initial, cmd => { throw new Error(cmd); }, true, { autoEnabled: true, report: { cutoff: "2024-05-06T08:00", cards: [initial] } });
    renderApp();
    await screen.findByRole("heading", { name: initial.work });
    expect(screen.queryByRole("button", { name: /自动整理|整理一次/ })).toBeNull();
    expect(screen.getByText(/自动整理已开启，/)).toBeTruthy();
    expect(screen.getByText("清单截至 2024-05-06 08:00")).toBeTruthy();
  });

  it("后台整理失败时保留已有清单并说明；重试入口在设置页", async () => {
    installReview(() => initial, cmd => { throw new Error(cmd); }, true, { autoEnabled: true, error: "会话判读失败（退出码 1）" });
    renderApp();
    expect(await screen.findByRole("heading", { name: initial.work })).toBeTruthy();
    expect(screen.getByRole("alert").textContent).toBe("本轮整理失败：会话判读失败（退出码 1） 清单仍是上一次整理的结果。");
    expect(screen.queryByRole("button", { name: /自动整理|整理一次/ })).toBeNull();
  });

  it("未绑定账号时给出下一步指引，不写成失败，也不能开启", async () => {
    installReview(() => initial, cmd => { throw new Error(cmd); }, false, { report: null, profile: null, error: "尚未绑定归档账号，请先完成一次归档同步" });
    renderApp();
    expect(await screen.findByText("尚未绑定归档账号，请先完成一次归档同步")).toBeTruthy();
    expect(screen.getByText(/设置 → 全可访问会话同步/)).toBeTruthy();
    expect(screen.getByRole("link", { name: "去同步" }).getAttribute("href")).toBe("/settings#settings-archive");
    expect(screen.queryByRole("button", { name: /自动整理|整理一次/ })).toBeNull();
    expect(screen.queryByText(/失败/)).toBeNull();
  });

  it("凭据未配置时保留清单与流转，只在清单下方说明怎么恢复", async () => {
    installReview(() => initial, cmd => { throw new Error(cmd); }, false, { error: CREDENTIAL_ERROR });
    renderApp();
    const item = await screen.findByRole("button", { name: "办完了" });
    const note = screen.getByText(/^自动整理暂不可用：/);
    expect(note.textContent).toContain(CREDENTIAL_ERROR);
    expect(within(note).getByRole("link", { name: "去设置" }).getAttribute("href")).toBe("/settings#ownership-settings");
    precedes(item, note);
    expect(screen.queryByRole("alert")).toBeNull();
    expect(screen.queryByRole("button", { name: /自动整理|整理一次/ })).toBeNull();
    expect(screen.getByText(/^自动整理未开启/)).toBeTruthy();
  });

  it("自动整理已开启且凭据失效时，卡片上方只显示开关状态与清单时间", async () => {
    installReview(() => initial, cmd => { throw new Error(cmd); }, false, {
      autoEnabled: true, error: CREDENTIAL_ERROR, report: { cutoff: "2024-05-06T08:00", cards: [initial] },
    });
    renderApp();
    const item = await screen.findByRole("heading", { name: initial.work });
    const header = screen.getByRole("heading", { name: "会话事项" }).parentElement;
    expect(header?.textContent).toBe("会话事项自动整理已开启，清单截至 2024-05-06 08:00");
    const note = screen.getByText(/^自动整理暂不可用：/);
    expect(note.textContent).toContain(CREDENTIAL_ERROR);
    precedes(item, note);
    expect(screen.queryByRole("alert")).toBeNull();
  });
});

describe("自动模式下的后台刷新", () => {
  afterEach(() => vi.useRealTimers());

  it("定期读取报告；cutoff 更新不重建正在编辑的卡片，保存时用最新版本", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(2026, 10, 23, 20, 0));
    let card = initial;
    let cutoff = "2026-11-23T09:00";
    const invoke = installReview(() => card, cmd => {
      if (cmd !== "ownership_move") throw new Error(cmd);
      return { undoToken: "undo-1" };
    }, true, () => ({ autoEnabled: true, report: { cutoff, cards: [card] } }));
    const views = () => invoke.mock.calls.filter(([cmd]) => cmd === "ownership_view").length;
    renderApp();
    await elapse();
    expect(views()).toBe(1);
    // 今天的截止时间只显示时刻，ISO 的 T 不带到界面上。
    expect(screen.getByText("清单截至 09:00")).toBeTruthy();
    fireEvent.click(button(/^更多：/));
    fireEvent.click(button("已交给对方"));
    const input = screen.getByRole("textbox", { name: "在等谁做什么？" }) as HTMLInputElement;
    fireEvent.change(input, { target: { value: "等工艺负责人核对工序" } });
    // 新报告把同一张卡改成「等对方」并换了版本：编辑照常进行，卡片留在原分组直到编辑结束。
    cutoff = "2026-11-23T09:30";
    card = { ...card, version: "v2", state: "waiting", action: "模型改写的动作" };
    await elapse(OWNERSHIP_AUTO_POLL_MS - 1000);
    expect(views()).toBe(1);
    await elapse(1000);
    expect(views()).toBe(2);
    expect(screen.getByRole("textbox", { name: "在等谁做什么？" })).toBe(input);
    expect(input.value).toBe("等工艺负责人核对工序");
    expect(document.activeElement).toBe(input);
    expect(screen.getByRole("heading", { name: initial.work })).toBeTruthy();
    expect(screen.getByRole("radio", { name: "等对方 1" })).toBeTruthy();
    expect(screen.getByText("清单截至 09:30")).toBeTruthy();
    fireEvent.click(button("保存"));
    await elapse();
    expect(invoke).toHaveBeenCalledWith("ownership_move", { id: initial.id, version: "v2", target: "waiting", action: "等工艺负责人核对工序" });
    expect(button("查看等对方")).toBeTruthy();
    expect(screen.queryByRole("heading", { name: initial.work })).toBeNull();
    await elapse(OWNERSHIP_AUTO_POLL_MS);
    expect(views()).toBeGreaterThan(2);
  });

  it("后台刷新移走当前卡片时，焦点回到选中的分组", async () => {
    vi.useFakeTimers();
    vi.setSystemTime(new Date(2026, 10, 23, 20, 0));
    let cards: Card[] = [{ ...initial }];
    installReview(() => initial, cmd => { throw new Error(cmd); }, true, () => ({ autoEnabled: true, report: { cutoff: "2026-11-23T09:00", cards } }));
    renderApp();
    await elapse();
    button("办完了").focus();
    cards = [];
    await elapse(OWNERSHIP_AUTO_POLL_MS + 100);
    expect(document.activeElement).toBe(screen.getByRole("radio", { checked: true }));
    expect(screen.queryByRole("button", { name: "办完了" })).toBeNull();
  });
});

describe("今日层级", () => {
  it("有会话清单时会话事项在前，本地补充记录在后", async () => {
    installReview(() => initial, cmd => { throw new Error(cmd); });
    renderApp();
    await screen.findByRole("heading", { name: initial.work });
    precedes(screen.getByRole("heading", { name: "会话事项" }), screen.getByRole("heading", { name: "本地事项记录" }));
    expect(screen.getByRole("heading", { level: 3, name: "轮到我" })).toBeTruthy();
  });

  it("已收尾的清单仍可进入全部事项，本地同步时间未知也不会误报未同步", async () => {
    installBridge(cmd => {
      if (cmd === "db_info") return { dbPath: "/tmp/test.db", schemaVersion: 1 };
      if (cmd === "sync_status") return [];
      if (cmd === "today_list") return { groups: { mine: [], waiting: [], confirm: [] }, freshness: { state: "unknown", lastSuccessAt: null } };
      if (cmd === "ownership_view") return { profile: "org:user", canRun: true, autoEnabled: true, report: { cutoff: "", cards: [{ ...initial, state: "ignored" }] } };
      throw new Error(cmd);
    });
    renderApp();
    const history = await screen.findByRole("link", { name: "查看全部事项" });
    expect(history.getAttribute("href")).toBe("/items");
    expect(screen.queryByText(initial.work)).toBeNull();
    expect(within(screen.getByRole("main")).queryByText(/尚未同步|还没有当前账号的清单/)).toBeNull();
  });

  it.each([null, { cutoff: "", cards: [] }])("没有会话卡片而本地有可操作记录时，本地记录在前（%j）", async report => {
    installReview(() => initial, cmd => { throw new Error(cmd); }, true, { report });
    renderApp();
    await screen.findByRole("link", { name: "另一件本地事项" });
    precedes(screen.getByRole("heading", { name: "本地事项记录" }), screen.getByRole("heading", { name: "会话事项" }));
    expect(button("开启自动整理")).toBeTruthy();
  });
});
