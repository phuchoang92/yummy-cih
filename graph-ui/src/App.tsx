import { Activity, CircleDot, X } from "lucide-react";
import { useEffect, useState } from "react";
import { api } from "./api";
import { ClassicViews } from "./ClassicViews";
import { Overview } from "./Overview";
import type { SymbolContext, TabId } from "./types";

const TABS: { id: TabId; label: string }[] = [
  { id: "overview", label: "Overview" }, { id: "search", label: "Search" },
  { id: "impact", label: "Impact" }, { id: "flow", label: "Flow" },
  { id: "communities", label: "Communities" }, { id: "routes", label: "Routes" },
];

function CompactInspector({ context, onClose }: { context: SymbolContext; onClose: () => void }) {
  return <aside className="compact-inspector"><button className="icon-button" onClick={onClose}><X size={15}/></button><small>{context.node.kind}</small><h2>{context.node.name}</h2><p>{context.node.qualified_name || context.node.id}</p>{context.node.file && <p>{context.node.file}</p>}<div className="metric-row"><span><b>{context.callees.length}</b> outbound</span><span><b>{context.callers.length}</b> inbound</span></div>{context.community && <div className="compact-group"><small>Community</small><strong>{context.community.name}</strong></div>}</aside>;
}

export function App() {
  const [active, setActive] = useState<TabId>("overview");
  const [selectedId, setSelectedId] = useState<string | null>(null);
  const [context, setContext] = useState<SymbolContext | null>(null);
  useEffect(() => {
    if (!selectedId || active === "overview") { setContext(null); return; }
    let current = true;
    api.context(selectedId).then((value) => { if (current) setContext(value); }).catch(() => { if (current) setContext(null); });
    return () => { current = false; };
  }, [selectedId, active]);

  return <div className="app-shell">
    <header className="app-header">
      <div className="brand"><span className="brand-orbit"><CircleDot size={14}/></span><div><b>CIH</b><span>Graph Explorer</span></div></div>
      <nav>{TABS.map((tab) => <button key={tab.id} className={active === tab.id ? "is-active" : ""} onClick={() => setActive(tab.id)}>{tab.label}</button>)}</nav>
      <div className="header-status"><Activity size={13}/><span>read-only</span>{selectedId && <button onClick={() => setSelectedId(null)}><small>selected</small>{selectedId.split("#").pop()?.split(":").pop()}<X size={12}/></button>}</div>
    </header>
    <main className="app-content">
      {active === "overview" ? <Overview selectedId={selectedId} onSelectedId={setSelectedId}/> : <ClassicViews tab={active} selectedId={selectedId} onSelectedId={setSelectedId}/>} 
      {active !== "overview" && context && <CompactInspector context={context} onClose={() => setSelectedId(null)}/>} 
    </main>
  </div>;
}
