import {
  forceCollide,
  forceLink,
  forceManyBody,
  forceSimulation,
  forceX,
  forceY,
  type Force,
  type Simulation,
  type SimulationLinkDatum,
  type SimulationNodeDatum,
} from "d3-force";
import { randomLcg } from "d3-random";
import type { LayoutCommand, LayoutFrame } from "./layout-protocol";
import type { GraphMode } from "./types";

interface SimulationNode extends SimulationNodeDatum {
  graphIndex: number;
  x: number;
  y: number;
  size: number;
  pinned?: boolean;
}

interface SimulationEdge extends SimulationLinkDatum<SimulationNode> {
  count?: number;
}

interface PointerField {
  x: number;
  y: number;
  radius: number;
}

interface InteractiveForce extends Force<SimulationNode, SimulationEdge> {
  setPointer: (pointer: PointerField | null) => void;
}

interface ActiveLayout {
  generation: number;
  mode: GraphMode;
  nodes: SimulationNode[];
  nodesByIndex: Map<number, SimulationNode>;
  simulation: Simulation<SimulationNode, SimulationEdge>;
  pointerForce: InteractiveForce;
  started: number;
  burstStarted: number;
  lastPosted: number;
  ticks: number;
  phase: "initial" | "interactive";
  running: boolean;
  draggingIndex: number | null;
  pointerExpires: number;
}

const FRAME_INTERVAL_MS = 50;
const INITIAL_BUDGET_MS = 500;
const INTERACTION_BUDGET_MS = 500;
const POINTER_IDLE_MS = 120;

function createPointerForce(): InteractiveForce {
  let nodes: SimulationNode[] = [];
  let pointer: PointerField | null = null;

  const force = ((alpha: number) => {
    if (!pointer || pointer.radius <= 0) return;
    const radiusSquared = pointer.radius * pointer.radius;
    for (const node of nodes) {
      if (node.fx != null || node.fy != null) continue;
      const dx = (node.x ?? 0) - pointer.x;
      const dy = (node.y ?? 0) - pointer.y;
      const distanceSquared = dx * dx + dy * dy;
      if (distanceSquared <= 0.0001 || distanceSquared >= radiusSquared) continue;
      const distance = Math.sqrt(distanceSquared);
      const strength = (1 - distance / pointer.radius) * alpha * 2.4;
      node.vx = (node.vx ?? 0) + (dx / distance) * strength;
      node.vy = (node.vy ?? 0) + (dy / distance) * strength;
    }
  }) as InteractiveForce;

  force.initialize = (next) => { nodes = next; };
  force.setPointer = (next) => { pointer = next; };
  return force;
}

export class LayoutController {
  private active: ActiveLayout | null = null;

  get running(): boolean { return this.active?.running ?? false; }
  get generation(): number | null { return this.active?.generation ?? null; }

  handle(command: LayoutCommand, now: number): boolean {
    if (command.type === "init") {
      this.initialize(command, now);
      return true;
    }
    const layout = this.active;
    if (!layout || command.generation !== layout.generation || layout.mode !== "fancy") return false;

    switch (command.type) {
      case "drag-start": {
        const node = layout.nodesByIndex.get(command.index);
        if (!node) return false;
        node.fx = command.x;
        node.fy = command.y;
        node.x = command.x;
        node.y = command.y;
        layout.draggingIndex = command.index;
        layout.simulation.alphaTarget(0.12);
        this.reheat(layout, now, 0.28);
        return true;
      }
      case "drag-move": {
        if (layout.draggingIndex !== command.index) return false;
        const node = layout.nodesByIndex.get(command.index);
        if (!node) return false;
        node.fx = command.x;
        node.fy = command.y;
        node.x = command.x;
        node.y = command.y;
        this.reheat(layout, now, 0.22);
        return true;
      }
      case "drag-end": {
        if (layout.draggingIndex !== command.index) return false;
        const node = layout.nodesByIndex.get(command.index);
        if (!node) return false;
        if (!command.pin) {
          node.fx = null;
          node.fy = null;
        }
        layout.draggingIndex = null;
        layout.simulation.alphaTarget(layout.pointerExpires > now ? 0.08 : 0);
        this.reheat(layout, now, 0.2);
        return true;
      }
      case "pointer-field": {
        layout.pointerForce.setPointer({ x: command.x, y: command.y, radius: command.radius });
        layout.pointerExpires = now + POINTER_IDLE_MS;
        if (layout.draggingIndex == null) layout.simulation.alphaTarget(0.08);
        this.reheat(layout, now, 0.16);
        return true;
      }
      case "pointer-clear": {
        layout.pointerForce.setPointer(null);
        layout.pointerExpires = 0;
        layout.simulation.alphaTarget(layout.draggingIndex == null ? 0 : 0.12);
        this.reheat(layout, now, 0.12);
        return true;
      }
    }
  }

  tick(now: number): boolean {
    const layout = this.active;
    if (!layout || !layout.running) return false;
    if (layout.pointerExpires > 0 && now >= layout.pointerExpires) {
      layout.pointerExpires = 0;
      layout.pointerForce.setPointer(null);
      if (layout.draggingIndex == null) layout.simulation.alphaTarget(0);
    }
    layout.simulation.tick();
    layout.ticks += 1;

    const elapsed = now - layout.burstStarted;
    const done = layout.phase === "initial"
      ? elapsed >= INITIAL_BUDGET_MS || layout.ticks >= 320 || (elapsed >= 300 && layout.simulation.alpha() <= 0.08)
      : layout.draggingIndex == null && layout.pointerExpires === 0
        && (elapsed >= INTERACTION_BUDGET_MS || layout.simulation.alpha() <= 0.03);
    if (done) {
      layout.running = false;
      layout.simulation.alphaTarget(0).stop();
    }
    return layout.running;
  }

  takeFrame(now: number, force = false): LayoutFrame | null {
    const layout = this.active;
    if (!layout) return null;
    if (!force && layout.running && now - layout.lastPosted < FRAME_INTERVAL_MS) return null;
    if (!force && !layout.running && layout.lastPosted >= now) return null;
    layout.lastPosted = now;
    return {
      type: "frame",
      generation: layout.generation,
      done: !layout.running,
      elapsed_ms: now - layout.started,
      positions: layout.nodes.map((node) => [node.x ?? 0, node.y ?? 0]),
    };
  }

  isPinned(index: number): boolean {
    const node = this.active?.nodesByIndex.get(index);
    return node?.fx != null && node?.fy != null && this.active?.draggingIndex !== index;
  }

  private initialize(command: Extract<LayoutCommand, { type: "init" }>, now: number) {
    const nodes: SimulationNode[] = command.nodes.map(({ index, ...node }) => ({ ...node, graphIndex: index }));
    const links: SimulationEdge[] = command.edges.map((edge) => ({ ...edge }));
    for (const node of nodes) {
      if (node.pinned) {
        node.fx = node.x;
        node.fy = node.y;
      }
    }
    const pointerForce = createPointerForce();
    const simulation = forceSimulation(nodes)
      .randomSource(randomLcg(0.41721))
      .alpha(0.7)
      .alphaDecay(0.055)
      .velocityDecay(0.34)
      .force("charge", forceManyBody<SimulationNode>().strength((node) => -18 - Math.min(80, node.size * 4)))
      .force("link", forceLink<SimulationNode, SimulationEdge>(links).id((node) => node.graphIndex).distance(42).strength(0.13))
      .force("collide", forceCollide<SimulationNode>().radius((node) => node.size + 4).strength(0.8))
      .force("x", forceX<SimulationNode>(0).strength(0.012))
      .force("y", forceY<SimulationNode>(0).strength(0.012))
      .force("pointer", pointerForce)
      .stop();

    this.active = {
      generation: command.generation,
      mode: command.mode,
      nodes,
      nodesByIndex: new Map(nodes.map((node) => [node.graphIndex, node])),
      simulation,
      pointerForce,
      started: now,
      burstStarted: now,
      lastPosted: now,
      ticks: 0,
      phase: "initial",
      running: true,
      draggingIndex: null,
      pointerExpires: 0,
    };
  }

  private reheat(layout: ActiveLayout, now: number, alpha: number) {
    layout.phase = "interactive";
    layout.burstStarted = now;
    layout.running = true;
    layout.simulation.alpha(Math.max(layout.simulation.alpha(), alpha));
  }
}
