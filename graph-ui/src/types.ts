export interface OverviewNode {
  index: number;
  id: string;
  kind: string;
  name: string;
  qualified_name?: string | null;
  file: string;
  degree: number;
  x: number;
  y: number;
  z: number;
  size: number;
  color: string;
}

export interface OverviewEdge {
  source: number;
  target: number;
  kind: string;
}

export interface OverviewData {
  nodes: OverviewNode[];
  edges: OverviewEdge[];
  total_nodes: number;
  total_edges: number;
  truncated: boolean;
}

export interface ContextNode {
  id: string;
  kind: string;
  name: string;
  qualified_name?: string | null;
  file: string;
}

export interface SymbolContext {
  node: ContextNode;
  callers: ContextNode[];
  callees: ContextNode[];
  processes: string[];
  community?: { id: string; name: string; symbol_count: number; cohesion: number };
}

export type TabId = "overview" | "search" | "impact" | "flow" | "communities" | "routes";

export interface FlatGraphNode {
  id: string;
  label: string;
  kind: string;
  depth: number;
  file?: string;
  raw?: unknown;
}

export interface FlatGraphEdge {
  source: string;
  target: string;
  label: string;
}

export interface FlatGraph {
  nodes: FlatGraphNode[];
  edges: FlatGraphEdge[];
}
