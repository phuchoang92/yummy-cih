import { Check, ChevronRight, RefreshCw, Search, X } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { api } from "./api";
import { cameraTarget, GalaxyScene } from "./Scene";
import type { OverviewData, OverviewNode, SymbolContext } from "./types";

const KIND_COLORS: Record<string, string> = {
  Community: "#a78bfa", Process: "#f59e0b", Route: "#eab308", IntegrationRoute: "#22d3ee",
  Class: "#a855f7", Interface: "#c084fc", Method: "#06b6d4", Function: "#06b6d4",
  File: "#3b82f6", Folder: "#22c55e", DbTable: "#60a5fa", ExternalEndpoint: "#fb7185",
};

function clusterName(file: string, kind: string): string {
  const parts = file.split("/").filter(Boolean);
  return parts.length ? parts.slice(0, Math.min(2, parts.length)).join("/") : `@${kind}`;
}

function ResizeHandle({ side, onDelta }: { side: "left" | "right"; onDelta: (delta: number) => void }) {
  return <div className="resize-handle" onPointerDown={(event) => {
    event.currentTarget.setPointerCapture(event.pointerId);
    let last = event.clientX;
    const move = (next: PointerEvent) => {
      const raw = next.clientX - last; last = next.clientX;
      onDelta(side === "left" ? raw : -raw);
    };
    const up = () => { window.removeEventListener("pointermove", move); window.removeEventListener("pointerup", up); };
    window.addEventListener("pointermove", move); window.addEventListener("pointerup", up);
  }} />;
}

function Inspector({ context, loading, onClose, onNavigate }: {
  context: SymbolContext | null; loading: boolean; onClose: () => void; onNavigate: (id: string) => void;
}) {
  if (loading) return <aside className="inspector"><div className="panel-loading">Loading context…</div></aside>;
  if (!context) return null;
  const groups = [{ title: "Calls", items: context.callees }, { title: "Called by", items: context.callers }];
  return <aside className="inspector">
    <div className="inspector-head">
      <div><span className="kind-dot" style={{ background: KIND_COLORS[context.node.kind] ?? "#94a3b8" }} /><small>{context.node.kind}</small><h2>{context.node.name}</h2></div>
      <button className="icon-button" onClick={onClose} aria-label="Close inspector"><X size={16} /></button>
    </div>
    <p className="node-id">{context.node.qualified_name || context.node.id}</p>
    {context.node.file && <p className="node-file">{context.node.file}</p>}
    <div className="metric-row"><span><b>{context.callees.length}</b> outbound</span><span><b>{context.callers.length}</b> inbound</span></div>
    {context.community && <section className="inspector-section"><h3>Community</h3><p>{context.community.name}</p><small>{context.community.symbol_count.toLocaleString()} symbols · {(context.community.cohesion * 100).toFixed(0)}% cohesion</small></section>}
    {context.processes.length > 0 && <section className="inspector-section"><h3>Processes</h3>{context.processes.map((process) => <p key={process}>{process}</p>)}</section>}
    {groups.map((group) => group.items.length > 0 && <section className="inspector-section" key={group.title}>
      <h3>{group.title} <span>{group.items.length}</span></h3>
      <div className="connection-list">{group.items.slice(0, 50).map((item) => <button key={item.id} onClick={() => onNavigate(item.id)}><span className="kind-dot" style={{ background: KIND_COLORS[item.kind] ?? "#94a3b8" }} /><span><b>{item.name}</b><small>{item.kind}</small></span><ChevronRight size={13} /></button>)}</div>
    </section>)}
  </aside>;
}

export function Overview({ selectedId, onSelectedId }: { selectedId: string | null; onSelectedId: (id: string | null) => void }) {
  const [data, setData] = useState<OverviewData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [loading, setLoading] = useState(true);
  const [enabledKinds, setEnabledKinds] = useState<Set<string>>(new Set());
  const [enabledEdges, setEnabledEdges] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<Set<number> | null>(null);
  const [context, setContext] = useState<SymbolContext | null>(null);
  const [contextLoading, setContextLoading] = useState(false);
  const [search, setSearch] = useState("");
  const [leftWidth, setLeftWidth] = useState(() => storedWidth("cih-left-width", 276));
  const [rightWidth, setRightWidth] = useState(() => storedWidth("cih-right-width", 310));

  const load = async () => {
    setLoading(true); setError(null);
    try {
      const next = await api.overview(); setData(next);
      setEnabledKinds(new Set(next.nodes.map((node) => node.kind)));
      setEnabledEdges(new Set(next.edges.map((edge) => edge.kind)));
    } catch (reason) { setError(reason instanceof Error ? reason.message : "Unable to load graph overview"); }
    finally { setLoading(false); }
  };
  useEffect(() => { void load(); }, []);

  const nodeByIndex = useMemo(() => new Map(data?.nodes.map((node) => [node.index, node]) ?? []), [data]);
  const filteredNodes = useMemo(() => data?.nodes.filter((node) => enabledKinds.has(node.kind)) ?? [], [data, enabledKinds]);
  const filteredNodeIds = useMemo(() => new Set(filteredNodes.map((node) => node.index)), [filteredNodes]);
  const filteredEdges = useMemo(() => data?.edges.filter((edge) => enabledEdges.has(edge.kind) && filteredNodeIds.has(edge.source) && filteredNodeIds.has(edge.target)) ?? [], [data, enabledEdges, filteredNodeIds]);
  const target = useMemo(() => cameraTarget(filteredNodes, selected ?? new Set()), [filteredNodes, selected]);

  const selectNode = async (node: OverviewNode) => {
    const connected = new Set<number>([node.index]);
    for (const edge of filteredEdges) { if (edge.source === node.index) connected.add(edge.target); if (edge.target === node.index) connected.add(edge.source); }
    setSelected(connected); onSelectedId(node.id); setContextLoading(true);
    try { setContext(await api.context(node.id)); } catch { setContext(null); }
    finally { setContextLoading(false); }
  };
  const selectById = (id: string) => { const node = data?.nodes.find((item) => item.id === id); if (node) void selectNode(node); };
  useEffect(() => { if (selectedId && data && context?.node.id !== selectedId) selectById(selectedId); }, [selectedId, data]);

  const counts = useMemo(() => {
    const kinds = new Map<string, number>(); const edges = new Map<string, number>(); const clusters = new Map<string, number>();
    for (const node of data?.nodes ?? []) { kinds.set(node.kind, (kinds.get(node.kind) ?? 0) + 1); const key = clusterName(node.file, node.kind); clusters.set(key, (clusters.get(key) ?? 0) + 1); }
    for (const edge of data?.edges ?? []) edges.set(edge.kind, (edges.get(edge.kind) ?? 0) + 1);
    return { kinds: [...kinds].sort((a, b) => b[1] - a[1]), edges: [...edges].sort((a, b) => b[1] - a[1]), clusters: [...clusters].sort((a, b) => b[1] - a[1]) };
  }, [data]);
  const matches = useMemo(() => search.trim() ? filteredNodes.filter((node) => `${node.name} ${node.id} ${node.file}`.toLowerCase().includes(search.toLowerCase())).slice(0, 40) : [], [filteredNodes, search]);

  if (loading) return <div className="center-state"><span className="spinner" /><strong>Computing repository layout</strong><small>Preparing the bounded graph overview</small></div>;
  if (error) return <div className="center-state error-state"><strong>Overview unavailable</strong><span>{error}</span><button onClick={load}>Retry</button></div>;
  if (!data || data.nodes.length === 0) return <div className="center-state"><strong>No graph data</strong><small>Index a repository, then refresh this view.</small></div>;

  return <div className="overview-shell">
    <aside className="filter-rail" style={{ width: leftWidth }}>
      <div className="rail-section rail-heading"><span>Projection</span><button className="icon-button" onClick={load} title="Refresh"><RefreshCw size={14} /></button></div>
      <div className="projection-meta"><b>{data.nodes.length.toLocaleString()}</b> of {data.total_nodes.toLocaleString()} nodes<br/><b>{data.edges.length.toLocaleString()}</b> of {data.total_edges.toLocaleString()} edges{data.truncated && <em>bounded view</em>}</div>
      <div className="rail-section"><div className="rail-label"><span>Node types</span><button onClick={() => setEnabledKinds(new Set(counts.kinds.map(([kind]) => kind)))}>All</button><button onClick={() => setEnabledKinds(new Set())}>None</button></div><div className="filter-chips">{counts.kinds.map(([kind, count]) => <button key={kind} className={enabledKinds.has(kind) ? "is-active" : ""} onClick={() => setEnabledKinds((before) => { const next = new Set(before); next.has(kind) ? next.delete(kind) : next.add(kind); return next; })}><i style={{ background: KIND_COLORS[kind] ?? "#94a3b8" }} />{kind}<span>{count.toLocaleString()}</span></button>)}</div></div>
      <div className="rail-section"><div className="rail-label"><span>Relationships</span></div><div className="filter-chips edge-chips">{counts.edges.map(([kind, count]) => <button key={kind} className={enabledEdges.has(kind) ? "is-active" : ""} onClick={() => setEnabledEdges((before) => { const next = new Set(before); next.has(kind) ? next.delete(kind) : next.add(kind); return next; })}>{enabledEdges.has(kind) && <Check size={10} />}{kind.replaceAll("_", " ").toLowerCase()}<span>{count.toLocaleString()}</span></button>)}</div></div>
      <div className="rail-search"><Search size={14} /><input value={search} onChange={(event) => setSearch(event.target.value)} placeholder="Find node or file…" />{search && <button onClick={() => setSearch("")}><X size={13}/></button>}</div>
      <div className="tree-list">{search ? matches.map((node) => <button key={node.id} onClick={() => void selectNode(node)}><i style={{ background: node.color }} /><span><b>{node.name}</b><small>{node.file || node.kind}</small></span></button>) : counts.clusters.slice(0, 120).map(([cluster, count]) => <button key={cluster} onClick={() => { const ids = new Set(data.nodes.filter((node) => clusterName(node.file, node.kind) === cluster).map((node) => node.index)); setSelected(ids); }}><ChevronRight size={11}/><span><b>{cluster}</b></span><em>{count.toLocaleString()}</em></button>)}</div>
      {selected && <button className="clear-selection" onClick={() => { setSelected(null); setContext(null); onSelectedId(null); }}>Clear selection</button>}
    </aside>
    <ResizeHandle side="left" onDelta={(delta) => setLeftWidth((width) => { const next = Math.max(210, Math.min(480, width + delta)); storeWidth("cih-left-width", next); return next; })} />
    <main className="galaxy-workspace">
      <GalaxyScene nodes={filteredNodes} edges={filteredEdges} selected={selected} target={target} onSelect={(node) => void selectNode(node)} />
      <div className="canvas-hud"><span>{filteredNodes.length.toLocaleString()} stars</span><span>{filteredEdges.length.toLocaleString()} links</span>{selected && <span className="is-accent">{selected.size.toLocaleString()} focused</span>}</div>
    </main>
    {(context || contextLoading) && <><ResizeHandle side="right" onDelta={(delta) => setRightWidth((width) => { const next = Math.max(250, Math.min(520, width + delta)); storeWidth("cih-right-width", next); return next; })} /><div style={{ width: rightWidth }} className="inspector-wrap"><Inspector context={context} loading={contextLoading} onClose={() => { setContext(null); setSelected(null); onSelectedId(null); }} onNavigate={selectById} /></div></>}
  </div>;
}

function storedWidth(key: string, fallback: number): number {
  try { return Number(window.localStorage?.getItem(key)) || fallback; } catch { return fallback; }
}

function storeWidth(key: string, value: number): void {
  try { window.localStorage?.setItem(key, String(value)); } catch { /* storage is optional */ }
}
