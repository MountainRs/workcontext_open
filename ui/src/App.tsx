import { Link, Navigate, Route, Routes, useLocation } from "react-router-dom";
import { SkillsPage } from "./pages/SkillsPage";
import { OverviewPage, SearchPage } from "./pages/WorkspacePages";
import { SourcesPage } from "./pages/SettingsPage";
import { Sidebar } from "./components/Sidebar";
import { BridgeBanner } from "./components/IpcNotice";
import { RealtimeBridge } from "./components/RealtimeBridge";
import { TodayPage } from "./pages/TodayPage";
import { ItemsPage } from "./pages/ItemsPage";
import { ItemDetailPage } from "./pages/ItemDetailPage";
import { PoolPage } from "./pages/PoolPage";
import { SettingsPage } from "./pages/SettingsPage";
import { RecordsPage, RecordDetailPage } from "./pages/RecordsPage";
import { ProjectsPage } from "./pages/ProjectsPage";
import "./context.css";

export default function App() {
  const { pathname } = useLocation();
  return (
    <div className="app-layout">
      <RealtimeBridge />
      <Sidebar />
      <main className="main-content" id="content">
        <BridgeBanner />
        {/^\/(follow-up|items|today)(\/|$)/.test(pathname) && <nav aria-label="跟进事项视图" className="followup-nav"><Link to="/today" aria-current={pathname !== "/items" && !pathname.startsWith("/items/") ? "page" : undefined}>今日</Link><Link to="/items" aria-current={pathname.startsWith("/items") ? "page" : undefined}>全部事项</Link></nav>}
        <Routes>
          <Route path="/" element={<Navigate to="/overview" replace />} />
          <Route path="/overview" element={<OverviewPage />} />
          <Route path="/search" element={<SearchPage />} />
          <Route path="/follow-up" element={<TodayPage />} />
          <Route path="/skills" element={<SkillsPage />} />
          <Route path="/sources" element={<SourcesPage />} />
          <Route path="/today" element={<TodayPage />} />
          <Route path="/items" element={<ItemsPage />} />
          <Route path="/items/:id" element={<ItemDetailPage />} />
          <Route path="/pool" element={<PoolPage />} />
          <Route path="/settings" element={<SettingsPage />} />
          <Route path="/records" element={<RecordsPage />} />
          <Route path="/records/:id" element={<RecordDetailPage />} />
          <Route path="/projects/:id?" element={<ProjectsPage />} />
          <Route path="*" element={<Navigate to="/overview" replace />} />
        </Routes>
      </main>
    </div>
  );
}
