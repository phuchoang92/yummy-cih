/// Incremental, cancellable force refinement. Server coordinates are always a
/// usable first frame; this worker only improves local separation and links.
import { forceCollide, forceLink, forceManyBody, forceSimulation, forceX, forceY, type Simulation } from "d3-force";
import { randomLcg } from "d3-random";

interface LayoutNode {
  index: number;
  x: number;
  y: number;
  size: number;
  pinned?: boolean;
  fx?: number | null;
  fy?: number | null;
}

interface LayoutEdge { source: number; target: number; count?: number }

interface LayoutRequest {
  generation: number;
  nodes: LayoutNode[];
  edges: LayoutEdge[];
}

interface ActiveLayout {
  generation: number;
  nodes: LayoutNode[];
  simulation: Simulation<LayoutNode, LayoutEdge>;
  started: number;
  lastPosted: number;
  ticks: number;
}

let active: ActiveLayout | null = null;
let scheduled = false;

self.onmessage = (event: MessageEvent<LayoutRequest>) => {
  const { generation, nodes, edges } = event.data;
  const started = performance.now();
  const links = edges.map((edge) => ({ ...edge }));
  for (const node of nodes) {
    if (node.pinned) {
      node.fx = node.x;
      node.fy = node.y;
    }
  }
  const simulation = forceSimulation(nodes)
    .randomSource(randomLcg(0.41721))
    .alpha(0.7)
    .alphaDecay(0.055)
    .velocityDecay(0.34)
    .force("charge", forceManyBody<LayoutNode>().strength((node) => -18 - Math.min(80, node.size * 4)))
    .force("link", forceLink<LayoutNode, LayoutEdge>(links).id((node) => node.index).distance(42).strength(0.13))
    .force("collide", forceCollide<LayoutNode>().radius((node) => node.size + 4).strength(0.8))
    .force("x", forceX<LayoutNode>(0).strength(0.012))
    .force("y", forceY<LayoutNode>(0).strength(0.012))
    .stop();

  active = { generation, nodes, simulation, started, lastPosted: started, ticks: 0 };
  scheduleStep();
};

function scheduleStep() {
  if (scheduled) return;
  scheduled = true;
  setTimeout(step, 0);
}

function step() {
  scheduled = false;
  const layout = active;
  if (!layout) return;
  const sliceStarted = performance.now();
  while (active === layout && performance.now() - sliceStarted < 8) {
    layout.simulation.tick();
    layout.ticks += 1;
    if (layout.ticks >= 320) break;
  }
  if (active !== layout) { scheduleStep(); return; }
  const now = performance.now();
  const elapsed = now - layout.started;
  // 300 ms is the quality target. The absolute 500 ms deadline wins even on
  // slow machines; a new request replaces `active` between these short slices.
  const done = elapsed >= 500 || layout.ticks >= 320 || (elapsed >= 300 && layout.simulation.alpha() <= 0.08);
  if (done || now - layout.lastPosted >= 50) {
    self.postMessage({
      generation: layout.generation,
      done,
      elapsed_ms: elapsed,
      positions: layout.nodes.map((node) => [node.x ?? 0, node.y ?? 0]),
    });
    layout.lastPosted = now;
  }
  if (done) active = null;
  else scheduleStep();
}

export {};
