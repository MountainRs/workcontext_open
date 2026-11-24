import { useQuery } from "@tanstack/react-query";
import { hasDesktopBridge, invoke } from "./ipc";
import { useLocation, useSearchParams } from "react-router-dom";

export type Scope = { id: string; kind: "project" | "domain"; title: string; goal: string; background: string; version: number };
export const recordKinds = { event: "事件", progress: "进展", decision: "决定", experience: "经验" };
export const origins = { manual: "本人手动录入", agent: "Agent 回写", archive: "钉钉归档" };
export const attributions = { reported: "来源陈述", inference: "推断", confirmed: "本人确认", derived: "自动整理" };
type Facet = { title: string; sourceRef: string; version: number; scopeId: string | null;
  classification: { facts: string[]; inferences: string[]; openQuestions: string[] } };
export type RecordSummary = { id: string; title: string; content: string; kind: keyof typeof recordKinds; occurredAt: number | null; createdAt: number; scopes: Scope[];
  highlights?: string[];
  scopeBasis: "source" | "correction" | "organizer"; sourceRef: string; producer: string | null; origin: keyof typeof origins; attribution: keyof typeof attributions; version: number; derived: Facet[] };
export type RecordDetail = { record: RecordSummary; original: string; materials: string[]; reportedResult: string | null; inference: string | null; openQuestions: string[];
  archiveSource?: { occurredAt: number; body: string | null; attachments: string[] };
  correction: { content: string | null; attribution: string | null; scopeIds: string[] | null } };
export type RecordsResult = { items: RecordSummary[]; total: number };

export function useContextWorkspace() {
  const [params] = useSearchParams();
  const localOnly = params.get("local") === "1";
  return { localOnly, suffix: localOnly ? "?local=1" : "", args: localOnly ? { localOnly: true } : {} };
}

export function scopeQueryKey(localOnly: boolean) {
  return localOnly ? ["context_scopes", "local"] : ["context_scopes"];
}

export function useContextScopes(localOnly = false) {
  return useQuery({ queryKey: scopeQueryKey(localOnly), queryFn: () => invoke<Scope[]>("context_scopes", localOnly ? { localOnly: true } : undefined), enabled: hasDesktopBridge(), retry: false, networkMode: "always" });
}

export function scopeKind(scope: Scope) {
  return scope.kind === "project" ? "项目" : "领域";
}

export function scopeLabel(scope: Scope, scopes: Scope[]) {
  const twins = scopes.filter(s => s.id !== scope.id && s.title === scope.title && s.kind === scope.kind);
  const label = `${scopeKind(scope)} · ${scope.title}`;
  if (!twins.length) return label;
  const description = scope.goal || scope.background;
  const ambiguous = !description || twins.some(s => (s.goal || s.background) === description);
  return [label, description, ambiguous && scope.id].filter(Boolean).join(" · ");
}

export function useListFilters() {
  const [params, setParams] = useSearchParams();
  const location = useLocation();
  function set(name: string, value: string) {
    const next = new URLSearchParams(params);
    if (name !== "offset") next.delete("offset");
    if (value) next.set(name, value); else next.delete(name);
    setParams(next, { replace: true, state: location.state });
  }
  return { get: (name: string) => params.get(name) ?? "", set };
}
