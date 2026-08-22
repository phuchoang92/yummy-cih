import { ArrowLeft, Check, ChevronRight, Gauge, Maximize, RefreshCw, Search, Sparkles, Tag, X } from "lucide-react";
import { useEffect, useMemo, useRef, useState } from "react";
import { api } from "./api";
import { GraphCanvas } from "./Scene";
import { kindColor } from "./colors";
import { Legend } from "./Legend";
import type { GraphMode, OverviewData, OverviewEdge, OverviewNode, SymbolContext } from "./types";

type Scope = "repository" | "community" | "file";
type Phase = "loading" | "ready" | "error";
type NavigationState = "idle" | "loading-in" | "enter-in" | "enter-out" | "refreshing";
type LoadIntent = "initial" | "in" | "refresh";
interface Crumb { scope: Scope; id?: string; label: string }
interface ProjectionSnapshot {
  crumb: Crumb;
  data: OverviewData;
  enabledKinds: Set<string>;
  enabledEdges: Set<string>;
  search: string;
}
const GRAPH_MODE_KEY = "cih-graph-mode";
const PROJECTION_TRANSITION_MS = 200;

function ResizeHandle({ side, onDelta }: { side: "left" | "right"; onDelta: (delta: number) => void }) {
  return <div className="resize-handle" onPointerDown={(event) => {
    event.currentTarget.setPointerCapture(event.pointerId);
    let last = event.clientX;
    const move = (next: PointerEvent) => { const raw = next.clientX - last; last = next.clientX; onDelta(side === "left" ? raw : -raw); };
    const up = () => { window.removeEventListener("pointermove", move); window.removeEventListener("pointerup", up); };
    window.addEventListener("pointermove", move); window.addEventListener("pointerup", up);
  }} />;
}

function mergeExpansion(base: OverviewData, layer: OverviewData, parent: OverviewNode): OverviewData {
  const nodes = base.nodes.map((node) => ({ ...node, pinned: true }));
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const layerIndexToId = new Map(layer.nodes.map((node) => [node.index, node.id]));
  for (const [offset, node] of layer.nodes.entries()) {
    if (byId.has(node.id)) continue;
    const angle = offset * 2.399963;
    const added = {
      ...node,
      index: nodes.length,
      x: parent.x + Math.cos(angle) * (36 + Math.sqrt(offset + 1) * 10),
      y: parent.y + Math.sin(angle) * (36 + Math.sqrt(offset + 1) * 10),
      pinned: false,
    };
    nodes.push(added);
    byId.set(added.id, added);
  }
  const edgeKeys = new Set(base.edges.map((edge) => `${edge.source}:${edge.target}:${edge.kind}`));
  const edges = [...base.edges];
  for (const edge of layer.edges) {
    const sourceId = layerIndexToId.get(edge.source); const targetId = layerIndexToId.get(edge.target);
    const source = sourceId ? byId.get(sourceId) : undefined; const target = targetId ? byId.get(targetId) : undefined;
    if (!source || !target) continue;
    const next: OverviewEdge = { ...edge, source: source.index, target: target.index };
    const key = `${next.source}:${next.target}:${next.kind}`;
    if (!edgeKeys.has(key)) { edges.push(next); edgeKeys.add(key); }
  }
  return {
    ...base,
    nodes: nodes.slice(0, 10_000),
    edges: edges.filter((edge) => edge.source < 10_000 && edge.target < 10_000).slice(0, 50_000),
    total_nodes: Math.max(base.total_nodes, nodes.length),
    total_edges: Math.max(base.total_edges, edges.length),
    truncated: base.truncated || layer.truncated || nodes.length > 10_000 || edges.length > 50_000,
  };
}

function Inspector({ node, context, loading, onClose, onExplore }: {
  node: OverviewNode; context: SymbolContext | null; loading: boolean; onClose: () => void; onExplore: () => void;
}) {
  return <aside className="inspector">
    <div className="inspector-head">
      <div><span className="kind-dot" style={{ background: kindColor(node.kind) }} /><small>{node.kind} · {node.role ?? "entity"}</small><h2>{node.name}</h2></div>
      <button className="icon-button" onClick={onClose} aria-label="Close inspector"><X size={16} /></button>
    </div>
    <p className="node-id">{node.id}</p>
    <div className="metric-row"><span><b>{node.member_count ?? 1}</b> members</span><span><b>{node.degree}</b> degree</span></div>
    {node.expandable !== false && <button className="kind-selector-load inspector-action" onClick={onExplore}>
      {node.kind === "Community" || node.kind === "File" ? "Open level" : "Expand one hop"} →
    </button>}
    {loading && <div className="panel-loading">Loading context…</div>}
    {context?.community && <section className="inspector-section"><h3>Community</h3><p>{context.community.name}</p><small>{context.community.symbol_count.toLocaleString()} symbols · {(context.community.cohesion * 100).toFixed(0)}% cohesion</small></section>}
    {context && [{ title: "Calls", items: context.callees }, { title: "Called by", items: context.callers }].map((group) => group.items.length > 0 && <section className="inspector-section" key={group.title}>
      <h3>{group.title} <span>{group.items.length}</span></h3>
      <div className="connection-list">{group.items.slice(0, 50).map((item) => <div key={item.id} className="connection-item"><span className="kind-dot" style={{ background: kindColor(item.kind) }} /><span><b>{item.name}</b><small>{item.kind}</small></span></div>)}</div>
    </section>)}
  </aside>;
}

export function Overview({ selectedId, onSelectedId }: { selectedId: string | null; onSelectedId: (id: string | null) => void }) {
  const [phase, setPhase] = useState<Phase>("loading");
  const [data, setData] = useState<OverviewData | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [breadcrumbs, setBreadcrumbs] = useState<Crumb[]>([{ scope: "repository", label: "Repository" }]);
  const [enabledKinds, setEnabledKinds] = useState<Set<string>>(new Set());
  const [enabledEdges, setEnabledEdges] = useState<Set<string>>(new Set());
  const [selected, setSelected] = useState<Set<number> | null>(null);
  const [selectedNode, setSelectedNode] = useState<OverviewNode | null>(null);
  const [context, setContext] = useState<SymbolContext | null>(null);
  const [contextLoading, setContextLoading] = useState(false);
  const [search, setSearch] = useState("");
  const [showLabels, setShowLabels] = useState(true);
  const [graphMode, setGraphMode] = useState<GraphMode>(storedGraphMode);
  const [physicsError, setPhysicsError] = useState<string | null>(null);
  const [navigation, setNavigation] = useState<NavigationState>("idle");
  const [pendingLabel, setPendingLabel] = useState("");
  const [resetNonce, setResetNonce] = useState(0);
  const [leftWidth, setLeftWidth] = useState(() => storedWidth("cih-left-width", 276));
  const [rightWidth, setRightWidth] = useState(() => storedWidth("cih-right-width", 310));
  const contextGeneration = useRef(0);
  const projectionGeneration = useRef(0);
  const historyRef = useRef<ProjectionSnapshot[]>([]);
  const transitionTimerRef = useRef<number | null>(null);

  const resetSelection = () => {
    contextGeneration.current += 1;
    setSelected(null); setSelectedNode(null); setContext(null); setContextLoading(false); onSelectedId(null);
  };

  const beginTransition = (next: Extract<NavigationState, "enter-in" | "enter-out">) => {
    if (transitionTimerRef.current != null) window.clearTimeout(transitionTimerRef.current);
    setNavigation(next);
    transitionTimerRef.current = window.setTimeout(() => {
      transitionTimerRef.current = null;
      setNavigation("idle");
    }, PROJECTION_TRANSITION_MS);
  };

  const loadProjection = async ({ scope, parentId, label, intent }: {
    scope: Scope; parentId?: string; label?: string; intent: LoadIntent;
  }) => {
    if (transitionTimerRef.current != null) {
      window.clearTimeout(transitionTimerRef.current);
      transitionTimerRef.current = null;
    }
    const generation = ++projectionGeneration.current;
    const initial = intent === "initial" || !data;
    const currentCrumb = breadcrumbs.at(-1) ?? { scope: "repository" as const, label: "Repository" };
    const snapshot = data ? {
      crumb: currentCrumb,
      data,
      enabledKinds: new Set(enabledKinds),
      enabledEdges: new Set(enabledEdges),
      search,
    } satisfies ProjectionSnapshot : null;
    if (initial) setPhase("loading");
    else {
      setNavigation(intent === "in" ? "loading-in" : "refreshing");
      setPendingLabel(intent === "in" ? (label ?? parentId ?? scope) : currentCrumb.label);
    }
    setError(null);
    try {
      const next = await api.projection(scope, parentId);
      if (generation !== projectionGeneration.current) return;
      setData(next);
      const availableKinds = new Set(next.nodes.map((node) => node.kind));
      const availableEdges = new Set(next.edges.map((edge) => edge.kind));
      if (intent === "refresh" && data) {
        setEnabledKinds(intersection(enabledKinds, availableKinds));
        setEnabledEdges(intersection(enabledEdges, availableEdges));
      } else {
        setEnabledKinds(availableKinds);
        setEnabledEdges(availableEdges);
      }
      if (intent === "in" && snapshot) {
        historyRef.current = [...historyRef.current, snapshot];
        setBreadcrumbs((before) => [...before, { scope, id: parentId, label: label ?? parentId ?? scope }]);
        setSearch("");
      } else if (initial) {
        historyRef.current = [];
        setBreadcrumbs([{ scope: "repository", label: "Repository" }]);
      }
      resetSelection();
      setPhase("ready");
      if (initial || intent === "refresh") setNavigation("idle");
      else beginTransition("enter-in");
    } catch (reason) {
      if (generation !== projectionGeneration.current) return;
      setError(reason instanceof Error ? reason.message : "Unable to load graph projection");
      if (data) setNavigation("idle");
      else setPhase("error");
    }
  };

  useEffect(() => { void loadProjection({ scope: "repository", intent: "initial" }); }, []);
  useEffect(() => () => {
    if (transitionTimerRef.current != null) window.clearTimeout(transitionTimerRef.current);
  }, []);

  const filteredNodes = useMemo(() => data?.nodes.filter((node) => enabledKinds.has(node.kind)) ?? [], [data, enabledKinds]);
  const filteredNodeIds = useMemo(() => new Set(filteredNodes.map((node) => node.index)), [filteredNodes]);
  const filteredEdges = useMemo(() => data?.edges.filter((edge) => enabledEdges.has(edge.kind) && filteredNodeIds.has(edge.source) && filteredNodeIds.has(edge.target)) ?? [], [data, enabledEdges, filteredNodeIds]);
  const counts = useMemo(() => {
    const kinds = new Map<string, number>(); const edges = new Map<string, number>();
    for (const node of data?.nodes ?? []) kinds.set(node.kind, (kinds.get(node.kind) ?? 0) + 1);
    for (const edge of data?.edges ?? []) edges.set(edge.kind, (edges.get(edge.kind) ?? 0) + 1);
    return { kinds: [...kinds].sort((a, b) => b[1] - a[1]), edges: [...edges].sort((a, b) => b[1] - a[1]) };
  }, [data]);
  const matches = useMemo(() => {
    const needle = search.trim().toLowerCase();
    return needle ? filteredNodes.filter((node) => `${node.name} ${node.id}`.toLowerCase().includes(needle)).slice(0, 120) : filteredNodes.slice(0, 120);
  }, [filteredNodes, search]);

  const selectNode = async (node: OverviewNode) => {
    const generation = ++contextGeneration.current;
    const connected = new Set<number>([node.index]);
    for (const edge of filteredEdges) { if (edge.source === node.index) connected.add(edge.target); if (edge.target === node.index) connected.add(edge.source); }
    setSelected(connected); setSelectedNode(node); onSelectedId(node.id); setContext(null); setContextLoading(false);
    if ((node.role ?? "entity") !== "entity") return;
    setContextLoading(true);
    try {
      const next = await api.context(node.id);
      if (contextGeneration.current === generation) setContext(next);
    } catch {
      if (contextGeneration.current === generation) setContext(null);
    } finally {
      if (contextGeneration.current === generation) setContextLoading(false);
    }
  };

  const exploreNode = async (node: OverviewNode) => {
    if (node.kind === "Community") { await loadProjection({ scope: "community", parentId: node.id, label: node.name, intent: "in" }); return; }
    if (node.kind === "File") { await loadProjection({ scope: "file", parentId: node.id, label: node.name, intent: "in" }); return; }
    if (!data) return;
    try {
      const layer = await api.expand(node.id, [...enabledEdges]);
      setData(mergeExpansion(data, layer, node));
      setError(null);
    } catch (reason) { setError(reason instanceof Error ? reason.message : "Unable to expand node"); }
  };

  const changeGraphMode = (next: GraphMode) => {
    setGraphMode(next);
    storeGraphMode(next);
    setPhysicsError(null);
  };

  const restoreAncestor = (breadcrumbIndex: number) => {
    if (navigation === "loading-in" || navigation === "refreshing" || breadcrumbIndex < 0 || breadcrumbIndex >= breadcrumbs.length - 1) return;
    const snapshot = historyRef.current[breadcrumbIndex];
    if (!snapshot) return;
    projectionGeneration.current += 1;
    historyRef.current = historyRef.current.slice(0, breadcrumbIndex);
    setData(snapshot.data);
    setEnabledKinds(new Set(snapshot.enabledKinds));
    setEnabledEdges(new Set(snapshot.enabledEdges));
    setSearch(snapshot.search);
    setBreadcrumbs((before) => before.slice(0, breadcrumbIndex + 1));
    setError(null);
    resetSelection();
    setPhase("ready");
    beginTransition("enter-out");
  };

  useEffect(() => {
    if (!selectedId || !data || selectedNode?.id === selectedId) return;
    const node = data.nodes.find((item) => item.id === selectedId);
    if (node) void selectNode(node);
  }, [selectedId, data]);

  if (!data && phase === "loading") return <div className="center-state"><span className="spinner" /><strong>Loading bounded projection</strong><small>Aggregating graph data on the server</small></div>;
  if (!data && phase === "error") return <div className="center-state error-state"><strong>Overview unavailable</strong><span>{error}</span><button onClick={() => void loadProjection({ scope: "repository", intent: "initial" })}>Retry</button></div>;
  if (!data || data.nodes.length === 0) return <div className="center-state"><strong>No graph data</strong><small>Index a repository, then refresh this view.</small></div>;

  const currentCrumb = breadcrumbs.at(-1)!;
  const parentCrumb = breadcrumbs.at(-2);
  const navigationLoading = navigation === "loading-in" || navigation === "refreshing";
  const projectionKey = `${currentCrumb.scope}:${currentCrumb.id ?? "root"}`;

  return <div className={`overview-shell${navigationLoading ? " is-navigation-loading" : ""}${navigation === "enter-in" ? " is-enter-in" : ""}${navigation === "enter-out" ? " is-enter-out" : ""}`}>
    <aside className="filter-rail" style={{ width: leftWidth }}>
      <div className="rail-section rail-heading"><span>Projection</span><div className="projection-actions">{parentCrumb && <button className="projection-back" disabled={navigationLoading} onClick={() => restoreAncestor(breadcrumbs.length - 2)} aria-label={`Back to ${parentCrumb.label}`} title={`Back to ${parentCrumb.label}`}><ArrowLeft size={12} />Back</button>}<button className="icon-button" disabled={navigationLoading} onClick={() => void loadProjection({ scope: currentCrumb.scope, parentId: currentCrumb.id, label: currentCrumb.label, intent: "refresh" })} title="Refresh"><RefreshCw size={14} /></button></div></div>
      <nav className="projection-breadcrumbs">{breadcrumbs.map((crumb, index) => <button key={`${crumb.scope}:${crumb.id ?? "root"}`} aria-current={index === breadcrumbs.length - 1 ? "page" : undefined} disabled={navigationLoading || index === breadcrumbs.length - 1} onClick={() => restoreAncestor(index)}>{index > 0 && <ChevronRight size={11} />}{crumb.label}</button>)}</nav>
      <div className="projection-meta"><b>{data.nodes.length.toLocaleString()}</b> of {data.total_nodes.toLocaleString()} groups/nodes<br/><b>{data.edges.length.toLocaleString()}</b> of {data.total_edges.toLocaleString()} relationships{data.truncated && <em>bounded view</em>}</div>
      <div className="rail-section"><div className="rail-label"><span>Node types</span><button onClick={() => setEnabledKinds(new Set(counts.kinds.map(([kind]) => kind)))}>All</button><button onClick={() => setEnabledKinds(new Set())}>None</button></div><div className="filter-chips">{counts.kinds.map(([kind, count]) => <button key={kind} className={enabledKinds.has(kind) ? "is-active" : ""} onClick={() => setEnabledKinds((before) => toggle(before, kind))}><i style={{ background: kindColor(kind) }} />{kind}<span>{count.toLocaleString()}</span></button>)}</div></div>
      <div className="rail-section"><div className="rail-label"><span>Relationships</span></div><div className="filter-chips edge-chips">{counts.edges.map(([kind, count]) => <button key={kind} className={enabledEdges.has(kind) ? "is-active" : ""} onClick={() => setEnabledEdges((before) => toggle(before, kind))}>{enabledEdges.has(kind) && <Check size={10} />}{kind.replaceAll("_", " ").toLowerCase()}<span>{count.toLocaleString()}</span></button>)}</div></div>
      <div className="rail-search"><Search size={14} /><input value={search} onChange={(event) => setSearch(event.target.value)} placeholder="Find node or group…" />{search && <button onClick={() => setSearch("")}><X size={13}/></button>}</div>
      <div className="tree-list">{matches.map((node) => <button key={node.id} onClick={() => void selectNode(node)} onDoubleClick={() => void exploreNode(node)}><i style={{ background: kindColor(node.kind) }} /><span><b>{node.name}</b><small>{node.role ?? node.kind}</small></span><em>{(node.member_count ?? node.degree).toLocaleString()}</em></button>)}</div>
      {selected && <button className="clear-selection" onClick={() => { setSelected(null); setSelectedNode(null); setContext(null); onSelectedId(null); }}>Clear selection</button>}
    </aside>
    <ResizeHandle side="left" onDelta={(delta) => setLeftWidth((width) => { const next = Math.max(210, Math.min(480, width + delta)); storeWidth("cih-left-width", next); return next; })} />
    <main className="graph-workspace">
      <GraphCanvas nodes={filteredNodes} edges={filteredEdges} selected={selected} showLabels={showLabels} resetNonce={resetNonce} mode={graphMode} projectionKey={projectionKey} onSelect={(node) => void selectNode(node)} onExplore={(node) => void exploreNode(node)} onPhysicsError={setPhysicsError} />
      {(error || physicsError) && <div className="graph-error-toast">{error || physicsError}<button onClick={() => { setError(null); setPhysicsError(null); }} aria-label="Dismiss error"><X size={12}/></button></div>}
      <div className="canvas-hud"><span>{filteredNodes.length.toLocaleString()} nodes</span><span>{filteredEdges.length.toLocaleString()} relationships</span>{selected && <span className="is-accent">{selected.size.toLocaleString()} focused</span>}</div>
      <div className="hud-controls">
        <div className="graph-mode-toggle" role="group" aria-label="Graph rendering mode">
          <button aria-pressed={graphMode === "performance"} className={graphMode === "performance" ? "is-on" : ""} onClick={() => changeGraphMode("performance")} title="Static, maximum-throughput graph layout"><Gauge size={12} />Performance</button>
          <button aria-pressed={graphMode === "fancy"} className={graphMode === "fancy" ? "is-on" : ""} onClick={() => changeGraphMode("fancy")} title="Interactive graph physics"><Sparkles size={12} />Fancy</button>
        </div>
        <Legend /><button className="hud-button" onClick={() => setResetNonce((value) => value + 1)} title="Fit graph"><Maximize size={13} /></button><button className={showLabels ? "hud-button is-on" : "hud-button"} aria-pressed={showLabels} onClick={() => setShowLabels((value) => !value)} title="Toggle labels"><Tag size={13} /></button>
      </div>
    </main>
    {selectedNode && <><ResizeHandle side="right" onDelta={(delta) => setRightWidth((width) => { const next = Math.max(250, Math.min(520, width + delta)); storeWidth("cih-right-width", next); return next; })} /><div style={{ width: rightWidth }} className="inspector-wrap"><Inspector node={selectedNode} context={context} loading={contextLoading} onClose={() => { setSelected(null); setSelectedNode(null); setContext(null); onSelectedId(null); }} onExplore={() => void exploreNode(selectedNode)} /></div></>}
    {navigationLoading && <div className="projection-loading-overlay" role="status" aria-live="polite"><div><span className="spinner" /><span>{navigation === "loading-in" ? `Opening ${pendingLabel}…` : `Refreshing ${pendingLabel}…`}</span></div></div>}
  </div>;
}

function toggle(before: Set<string>, value: string): Set<string> { const next = new Set(before); next.has(value) ? next.delete(value) : next.add(value); return next; }
function intersection(before: Set<string>, available: Set<string>): Set<string> { return new Set([...before].filter((value) => available.has(value))); }
function storedWidth(key: string, fallback: number): number { try { return Number(window.localStorage?.getItem(key)) || fallback; } catch { return fallback; } }
function storeWidth(key: string, value: number): void { try { window.localStorage?.setItem(key, String(value)); } catch { /* optional */ } }
function storedGraphMode(): GraphMode { try { return window.localStorage?.getItem(GRAPH_MODE_KEY) === "fancy" ? "fancy" : "performance"; } catch { return "performance"; } }
function storeGraphMode(value: GraphMode): void { try { window.localStorage?.setItem(GRAPH_MODE_KEY, value); } catch { /* optional */ } }
