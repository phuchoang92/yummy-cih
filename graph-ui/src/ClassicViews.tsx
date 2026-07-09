import { Copy, Search } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { api } from "./api";
import type { FlatGraph, FlatGraphEdge, FlatGraphNode, TabId } from "./types";

function idOf(value: any): string {
  if (typeof value === "string") return value;
  return value?.id ?? value?.node_id ?? "";
}

function shortLabel(id: string): string { return id.split("#").pop()?.split(":").pop() || id; }

export function normalizeGraph(input: any): FlatGraph {
  const rawNodes = input?.nodes ?? [];
  const rawEdges = input?.links ?? input?.edges ?? [];
  const nodes = new Map<string, FlatGraphNode>();
  for (const raw of rawNodes) {
    const id = idOf(raw); if (!id || nodes.has(id)) continue;
    nodes.set(id, { id, label: raw.label ?? raw.name ?? shortLabel(id), kind: raw.kind ?? "Node", depth: Number(raw.depth ?? 0), file: raw.file ?? "", raw });
  }
  const edges: FlatGraphEdge[] = [];
  for (const raw of rawEdges) {
    const source = idOf(raw.source ?? raw.src); const target = idOf(raw.target ?? raw.dst);
    if (!source || !target) continue;
    if (!nodes.has(source)) nodes.set(source, { id: source, label: shortLabel(source), kind: "Node", depth: 0 });
    if (!nodes.has(target)) nodes.set(target, { id: target, label: shortLabel(target), kind: "Node", depth: 1 });
    edges.push({ source, target, label: raw.label ?? raw.kind ?? raw.via ?? "" });
  }
  return { nodes: [...nodes.values()], edges };
}

const KIND_COLORS: Record<string, string> = { Route: "#f59e0b", Class: "#a855f7", Interface: "#c084fc", Method: "#06b6d4", Function: "#06b6d4", Community: "#a78bfa", ExternalEndpoint: "#fb7185", Node: "#64748b" };

function DirectedGraph({ graph, selectedId, onSelect }: { graph: FlatGraph; selectedId: string | null; onSelect: (id: string) => void }) {
  const layout = useMemo(() => {
    const width = 1100, height = 720;
    const depths = new Map<number, FlatGraphNode[]>();
    const hasDepth = graph.nodes.some((node) => node.depth > 0);
    graph.nodes.forEach((node, index) => { const depth = hasDepth ? node.depth : index; const key = hasDepth ? depth : 0; depths.set(key, [...(depths.get(key) ?? []), node]); });
    const positions = new Map<string, { x: number; y: number }>();
    if (!hasDepth) graph.nodes.forEach((node, index) => { const angle = Math.PI * 2 * index / Math.max(1, graph.nodes.length); positions.set(node.id, { x: width / 2 + Math.cos(angle) * Math.min(310, 80 + graph.nodes.length * 6), y: height / 2 + Math.sin(angle) * Math.min(260, 70 + graph.nodes.length * 5) }); });
    else {
      const ordered = [...depths].sort((a, b) => a[0] - b[0]);
      ordered.forEach(([_, nodes], column) => nodes.forEach((node, row) => positions.set(node.id, { x: 100 + column * ((width - 200) / Math.max(1, ordered.length - 1)), y: 70 + row * ((height - 140) / Math.max(1, nodes.length - 1)) })));
    }
    return { width, height, positions };
  }, [graph]);
  if (!graph.nodes.length) return <div className="graph-empty"><strong>No graph loaded</strong><span>Run the selected query to inspect a bounded directional view.</span></div>;
  return <svg className="directed-graph" viewBox={`0 0 ${layout.width} ${layout.height}`} role="img" aria-label="Directed code graph">
    <defs><marker id="classic-arrow" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="6" markerHeight="6" orient="auto"><path d="M0 0L10 5L0 10z" fill="#536475" /></marker></defs>
    <g>{graph.edges.map((edge, index) => { const source = layout.positions.get(edge.source), target = layout.positions.get(edge.target); if (!source || !target) return null; return <g key={`${edge.source}-${edge.target}-${index}`}><line x1={source.x} y1={source.y} x2={target.x} y2={target.y} markerEnd="url(#classic-arrow)" /><text x={(source.x + target.x) / 2} y={(source.y + target.y) / 2 - 5}>{edge.label}</text></g>; })}</g>
    <g>{graph.nodes.map((node) => { const point = layout.positions.get(node.id)!; const active = node.id === selectedId; return <g key={node.id} className={active ? "classic-node is-selected" : "classic-node"} transform={`translate(${point.x} ${point.y})`} onClick={() => onSelect(node.id)} role="button" tabIndex={0}><circle r={active ? 18 : 14} fill={KIND_COLORS[node.kind] ?? "#64748b"}/><text y={31}>{node.label.slice(0, 28)}</text><title>{node.kind}: {node.id}</title></g>; })}</g>
  </svg>;
}

function Toolbar({ children }: { children: React.ReactNode }) { return <div className="classic-toolbar">{children}</div>; }

export function ClassicViews({ tab, selectedId, onSelectedId }: { tab: Exclude<TabId, "overview">; selectedId: string | null; onSelectedId: (id: string) => void }) {
  const [graph, setGraph] = useState<FlatGraph>({ nodes: [], edges: [] });
  const [items, setItems] = useState<any[]>([]);
  const [query, setQuery] = useState(""); const [prefix, setPrefix] = useState("");
  const [direction, setDirection] = useState("upstream"); const [depth, setDepth] = useState(4);
  const [loading, setLoading] = useState(false); const [error, setError] = useState<string | null>(null);
  const [exportValue, setExportValue] = useState("");
  const [clusters, setClusters] = useState<any[]>([]); const [selectedCluster, setSelectedCluster] = useState<string | null>(null); const [note, setNote] = useState("");

  const run = async (action: () => Promise<any>, project: (data: any) => void) => {
    setLoading(true); setError(null);
    try { project(await action()); } catch (reason) { setError(reason instanceof Error ? reason.message : "Request failed"); }
    finally { setLoading(false); }
  };
  const loadCommunities = () => run(api.communities, (data) => { setGraph(normalizeGraph(data)); setItems(data.nodes ?? []); });
  const loadRoutes = () => run(() => api.routes(prefix), (data) => { setItems(data.routes ?? []); setExportValue(JSON.stringify(data.openapi ?? {}, null, 2)); setGraph({ nodes: [], edges: [] }); });
  const loadClusters = () => run(api.features, (data) => { const list = data.clusters ?? []; setClusters(list); setNote(data.note ?? ""); setSelectedCluster((prev) => { if (prev && list.some((c: any) => c.name === prev)) return prev; const firstReal = list.find((c: any) => c.name !== "shared"); return (firstReal ?? list[0])?.name ?? null; }); });
  useEffect(() => { setError(null); setItems([]); setGraph({ nodes: [], edges: [] }); setClusters([]); setSelectedCluster(null); setNote(""); if (tab === "communities") void loadCommunities(); if (tab === "routes") void loadRoutes(); if (tab === "clusters") void loadClusters(); }, [tab]);

  if (tab === "clusters") {
    const current = clusters.find((c) => c.name === selectedCluster);
    return <div className="classic-shell">
      <Toolbar><div><span>{tab}</span><h1>Embedding clusters</h1></div><div className="classic-controls"><button onClick={() => void loadClusters()}>Refresh clusters</button></div></Toolbar>
      <div className="classic-body">
        <aside className="result-rail"><div className="result-heading"><span>Clusters</span><b>{clusters.length}</b></div>{[...clusters].sort((a, b) => (a.name === "shared" ? 1 : 0) - (b.name === "shared" ? 1 : 0)).map((c) => { const isShared = c.name === "shared"; return <button key={c.name} onClick={() => setSelectedCluster(c.name)} className={`${c.name === selectedCluster ? "is-active " : ""}${isShared ? "is-unclustered" : ""}`}><small>{c.node_count} nodes · {Math.round((c.avg_confidence ?? 0) * 100)}% avg</small><strong>{isShared ? "shared · unclustered" : c.name}</strong></button>; })}</aside>
        <main className="classic-stage">{loading ? <div className="graph-empty"><span className="spinner"/><strong>Loading clusters</strong></div>
          : error ? <div className="graph-empty error-state"><strong>Request failed</strong><span>{error}</span></div>
          : !clusters.length ? <div className="graph-empty"><strong>No embedding clusters</strong><span>{note || "Run `cih-engine discover <repo> --feature-strategy embed` to generate them."}</span></div>
          : !current ? <div className="graph-empty"><strong>Select a cluster</strong><span>Pick a cluster to inspect its member nodes.</span></div>
          : <div className="cluster-members"><div className="cluster-members-head"><h2>{current.name === "shared" ? "shared · unclustered" : current.name}</h2><span>{current.name === "shared" ? `${current.node_count} first-party nodes the clusterer couldn't place` : `${current.node_count} nodes · ${Math.round((current.avg_confidence ?? 0) * 100)}% avg confidence · lowest-confidence first`}</span></div><ul>{current.members.map((m: any) => { const kind = m.node_id.split(":")[0] || "Node"; const conf = Math.round((m.confidence ?? 0) * 100); const weak = (m.confidence ?? 0) < 0.5; return <li key={m.node_id} className={m.node_id === selectedId ? "is-active" : ""} onClick={() => onSelectedId(m.node_id)} title={m.evidence}><span className="member-kind" style={{ color: KIND_COLORS[kind] ?? "#64748b" }}>{kind}</span><span className="member-name">{shortLabel(m.node_id)}</span>{m.pinned && <span className="member-pin">pinned</span>}<span className={weak ? "member-conf is-weak" : "member-conf"}>{conf}%</span></li>; })}</ul></div>}</main>
      </div>
    </div>;
  }

  const controls = tab === "search" ? <form onSubmit={(event) => { event.preventDefault(); if (query.trim()) void run(() => api.search(query.trim()), (data) => { setItems(data.hits ?? []); setGraph(normalizeGraph(data.subgraph)); }); }}><Search size={15}/><input value={query} onChange={(event) => setQuery(event.target.value)} placeholder="Search symbol, route, table, or feature"/><button>Search</button></form>
    : tab === "impact" ? <><select value={direction} onChange={(event) => setDirection(event.target.value)}><option value="upstream">Upstream</option><option value="downstream">Downstream</option><option value="both">Both</option></select><input type="number" min="1" max="8" value={depth} onChange={(event) => setDepth(Number(event.target.value))}/><button disabled={!selectedId} onClick={() => selectedId && void run(() => api.impact(selectedId, direction, depth), (data) => setGraph(normalizeGraph(data)))}>Load impact</button></>
    : tab === "flow" ? <><input type="number" min="1" max="10" value={depth} onChange={(event) => setDepth(Number(event.target.value))}/><button disabled={!selectedId} onClick={() => selectedId && void run(() => api.flow(selectedId, depth), (data) => { setGraph(normalizeGraph(data)); setExportValue(data.mermaid ?? ""); })}>Trace flow</button>{exportValue && <button className="secondary" onClick={() => navigator.clipboard.writeText(exportValue)}><Copy size={13}/> Mermaid</button>}</>
    : tab === "communities" ? <button onClick={() => void loadCommunities()}>Refresh communities</button>
    : <><input value={prefix} onChange={(event) => setPrefix(event.target.value)} placeholder="Route prefix, e.g. /api"/><button onClick={() => void loadRoutes()}>Load routes</button>{exportValue && <button className="secondary" onClick={() => navigator.clipboard.writeText(exportValue)}><Copy size={13}/> OpenAPI</button>}</>;

  return <div className="classic-shell">
    <Toolbar><div><span>{tab}</span><h1>{tab === "search" ? "Find graph context" : tab === "impact" ? "Dependency impact" : tab === "flow" ? "Execution flow" : tab === "communities" ? "Community map" : "HTTP routes"}</h1></div><div className="classic-controls">{controls}</div></Toolbar>
    <div className="classic-body">
      {(tab === "search" || tab === "communities" || tab === "routes") && <aside className="result-rail"><div className="result-heading"><span>Results</span><b>{items.length}</b></div>{items.map((item, index) => { const id = idOf(item.handler_id ?? item.node_id ?? item.id); return <button key={id || index} onClick={() => id && onSelectedId(id)} className={id === selectedId ? "is-active" : ""}><small>{item.kind ?? item.http_method ?? "Node"}</small><strong>{item.name ?? item.path ?? shortLabel(id)}</strong><span>{item.qualified_name ?? item.handler_qualified ?? item.file ?? id}</span></button>; })}</aside>}
      <main className="classic-stage">{loading ? <div className="graph-empty"><span className="spinner"/><strong>Loading {tab}</strong></div> : error ? <div className="graph-empty error-state"><strong>Request failed</strong><span>{error}</span></div> : tab === "routes" ? <div className="route-canvas"><strong>Select a route to inspect its handler.</strong><span>The route list remains intentionally textual; graph relationships appear in Overview and Flow.</span></div> : <DirectedGraph graph={graph} selectedId={selectedId} onSelect={onSelectedId}/>}</main>
    </div>
  </div>;
}
