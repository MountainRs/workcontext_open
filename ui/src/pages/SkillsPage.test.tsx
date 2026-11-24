import { fireEvent, render, screen, waitFor } from "@testing-library/react";
import { QueryClient, QueryClientProvider } from "@tanstack/react-query";
import { describe, expect, it, vi } from "vitest";
import { SkillsPage } from "./SkillsPage";

const data = { profile: "corp:user", scopes: [{ id: "p1", title: "迁移项目" }], items: [
  { name: "诊断", purpose: "分析日志", path: "C:/one/SKILL.md", scopeId: null, global: true, available: true },
  { name: "地图", purpose: "查找数据", path: "C:/two/SKILL.md", scopeId: "p1", global: false, available: false },
] };
function renderRegistry() {
  const invoke = vi.fn(async (command: string) => command === "skills_list" ? data : null);
  window.__TAURI_INTERNALS__ = { invoke };
  render(<QueryClientProvider client={new QueryClient()}><SkillsPage /></QueryClientProvider>);
  return invoke;
}

describe("能力索引", () => {
  it("显示原始路径、缺失状态，可按用途和范围筛选", async () => {
    renderRegistry();
    expect(await screen.findByText("C:/two/SKILL.md")).toBeTruthy();
    expect(screen.getByText("文件缺失或不可读")).toBeTruthy();
    fireEvent.change(screen.getByLabelText("查找能力"), { target: { value: "数据" } });
    expect(screen.queryByText("诊断")).toBeNull();
    fireEvent.change(screen.getByLabelText("筛选范围"), { target: { value: "global" } });
    expect(screen.getByText("没有匹配的能力。")).toBeTruthy();
  });
  it("提交当前账号下已有范围和原始路径", async () => {
    const invoke = renderRegistry();
    await screen.findByText("分析日志");
    fireEvent.click(screen.getByRole("button", { name: "登记能力" }));
    for (const [label, value] of [["名称", "核对"], ["用途", "核对报告"], ["原始 SKILL.md 绝对路径", "D:/report/SKILL.md"], ["关联范围", "scope:p1"]]) {
      fireEvent.change(screen.getByLabelText(label), { target: { value } });
    }
    fireEvent.click(screen.getByRole("button", { name: "保存登记" }));
    await waitFor(() => expect(invoke).toHaveBeenCalledWith("skills_register", { profile: "corp:user", entry: { name: "核对", purpose: "核对报告", path: "D:/report/SKILL.md", scopeId: "p1" } }));
    expect(await screen.findByText("能力已登记")).toBeTruthy();
  });
});
