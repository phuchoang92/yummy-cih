import { Palette, X } from "lucide-react";
import { useState } from "react";
import { EDGE_COLORS, edgeLabel, KIND_COLORS } from "./colors";

// Kinds worth surfacing in the legend, in a sensible reading order. "Node" is a
// generic fallback, not a real kind, so it is intentionally omitted.
const LEGEND_KINDS = [
  "Route", "Process", "Community", "IntegrationRoute", "Class", "Interface",
  "Method", "Function", "File", "Folder", "DbTable", "ExternalEndpoint",
].filter((kind) => kind in KIND_COLORS);

/** Collapsible legend that decodes the graph's color systems. Collapsed by
 *  default so it never obscures the scene or collides with rail labels. */
export function Legend() {
  const [open, setOpen] = useState(false);
  if (!open) {
    return (
      <button className="hud-button" aria-label="Show legend" onClick={() => setOpen(true)}>
        <Palette size={13} /> Legend
      </button>
    );
  }

  return (
    <div className="legend-panel" role="dialog" aria-label="Legend">
      <div className="legend-head">
        <span>Legend</span>
        <button className="icon-button" aria-label="Hide legend" onClick={() => setOpen(false)}><X size={14} /></button>
      </div>

      <section className="legend-section">
        <h4>Node size</h4>
        <small>Size reflects aggregate members or graph degree; color identifies node kind.</small>
      </section>

      <section className="legend-section">
        <h4>Node kinds</h4>
        <div className="legend-swatches">
          {LEGEND_KINDS.map((kind) => (
            <span key={kind} className="legend-swatch"><i style={{ background: KIND_COLORS[kind] }} />{kind}</span>
          ))}
        </div>
      </section>

      <section className="legend-section">
        <h4>Relationships</h4>
        <div className="legend-swatches">
          {Object.entries(EDGE_COLORS).map(([kind, color]) => (
            <span key={kind} className="legend-swatch"><i style={{ background: color }} />{edgeLabel(kind)}</span>
          ))}
        </div>
      </section>
    </div>
  );
}
