import type { OverviewData, SymbolContext } from "./types";

export async function fetchJson<T>(url: string): Promise<T> {
  const response = await fetch(url, { headers: { Accept: "application/json" } });
  const text = await response.text();
  const payload = text ? JSON.parse(text) : {};
  if (!response.ok) throw new Error(payload.error ?? `${response.status} ${response.statusText}`);
  return payload as T;
}

export const api = {
  overview: () => fetchJson<OverviewData>("/api/graph/overview"),
  context: (id: string) => fetchJson<SymbolContext>(`/api/graph/context?id=${encodeURIComponent(id)}`),
  search: (query: string) => fetchJson<any>(`/api/graph/search?q=${encodeURIComponent(query)}&limit=20`),
  impact: (id: string, direction: string, depth: number) => fetchJson<any>(`/api/graph/impact?id=${encodeURIComponent(id)}&direction=${direction}&depth=${depth}`),
  flow: (id: string, depth: number) => fetchJson<any>(`/api/graph/flow?id=${encodeURIComponent(id)}&depth=${depth}`),
  communities: () => fetchJson<any>("/api/graph/communities"),
  routes: (prefix: string) => fetchJson<any>(`/api/graph/routes?limit=500${prefix ? `&prefix=${encodeURIComponent(prefix)}` : ""}`),
};
