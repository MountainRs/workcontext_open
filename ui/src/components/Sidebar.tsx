import { useEffect, useState } from "react";
import { Link, useLocation, useMatch, useNavigate } from "react-router-dom";
import { hasDesktopBridge } from "../lib/ipc";
import { StatusBar } from "./StatusBar";
import { RecordIssueDialog, type IssueDefaults } from "./RecordIssueDialog";

const SHORTCUTS = ["/today", "/items", "/pool", "/settings", "/records", "/projects", "/overview", "/search", "/skills"];
const GROUPS = [
  { label: "工作空间", items: [
    ["/overview", "工作概览", "M4 4h7v7H4z M14 4h6v4h-6z M14 11h6v9h-6z M4 14h7v6H4z"],
    ["/projects", "重点项目", "M3 7h7l2 2h9v10H3z"],
    ["/records", "工作记录", "M5 4h14v16H5z M8 8h8m-8 4h8m-8 4h5"],
    ["/follow-up", "跟进事项", "M4 7h3m3 0h10M4 12h3m3 0h10M4 17h3m3 0h10"],
    ["/search", "全局检索", "M16 16l5 5 M18 10a8 8 0 1 1-16 0 8 8 0 0 1 16 0"],
  ]},
  { label: "系统", items: [
    ["/skills", "能力索引", "M3 5h7l2 2 2-2h7v14h-7l-2 2-2-2H3z M12 7v14"],
    ["/sources", "数据来源", "M4 5h16v5H4z M4 14h16v5H4z M7 7.5h1m-1 9h1"],
    ["/pool", "迭代反馈", "M4 5h16v12H9l-5 4z M8 9h8m-8 4h4"],
    ["/settings", "设置", "M4 8h6m4 0h6M4 16h10m4 0h2 M14 8a2 2 0 1 1-4 0 2 2 0 0 1 4 0Zm4 8a2 2 0 1 1-4 0 2 2 0 0 1 4 0Z"],
  ]},
];

export function Sidebar() {
  const location = useLocation();
  const itemMatch = useMatch("/items/:id");
  const [issueOpen, setIssueOpen] = useState(false);
  const navigate = useNavigate();
  const desktop = hasDesktopBridge();
  const mac = navigator.platform.startsWith("Mac");
  useEffect(() => {
    if (!desktop) return;
    const switchPage = (event: KeyboardEvent) => {
      if (event.defaultPrevented || event.repeat || event.isComposing || event.altKey || event.shiftKey) return;
      const hasShortcutModifier = mac ? event.metaKey && !event.ctrlKey : event.ctrlKey && !event.metaKey;
      if (!hasShortcutModifier) return;
      if (document.querySelector('[aria-modal="true"], dialog[open]')) return;
      if (event.target instanceof Element && event.target.closest('input, textarea, select, [contenteditable]:not([contenteditable="false"])')) return;
      const index = SHORTCUTS.findIndex((_, i) => String(i + 1) === event.key);
      if (index < 0) return;
      event.preventDefault();
      navigate(SHORTCUTS[index]);
    };
    window.addEventListener("keydown", switchPage);
    return () => window.removeEventListener("keydown", switchPage);
  }, [desktop, mac, navigate]);

  const issueDefaults: IssueDefaults = {
    route: location.pathname,
    targetType: itemMatch ? "work_item" : undefined,
    targetId: itemMatch?.params.id,
  };

  return (
    <nav aria-label="主导航" className="sidebar">
      <div className="sidebar-brand">
        <img className="sidebar-mark" src="/workcontext.svg" width="36" height="36" alt="" />
        <div>
          <span className="sidebar-brand-name">WorkContext</span>
          <span className="sidebar-brand-description">个人 AI 工作台</span>
        </div>
      </div>

      <div className="sidebar-nav">
        {GROUPS.map(group => <div className="workspace-nav-group" role="group" aria-label={group.label} key={group.label}>
          <p>{group.label}</p>
          {group.items.map(([to,label,icon]) => {
            const followUp = to === "/follow-up" && /^\/(today|items)(\/|$)/.test(location.pathname);
            const shortcut = SHORTCUTS.indexOf(to) + 1;
            const active = followUp || location.pathname === to || location.pathname.startsWith(`${to}/`);
            return <Link key={to} to={to} aria-current={active ? "page" : undefined}
              className={`nav-link${active ? " active" : ""}`}
              aria-keyshortcuts={desktop && shortcut ? `${mac ? "Meta" : "Control"}+${shortcut}` : undefined}>
              <svg className="sidebar-icon" viewBox="0 0 24 24" aria-hidden="true"><path d={icon} /></svg>
              {label}{desktop && shortcut > 0 && <kbd aria-hidden="true">{mac ? "⌘" : "Ctrl+"}{shortcut}</kbd>}
            </Link>;
          })}
        </div>)}
      </div>

      <div className="sidebar-actions">
        <button
          type="button"
          className="record-issue-btn"
          onClick={() => setIssueOpen(true)}
        >
          <svg className="sidebar-icon" viewBox="0 0 24 24" aria-hidden="true">
            <path d="M5 4.5h14v12H10L5 20zM9 9h6m-6 4h3" />
          </svg>
          <span>记录问题<span className="record-issue-hint">当前页面 · 默认脱敏</span></span>
        </button>
      </div>

      <StatusBar />

      <RecordIssueDialog
        open={issueOpen}
        onClose={() => setIssueOpen(false)}
        defaults={issueDefaults}
      />
    </nav>
  );
}
