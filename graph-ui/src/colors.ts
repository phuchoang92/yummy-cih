// Single source of truth for graph colors, shared by Overview, ClassicViews,
// Scene, and Legend. Keep this the only place these palettes are defined.

/** Node-kind accent colors used by Canvas nodes, rail chips and the legend. */
export const KIND_COLORS: Record<string, string> = {
  Community: "#a78bfa", Process: "#f59e0b", Route: "#eab308", IntegrationRoute: "#22d3ee",
  Class: "#a855f7", Interface: "#c084fc", Method: "#06b6d4", Function: "#06b6d4",
  File: "#3b82f6", Folder: "#22c55e", DbTable: "#60a5fa", ExternalEndpoint: "#fb7185",
  Node: "#64748b",
};

/** Fallback for kinds without an explicit color. */
export const KIND_FALLBACK = "#94a3b8";

export function kindColor(kind: string): string {
  return KIND_COLORS[kind] ?? KIND_FALLBACK;
}

/** Edge-kind colors, keyed by the server's SCREAMING_SNAKE relationship labels. */
export const EDGE_COLORS: Record<string, string> = {
  CALLS: "#1da27e", HANDLES_ROUTE: "#eab308", IMPORTS: "#3b82f6",
  EXTENDS: "#f97316", IMPLEMENTS: "#a855f7", EXTERNAL_CALL: "#e11d48",
  PUBLISHES_EVENT: "#ec4899", LISTENS_TO: "#ec4899", INTEGRATION_LINK: "#06b6d4",
  READS_TABLE: "#60a5fa", WRITES_TABLE: "#fb7185", TESTS: "#22d3ee",
};

/** Fallback for edge kinds without an explicit color. */
export const EDGE_FALLBACK = "#1c8585";

export function edgeColor(kind: string): string {
  return EDGE_COLORS[kind] ?? EDGE_FALLBACK;
}

/** Humanize a SCREAMING_SNAKE edge label, e.g. HANDLES_ROUTE -> "handles route". */
export function edgeLabel(kind: string): string {
  return kind.replaceAll("_", " ").toLowerCase();
}
