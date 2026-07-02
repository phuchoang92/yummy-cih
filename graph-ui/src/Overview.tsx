import { Check, ChevronRight, RefreshCw, Search, X } from "lucide-react";
import { useEffect, useMemo, useState } from "react";
import { api } from "./api";
import type { GraphSummary } from "./api";
import { cameraTarget, GalaxyScene } from "./Scene";
import type { OverviewData, OverviewNode, SymbolContext } from "./types";

const KIND_COLORS: Record<string, string> = {
  Community: "#a78bfa", Process: "#f59e0b", Route: "#eab308", IntegrationRoute: "#22d3ee",
  Class: "#a855f7", Interface: "#c084fc", Method: "#06b6d4", Function: "#06b6d4",
  File: "#3b82f6", Folder: "#22c55e", DbTable: "#60a5fa", ExternalEndpoint: "#fb7185",
};

// These kinds are pre-selected in the selector for large graphs.
const STRUCTURAL_KINDS = new Set([
  "Community", "Process", "Route", "IntegrationRoute",
  "MessageDestination", "KafkaTopic", "ExternalEndpoint", "DbTable", "DbQuery",
]);

// Graphs with fewer nodes than this skip the selector and load immediately.
const SMALL_GRAPH_THRESHOLD = 10_000;

// Warn when a kind has more than this many nodes.
const LARGE_KIND_WARN = 10_000;

type Phase = "idle" | "selecting" | "loading" | "ready" | "error";

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

function KindSelector({ summary, selected, onChange, onLoad }: {
  summary: GraphSummary;
  selected: Set<string>;
  onChange: (kinds: Set<string>) => void;
  onLoad: () => void;
}) {
  const toggle = (kind: string) => {
    const next = new Set(selected);
    next.has(kind) ? next.delete(kind) : next.add(kind);
    onChange(next);
  };

  return (
    <div style={{
      position: "fixed", inset: 0, background: "rgba(6,9,15,0.88)",
      display: "flex", alignItems: "center", justifyContent: "center", zIndex: 50,
    }}>
      <div style={{
        background: "#0e2028", border: "1px solid rgba(98,150,158,.2)", borderRadius: "12px",
        padding: "24px", width: "380px", maxHeight: "85vh",
        display: "flex", flexDirection: "column", gap: "16px",
      }}>
        <div>
          <h2 style={{ margin: "0 0 4px", fontSize: "15px", color: "#e1ecec" }}>Graph Explorer</h2>
          <p style={{ margin: 0, color: "#709093", fontSize: "10px" }}>
            {summary.total_nodes.toLocaleString()} nodes · {summary.total_edges.toLocaleString()} edges
            <span style={{ marginLeft: 8, color: "#425d61" }}>— select kinds to load</span>
          </p>
        </div>

        <div style={{ display: "flex", flexDirection: "column", gap: "8px" }}>
          <div className="rail-label">
            <span>Node kinds</span>
            <button onClick={() => onChange(new Set(summary.kinds.map((k) => k.kind)))}>All</button>
            <button onClick={() => onChange(new Set())}>None</button>
          </div>
          <div style={{ display: "flex", flexDirection: "column", gap: "2px", maxHeight: "380px", overflowY: "auto" }}>
            {summary.kinds.map(({ kind, count }) => {
              const isLarge = count > LARGE_KIND_WARN;
              const isActive = selected.has(kind);
              return (
                <button
                  key={kind}
                  onClick={() => toggle(kind)}
                  style={{
                    display: "flex", alignItems: "center", gap: "8px", width: "100%",
                    border: "1px solid", borderColor: isActive ? "rgba(255,255,255,.1)" : "transparent",
                    borderRadius: "6px", background: isActive ? "rgba(255,255,255,.05)" : "transparent",
                    color: isActive ? "#9cb0b2" : "#425d61", padding: "5px 8px",
                    textAlign: "left", cursor: "pointer", fontSize: "10px", transition: ".12s",
                  }}
                >
                  <i style={{ width: 5, height: 5, borderRadius: "50%", flex: "none", background: KIND_COLORS[kind] ?? "#94a3b8" }} />
                  <span style={{ flex: 1 }}>{kind}</span>
                  <span style={{ fontVariantNumeric: "tabular-nums", color: "#425d61" }}>{count.toLocaleString()}</span>
                  {isLarge && <span style={{ color: "#f59e0b", fontSize: "9px" }}>⚠</span>}
                </button>
              );
            })}
          </div>
        </div>

        <button
          onClick={onLoad}
          disabled={selected.size === 0}
          style={{
            height: "34px", border: "1px solid rgba(29,162,126,.45)", borderRadius: "7px",
            background: selected.size === 0 ? "transparent" : "#1da27e",
            color: selected.size === 0 ? "#425d61" : "#03120e",
            fontWeight: 700, fontSize: "11px",
            cursor: selected.size === 0 ? "not-allowed" : "pointer",
          }}
        >
          Load Graph →
        </button>
      </div>
    </div>
  );
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
  const [phase, setPhase] = useState<Phase>("idle");
  const [summary, setSummary] = useState<GraphSummary | null>(null);
  const [selectedKinds, setSelectedKinds] = useState<Set<string>>(new Set());
  const [data, setData] = useState<OverviewData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [enabledKinds, setEnabledKinds] = useState<Set<string>>(new Set());
  const [enabledEdges, setEnabledEdges] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<Set<number> | null>(null);
  const [context, setContext] = useState<SymbolContext | null>(null);
  const [contextLoading, setContextLoading] = useState(false);
  const [search, setSearch] = useState("");
  const [leftWidth, setLeftWidth] = useState(() => storedWidth("cih-left-width", 276));
  const [rightWidth, setRightWidth] = useState(() => storedWidth("cih-right-width", 310));

  const loadOverview = async (kinds?: string[]) => {
    setPhase("loading");
    setError(null);
    try {
      const next = await api.overview(kinds);
      setData(next);
      setEnabledKinds(new Set(next.nodes.map((node) => node.kind)));
      setEnabledEdges(new Set(next.edges.map((edge) => edge.kind)));
      setPhase("ready");
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : "Unable to load graph overview");
      setPhase("error");
    }
  };

  useEffect(() => {
    void (async () => {
      try {
        const s = await api.summary();
        setSummary(s);
        const defaults = new Set((s.kinds ?? []).map((k) => k.kind).filter((k) => STRUCTURAL_KINDS.has(k)));
        setSelectedKinds(defaults);
        if (s.total_nodes < SMALL_GRAPH_THRESHOLD) {
          await loadOverview();
        } else {
          setPhase("selecting");
        }
      } catch (reason) {
        setError(reason instanceof Error ? reason.message : "Unable to reach graph server");
        setPhase("error");
      }
    })();
  }, []);

  const handleLoadFromSelector = () => {
    void loadOverview([...selectedKinds]);
  };

  const handleRefresh = () => {
    if (summary && summary.total_nodes >= SMALL_GRAPH_THRESHOLD) {
      setPhase("selecting");
    } else {
      void loadOverview();
    }
  };

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

  if (phase === "idle" || phase === "loading") {
    return <div className="center-state"><span className="spinner" /><strong>Computing repository layout</strong><small>Preparing the graph overview</small></div>;
  }
  if (phase === "error") {
    return <div className="center-state error-state"><strong>Overview unavailable</strong><span>{error}</span><button onClick={handleRefresh}>Retry</button></div>;
  }
  if (phase === "selecting" && summary) {
    return <KindSelector summary={summary} selected={selectedKinds} onChange={setSelectedKinds} onLoad={handleLoadFromSelector} />;
  }
  if (!data || data.nodes.length === 0) {
    return <div className="center-state"><strong>No graph data</strong><small>Index a repository, then refresh this view.</small></div>;
  }

  return <div className="overview-shell">
    <aside className="filter-rail" style={{ width: leftWidth }}>
      <div className="rail-section rail-heading"><span>Projection</span><button className="icon-button" onClick={handleRefresh} title="Change selection"><RefreshCw size={14} /></button></div>
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
