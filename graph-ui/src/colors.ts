// Single source of truth for graph colors, shared by Overview, ClassicViews,
// Scene, and Legend. Keep this the only place these palettes are defined.

/** Node-kind accent colors — used for rail dots/chips, inspector, and the legend.
 *  Note: in the 3D overview, star color is degree-driven (see STELLAR_RAMP), not
 *  kind-driven; these colors identify kinds everywhere *outside* the star cloud. */
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

/** Degree -> star-color ramp, mirroring `stellar_color` in
 *  crates/cih-server/src/layout.rs. The server assigns each node's `color`; this
 *  copy exists only so the Legend can *describe* the ramp. Keep the stops and
 *  thresholds in sync with layout.rs if either changes. */
export const STELLAR_RAMP: { color: string; minDegree: number }[] = [
  { color: "#ff6050", minDegree: 0 },
  { color: "#ff8855", minDegree: 2 },
  { color: "#ffc070", minDegree: 4 },
  { color: "#ffe080", minDegree: 7 },
  { color: "#fff8e8", minDegree: 13 },
  { color: "#c0d0ff", minDegree: 26 },
  { color: "#80a0ff", minDegree: 51 },
];
