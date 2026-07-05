import type { OverviewData, SymbolContext } from "./types";

export interface KindCount {
  kind: string;
  count: number;
}

export interface GraphSummary {
  kinds: KindCount[];
  total_nodes: number;
  total_edges: number;
}

export async function fetchJson<T>(url: string): Promise<T> {
  const response = await fetch(url, { headers: { Accept: "application/json" } });
  const text = await response.text();
  const payload = text ? JSON.parse(text) : {};
  if (!response.ok) throw new Error(payload.error ?? `${response.status} ${response.statusText}`);
  return payload as T;
}

export const api = {
  summary: () => fetchJson<GraphSummary>("/api/graph/summary"),
  overview: (kinds?: string[]) =>
    fetchJson<OverviewData>(
      kinds?.length
        ? `/api/graph/overview?kinds=${encodeURIComponent(kinds.join(","))}`
        : "/api/graph/overview",
    ),
  context: (id: string) => fetchJson<SymbolContext>(`/api/graph/context?id=${encodeURIComponent(id)}`),
  search: (query: string) => fetchJson<any>(`/api/graph/search?q=${encodeURIComponent(query)}&limit=20`),
  impact: (id: string, direction: string, depth: number) => fetchJson<any>(`/api/graph/impact?id=${encodeURIComponent(id)}&direction=${direction}&depth=${depth}`),
  flow: (id: string, depth: number) => fetchJson<any>(`/api/graph/flow?id=${encodeURIComponent(id)}&depth=${depth}`),
  communities: () => fetchJson<any>("/api/graph/communities"),
  features: () => fetchJson<any>("/api/graph/features"),
  routes: (prefix: string) => fetchJson<any>(`/api/graph/routes?limit=500${prefix ? `&prefix=${encodeURIComponent(prefix)}` : ""}`),
};
