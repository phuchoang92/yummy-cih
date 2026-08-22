import { quadtree, type Quadtree } from "d3-quadtree";
import { select } from "d3-selection";
import { zoom, zoomIdentity, type ZoomBehavior, type ZoomTransform } from "d3-zoom";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { edgeColor, kindColor } from "./colors";
import type { OverviewEdge, OverviewNode } from "./types";
import LayoutWorker from "./layout.worker?worker&inline";

interface PositionedNode extends OverviewNode { px: number; py: number }

export function cameraTarget(): null { return null; }
export function hasWebGl(): boolean { return true; }

export function GraphCanvas({
  nodes,
  edges,
  selected,
  showLabels,
  resetNonce,
  onSelect,
  onExplore,
}: {
  nodes: OverviewNode[];
  edges: OverviewEdge[];
  selected: Set<number> | null;
  showLabels: boolean;
  resetNonce: number;
  onSelect: (node: OverviewNode) => void;
  onExplore?: (node: OverviewNode) => void;
  autoRotate?: boolean;
  target?: null;
}) {
  const hostRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const workerRef = useRef<Worker | null>(null);
  const generationRef = useRef(0);
  const transformRef = useRef<ZoomTransform>(zoomIdentity);
  const zoomRef = useRef<ZoomBehavior<HTMLCanvasElement, unknown> | null>(null);
  const positionedRef = useRef<PositionedNode[]>([]);
  const treeRef = useRef<Quadtree<PositionedNode> | null>(null);
  const frameRef = useRef<number | null>(null);
  const movingRef = useRef(false);
  const [hovered, setHovered] = useState<{ node: OverviewNode; x: number; y: number } | null>(null);

  const edgeNodes = useMemo(() => new Map(nodes.map((node) => [node.index, node])), [nodes]);

  const draw = useCallback(() => {
    frameRef.current = null;
    const canvas = canvasRef.current;
    const host = hostRef.current;
    if (!canvas || !host) return;
    const width = Math.max(1, host.clientWidth);
    const height = Math.max(1, host.clientHeight);
    const dpr = Math.min(window.devicePixelRatio || 1, 1.5);
    const pixelWidth = Math.round(width * dpr);
    const pixelHeight = Math.round(height * dpr);
    if (canvas.width !== pixelWidth || canvas.height !== pixelHeight) {
      canvas.width = pixelWidth;
      canvas.height = pixelHeight;
      canvas.style.width = `${width}px`;
      canvas.style.height = `${height}px`;
    }
    const context = canvas.getContext("2d");
    if (!context) return;
    context.setTransform(dpr, 0, 0, dpr, 0, 0);
    context.fillStyle = "#080c12";
    context.fillRect(0, 0, width, height);
    const transform = transformRef.current;
    context.save();
    context.translate(transform.x, transform.y);
    context.scale(transform.k, transform.k);
    const byIndex = new Map(positionedRef.current.map((node) => [node.index, node]));
    const active = !!selected?.size;

    if (!movingRef.current || nodes.length <= 5_000) {
      context.lineWidth = Math.max(0.35, 1 / transform.k);
      for (const edge of edges) {
        const source = byIndex.get(edge.source);
        const target = byIndex.get(edge.target);
        if (!source || !target) continue;
        const connected = !active || selected?.has(source.index) || selected?.has(target.index);
        if (!connected) continue;
        context.globalAlpha = active ? 0.38 : Math.min(0.34, 0.08 + Math.log1p(edge.count ?? 1) * 0.04);
        context.strokeStyle = edgeColor(edge.kind);
        context.beginPath();
        context.moveTo(source.px, source.py);
        context.lineTo(target.px, target.py);
        context.stroke();
      }
    }

    context.globalAlpha = 1;
    for (const node of positionedRef.current) {
      const isSelected = !!selected?.has(node.index);
      context.globalAlpha = active && !isSelected ? 0.13 : 0.95;
      context.fillStyle = kindColor(node.kind);
      const radius = Math.max(2.2, Math.min(15, node.size * (isSelected ? 1.35 : 1)));
      context.beginPath();
      context.arc(node.px, node.py, radius / Math.sqrt(transform.k), 0, Math.PI * 2);
      context.fill();
      if (node.role === "boundary") {
        context.strokeStyle = "#f5b942";
        context.lineWidth = 1.5 / transform.k;
        context.stroke();
      }
    }

    if (showLabels) {
      const labels = [...positionedRef.current]
        .sort((a, b) => b.degree - a.degree || a.id.localeCompare(b.id))
        .slice(0, 150);
      if (selected) {
        for (const node of positionedRef.current) {
          if (selected.has(node.index) && !labels.includes(node)) labels.push(node);
        }
      }
      context.font = `${11 / transform.k}px ui-sans-serif, system-ui`;
      context.fillStyle = "#cbd5e1";
      context.globalAlpha = 0.82;
      for (const node of labels) context.fillText(node.name || node.id, node.px + 6 / transform.k, node.py - 6 / transform.k);
    }
    context.restore();
    context.globalAlpha = 1;
  }, [edges, nodes.length, selected, showLabels]);

  const requestDraw = useCallback(() => {
    if (frameRef.current == null) frameRef.current = requestAnimationFrame(draw);
  }, [draw]);

  const fit = useCallback(() => {
    const host = hostRef.current;
    const canvas = canvasRef.current;
    const behavior = zoomRef.current;
    if (!host || !canvas || !behavior || positionedRef.current.length === 0) return;
    const xs = positionedRef.current.map((node) => node.px);
    const ys = positionedRef.current.map((node) => node.py);
    const minX = Math.min(...xs); const maxX = Math.max(...xs);
    const minY = Math.min(...ys); const maxY = Math.max(...ys);
    const scale = Math.max(0.05, Math.min(4, 0.84 * Math.min(host.clientWidth / Math.max(80, maxX - minX), host.clientHeight / Math.max(80, maxY - minY))));
    const next = zoomIdentity.translate(host.clientWidth / 2, host.clientHeight / 2).scale(scale).translate(-(minX + maxX) / 2, -(minY + maxY) / 2);
    select(canvas).call(behavior.transform, next);
  }, []);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const behavior = zoom<HTMLCanvasElement, unknown>()
      .scaleExtent([0.03, 12])
      .on("zoom", (event) => {
        transformRef.current = event.transform;
        movingRef.current = true;
        requestDraw();
      })
      .on("end", () => {
        movingRef.current = false;
        requestDraw();
      });
    zoomRef.current = behavior;
    select(canvas).call(behavior).on("dblclick.zoom", null);
    return () => { select(canvas).on(".zoom", null); };
  }, [requestDraw]);

  useEffect(() => {
    const previous = new Map(positionedRef.current.map((node) => [node.id, node]));
    const hadExisting = nodes.some((node) => previous.has(node.id));
    positionedRef.current = nodes.map((node) => {
      const existing = previous.get(node.id);
      return { ...node, px: existing?.px ?? node.x, py: existing?.py ?? node.y };
    });
    treeRef.current = quadtree<PositionedNode>().x((node) => node.px).y((node) => node.py).addAll(positionedRef.current);
    generationRef.current += 1;
    const generation = generationRef.current;
    if (!workerRef.current) {
      const worker = new LayoutWorker();
      workerRef.current = worker;
      worker.onmessage = (event: MessageEvent<{ generation: number; done: boolean; positions: [number, number][] }>) => {
        if (event.data.generation !== generationRef.current) return;
        event.data.positions.forEach(([x, y], index) => {
          const node = positionedRef.current[index];
          if (node) { node.px = x; node.py = y; }
        });
        movingRef.current = !event.data.done;
        treeRef.current = quadtree<PositionedNode>().x((node) => node.px).y((node) => node.py).addAll(positionedRef.current);
        requestDraw();
      };
    }
    movingRef.current = true;
    workerRef.current?.postMessage({
      generation,
      nodes: positionedRef.current.map((node) => ({ index: node.index, x: node.px, y: node.py, size: node.size, pinned: node.pinned || previous.has(node.id) })),
      edges,
    });
    requestDraw();
    if (!hadExisting) queueMicrotask(fit);
  }, [nodes, edges, fit, requestDraw]);

  useEffect(() => () => {
    workerRef.current?.terminate();
    if (frameRef.current != null) cancelAnimationFrame(frameRef.current);
  }, []);

  useEffect(() => {
    const host = hostRef.current;
    if (!host) return;
    const observer = typeof ResizeObserver === "undefined" ? null : new ResizeObserver(requestDraw);
    observer?.observe(host);
    window.addEventListener("resize", requestDraw);
    return () => { observer?.disconnect(); window.removeEventListener("resize", requestDraw); };
  }, [requestDraw]);

  useEffect(() => { if (resetNonce > 0) fit(); }, [fit, resetNonce]);
  useEffect(requestDraw, [requestDraw, selected, showLabels]);

  const pick = (event: React.PointerEvent<HTMLCanvasElement> | React.MouseEvent<HTMLCanvasElement>) => {
    const rect = event.currentTarget.getBoundingClientRect();
    const [x, y] = transformRef.current.invert([event.clientX - rect.left, event.clientY - rect.top]);
    return treeRef.current?.find(x, y, 14 / transformRef.current.k) ?? null;
  };

  return <div className="graph-canvas" ref={hostRef}>
    <canvas
      ref={canvasRef}
      aria-label="Code graph canvas"
      onPointerMove={(event) => {
        const node = pick(event);
        setHovered(node ? { node, x: event.nativeEvent.offsetX, y: event.nativeEvent.offsetY } : null);
      }}
      onPointerLeave={() => setHovered(null)}
      onClick={(event) => { const node = pick(event); if (node) onSelect(edgeNodes.get(node.index) ?? node); }}
      onDoubleClick={(event) => { const node = pick(event); if (node) onExplore?.(edgeNodes.get(node.index) ?? node); }}
    />
    {hovered && <div className="node-tooltip" style={{ left: hovered.x + 12, top: hovered.y + 12 }}>
      <span style={{ background: kindColor(hovered.node.kind) }} />
      <strong>{hovered.node.name}</strong>
      <small>{hovered.node.kind} · {(hovered.node.member_count ?? hovered.node.degree).toLocaleString()}</small>
    </div>}
  </div>;
}
