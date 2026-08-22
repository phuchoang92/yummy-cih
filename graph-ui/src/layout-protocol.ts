import type { GraphMode } from "./types";

export interface LayoutNodeInput {
  index: number;
  x: number;
  y: number;
  size: number;
  pinned?: boolean;
  fx?: number | null;
  fy?: number | null;
  vx?: number;
  vy?: number;
}

export interface LayoutEdgeInput {
  source: number;
  target: number;
  count?: number;
}

export type LayoutCommand =
  | {
    type: "init";
    generation: number;
    mode: GraphMode;
    nodes: LayoutNodeInput[];
    edges: LayoutEdgeInput[];
  }
  | { type: "drag-start"; generation: number; index: number; x: number; y: number }
  | { type: "drag-move"; generation: number; index: number; x: number; y: number }
  | { type: "drag-end"; generation: number; index: number; pin: boolean }
  | { type: "pointer-field"; generation: number; x: number; y: number; radius: number }
  | { type: "pointer-clear"; generation: number };

export interface LayoutFrame {
  type: "frame";
  generation: number;
  done: boolean;
  elapsed_ms: number;
  positions: [number, number][];
}
