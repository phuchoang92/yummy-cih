import { quadtree, type Quadtree } from "d3-quadtree";
import { select } from "d3-selection";
import { zoom, zoomIdentity, type ZoomBehavior, type ZoomTransform } from "d3-zoom";
import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { edgeColor, kindColor } from "./colors";
import type { LayoutCommand, LayoutFrame } from "./layout-protocol";
import type { GraphMode, OverviewEdge, OverviewNode } from "./types";
import LayoutWorker from "./layout.worker?worker&inline";

interface PositionedNode extends OverviewNode { px: number; py: number }
interface DragState {
  pointerId: number;
  node: PositionedNode;
  startClientX: number;
  startClientY: number;
  moved: boolean;
  wasPinned: boolean;
}

export function cameraTarget(): null { return null; }
export function hasWebGl(): boolean { return true; }

export function GraphCanvas({
  nodes,
  edges,
  selected,
  showLabels,
  resetNonce,
  mode,
  projectionKey,
  onSelect,
  onExplore,
  onPhysicsError,
}: {
  nodes: OverviewNode[];
  edges: OverviewEdge[];
  selected: Set<number> | null;
  showLabels: boolean;
  resetNonce: number;
  mode: GraphMode;
  projectionKey: string;
  onSelect: (node: OverviewNode) => void;
  onExplore?: (node: OverviewNode) => void;
  onPhysicsError?: (message: string | null) => void;
}) {
  const hostRef = useRef<HTMLDivElement>(null);
  const canvasRef = useRef<HTMLCanvasElement>(null);
  const workerRef = useRef<Worker | null>(null);
  const generationRef = useRef(0);
  const projectionKeyRef = useRef(projectionKey);
  const transformRef = useRef<ZoomTransform>(zoomIdentity);
  const zoomRef = useRef<ZoomBehavior<HTMLCanvasElement, unknown> | null>(null);
  const positionedRef = useRef<PositionedNode[]>([]);
  const treeRef = useRef<Quadtree<PositionedNode> | null>(null);
  const frameRef = useRef<number | null>(null);
  const drawRef = useRef<() => void>(() => {});
  const movingRef = useRef(false);
  const modeRef = useRef(mode);
  const reducedMotionRef = useRef(false);
  const dragRef = useRef<DragState | null>(null);
  const suppressClickRef = useRef(false);
  const manualPinsRef = useRef(new Map<string, { x: number; y: number }>());
  const hoveredIndexRef = useRef<number | null>(null);
  const pendingPointerRef = useRef<{ x: number; y: number; radius: number } | null>(null);
  const pointerFrameRef = useRef<number | null>(null);
  const physicsErrorRef = useRef(onPhysicsError);
  const [hovered, setHovered] = useState<{ node: OverviewNode; x: number; y: number } | null>(null);
  const [dragging, setDragging] = useState(false);
  const [pinRevision, setPinRevision] = useState(0);

  modeRef.current = mode;
  physicsErrorRef.current = onPhysicsError;

  const edgeNodes = useMemo(() => new Map(nodes.map((node) => [node.index, node])), [nodes]);

  const draw = useCallback(() => {
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
      const isHovered = hoveredIndexRef.current === node.index;
      const isPinned = manualPinsRef.current.has(node.id);
      const highlighted = mode === "fancy" && (isSelected || isHovered || isPinned);
      context.globalAlpha = active && !isSelected ? 0.13 : 0.95;
      context.fillStyle = kindColor(node.kind);
      const radius = Math.max(2.2, Math.min(15, node.size * (isSelected ? 1.35 : 1)));
      if (highlighted) {
        context.save();
        context.shadowColor = kindColor(node.kind);
        context.shadowBlur = 12 / Math.sqrt(transform.k);
        context.beginPath();
        context.arc(node.px, node.py, radius / Math.sqrt(transform.k), 0, Math.PI * 2);
        context.fill();
        context.restore();
      } else {
        context.beginPath();
        context.arc(node.px, node.py, radius / Math.sqrt(transform.k), 0, Math.PI * 2);
        context.fill();
      }
      if (node.role === "boundary" || isPinned) {
        context.strokeStyle = isPinned ? "#67e8f9" : "#f5b942";
        context.lineWidth = (isPinned ? 2 : 1.5) / transform.k;
        context.beginPath();
        context.arc(node.px, node.py, (radius + (isPinned ? 4 : 0)) / Math.sqrt(transform.k), 0, Math.PI * 2);
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
  }, [edges, mode, nodes.length, pinRevision, selected, showLabels]);

  drawRef.current = draw;
  const requestDraw = useCallback(() => {
    if (frameRef.current == null) {
      frameRef.current = requestAnimationFrame(() => {
        frameRef.current = null;
        drawRef.current();
      });
    }
  }, []);

  const pointAt = useCallback((clientX: number, clientY: number, canvas = canvasRef.current) => {
    if (!canvas) return null;
    const rect = canvas.getBoundingClientRect();
    return transformRef.current.invert([clientX - rect.left, clientY - rect.top]);
  }, []);

  const pickAt = useCallback((clientX: number, clientY: number, canvas = canvasRef.current) => {
    const point = pointAt(clientX, clientY, canvas);
    if (!point) return null;
    return treeRef.current?.find(point[0], point[1], 14 / transformRef.current.k) ?? null;
  }, [pointAt]);

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

  const postCommand = useCallback((command: LayoutCommand) => {
    workerRef.current?.postMessage(command);
  }, []);

  const clearPointerField = useCallback(() => {
    pendingPointerRef.current = null;
    if (pointerFrameRef.current != null) {
      cancelAnimationFrame(pointerFrameRef.current);
      pointerFrameRef.current = null;
    }
    if (modeRef.current === "fancy") {
      postCommand({ type: "pointer-clear", generation: generationRef.current });
    }
  }, [postCommand]);

  const queuePointerField = useCallback((x: number, y: number, radius: number) => {
    if (modeRef.current !== "fancy" || reducedMotionRef.current) return;
    pendingPointerRef.current = { x, y, radius };
    if (pointerFrameRef.current != null) return;
    pointerFrameRef.current = requestAnimationFrame(() => {
      pointerFrameRef.current = null;
      const pointer = pendingPointerRef.current;
      pendingPointerRef.current = null;
      if (!pointer || modeRef.current !== "fancy" || reducedMotionRef.current) return;
      postCommand({ type: "pointer-field", generation: generationRef.current, ...pointer });
    });
  }, [postCommand]);

  useEffect(() => {
    const media = window.matchMedia?.("(prefers-reduced-motion: reduce)");
    const update = () => {
      reducedMotionRef.current = media?.matches ?? false;
      if (reducedMotionRef.current) clearPointerField();
    };
    update();
    media?.addEventListener?.("change", update);
    return () => media?.removeEventListener?.("change", update);
  }, [clearPointerField]);

  useEffect(() => {
    const canvas = canvasRef.current;
    if (!canvas) return;
    const behavior = zoom<HTMLCanvasElement, unknown>()
      .filter((event) => {
        if (event.type === "wheel") return true;
        if (event.button) return false;
        if (dragRef.current) return false;
        return modeRef.current !== "fancy" || !("clientX" in event) || !pickAt(event.clientX, event.clientY, canvas);
      })
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
  }, [pickAt, requestDraw]);

  useEffect(() => {
    try {
      const worker = new LayoutWorker();
      workerRef.current = worker;
      physicsErrorRef.current?.(null);
      worker.onmessage = (event: MessageEvent<LayoutFrame>) => {
        if (event.data.type !== "frame" || event.data.generation !== generationRef.current) return;
        event.data.positions.forEach(([x, y], index) => {
          const node = positionedRef.current[index];
          if (node) { node.px = x; node.py = y; }
        });
        movingRef.current = !event.data.done;
        treeRef.current = quadtree<PositionedNode>().x((node) => node.px).y((node) => node.py).addAll(positionedRef.current);
        requestDraw();
      };
      worker.onerror = () => {
        movingRef.current = false;
        worker.terminate();
        if (workerRef.current === worker) workerRef.current = null;
        physicsErrorRef.current?.("Graph physics worker unavailable; using seeded positions.");
        requestDraw();
      };
    } catch {
      workerRef.current = null;
      physicsErrorRef.current?.("Graph physics worker unavailable; using seeded positions.");
    }
    return () => {
      workerRef.current?.terminate();
      workerRef.current = null;
    };
  }, [requestDraw]);

  useEffect(() => {
    const projectionChanged = projectionKeyRef.current !== projectionKey;
    if (projectionChanged) {
      projectionKeyRef.current = projectionKey;
      manualPinsRef.current.clear();
      hoveredIndexRef.current = null;
      dragRef.current = null;
      pendingPointerRef.current = null;
      if (pointerFrameRef.current != null) {
        cancelAnimationFrame(pointerFrameRef.current);
        pointerFrameRef.current = null;
      }
      setHovered(null);
      setDragging(false);
      setPinRevision((value) => value + 1);
    }
    const previous = projectionChanged ? new Map<string, PositionedNode>() : new Map(positionedRef.current.map((node) => [node.id, node]));
    const hadExisting = nodes.some((node) => previous.has(node.id));
    positionedRef.current = nodes.map((node) => {
      const existing = previous.get(node.id);
      const manualPin = manualPinsRef.current.get(node.id);
      return { ...node, px: manualPin?.x ?? existing?.px ?? node.x, py: manualPin?.y ?? existing?.py ?? node.y };
    });
    treeRef.current = quadtree<PositionedNode>().x((node) => node.px).y((node) => node.py).addAll(positionedRef.current);
    generationRef.current += 1;
    const generation = generationRef.current;
    movingRef.current = workerRef.current != null;
    postCommand({
      type: "init",
      generation,
      mode,
      nodes: positionedRef.current.map((node) => ({
        index: node.index,
        x: node.px,
        y: node.py,
        size: node.size,
        pinned: manualPinsRef.current.has(node.id) || !!node.pinned || (mode === "performance" && previous.has(node.id)),
      })),
      edges,
    });
    requestDraw();
    if (!hadExisting) queueMicrotask(fit);
  }, [nodes, edges, mode, projectionKey, fit, postCommand, requestDraw]);

  useEffect(() => {
    dragRef.current = null;
    setDragging(false);
    pendingPointerRef.current = null;
    if (pointerFrameRef.current != null) {
      cancelAnimationFrame(pointerFrameRef.current);
      pointerFrameRef.current = null;
    }
  }, [mode]);

  useEffect(() => () => {
    if (frameRef.current != null) {
      cancelAnimationFrame(frameRef.current);
      frameRef.current = null;
    }
    if (pointerFrameRef.current != null) {
      cancelAnimationFrame(pointerFrameRef.current);
      pointerFrameRef.current = null;
    }
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
  useEffect(requestDraw, [requestDraw, selected, showLabels, mode, pinRevision]);

  const startDrag = (event: React.PointerEvent<HTMLCanvasElement>) => {
    if (mode !== "fancy" || event.button !== 0) return;
    const node = pickAt(event.clientX, event.clientY, event.currentTarget);
    if (!node) return;
    const point = pointAt(event.clientX, event.clientY, event.currentTarget);
    if (!point) return;
    clearPointerField();
    dragRef.current = {
      pointerId: event.pointerId,
      node,
      startClientX: event.clientX,
      startClientY: event.clientY,
      moved: false,
      wasPinned: manualPinsRef.current.has(node.id),
    };
    setDragging(true);
    try { event.currentTarget.setPointerCapture(event.pointerId); } catch { /* optional in synthetic environments */ }
    postCommand({ type: "drag-start", generation: generationRef.current, index: node.index, x: point[0], y: point[1] });
    event.preventDefault();
    event.stopPropagation();
  };

  const movePointer = (event: React.PointerEvent<HTMLCanvasElement>) => {
    const drag = dragRef.current;
    if (drag && drag.pointerId === event.pointerId) {
      const point = pointAt(event.clientX, event.clientY, event.currentTarget);
      if (!point) return;
      if (Math.hypot(event.clientX - drag.startClientX, event.clientY - drag.startClientY) >= 3) drag.moved = true;
      drag.node.px = point[0];
      drag.node.py = point[1];
      hoveredIndexRef.current = drag.node.index;
      setHovered({ node: drag.node, x: event.nativeEvent.offsetX, y: event.nativeEvent.offsetY });
      postCommand({ type: "drag-move", generation: generationRef.current, index: drag.node.index, x: point[0], y: point[1] });
      requestDraw();
      event.preventDefault();
      return;
    }

    const node = pickAt(event.clientX, event.clientY, event.currentTarget);
    hoveredIndexRef.current = node?.index ?? null;
    setHovered(node ? { node, x: event.nativeEvent.offsetX, y: event.nativeEvent.offsetY } : null);
    const point = pointAt(event.clientX, event.clientY, event.currentTarget);
    if (point) queuePointerField(point[0], point[1], 90 / transformRef.current.k);
    requestDraw();
  };

  const endDrag = (event: React.PointerEvent<HTMLCanvasElement>, cancelled = false) => {
    const drag = dragRef.current;
    if (!drag || drag.pointerId !== event.pointerId) return;
    const pin = !cancelled && drag.moved && event.shiftKey ? true : !drag.moved && drag.wasPinned;
    postCommand({ type: "drag-end", generation: generationRef.current, index: drag.node.index, pin });
    if (drag.moved) {
      if (pin) manualPinsRef.current.set(drag.node.id, { x: drag.node.px, y: drag.node.py });
      else manualPinsRef.current.delete(drag.node.id);
      setPinRevision((value) => value + 1);
      suppressClickRef.current = true;
      window.setTimeout(() => { suppressClickRef.current = false; }, 0);
    }
    dragRef.current = null;
    setDragging(false);
    try { event.currentTarget.releasePointerCapture(event.pointerId); } catch { /* optional in synthetic environments */ }
    requestDraw();
  };

  return <div className="graph-canvas" ref={hostRef}>
    <canvas
      ref={canvasRef}
      className={dragging ? "is-dragging" : hovered ? "is-node-hovered" : undefined}
      aria-label="Code graph canvas"
      data-graph-mode={mode}
      data-projection-key={projectionKey}
      onPointerDown={startDrag}
      onPointerMove={movePointer}
      onPointerUp={(event) => endDrag(event)}
      onPointerCancel={(event) => endDrag(event, true)}
      onLostPointerCapture={(event) => endDrag(event, true)}
      onPointerLeave={() => {
        if (dragRef.current) return;
        hoveredIndexRef.current = null;
        setHovered(null);
        clearPointerField();
        requestDraw();
      }}
      onClick={(event) => {
        if (suppressClickRef.current) return;
        const node = pickAt(event.clientX, event.clientY, event.currentTarget);
        if (node) onSelect(edgeNodes.get(node.index) ?? node);
      }}
      onDoubleClick={(event) => {
        if (suppressClickRef.current) return;
        const node = pickAt(event.clientX, event.clientY, event.currentTarget);
        if (node) onExplore?.(edgeNodes.get(node.index) ?? node);
      }}
    />
    {hovered && <div className="node-tooltip" style={{ left: hovered.x + 12, top: hovered.y + 12 }}>
      <span style={{ background: kindColor(hovered.node.kind) }} />
      <strong>{hovered.node.name}</strong>
      <small>{hovered.node.kind} · {(hovered.node.member_count ?? hovered.node.degree).toLocaleString()}{manualPinsRef.current.has(hovered.node.id) ? " · pinned" : ""}</small>
      {mode === "fancy" && <small className="tooltip-hint">Drag to move · Shift-drag to pin</small>}
    </div>}
  </div>;
}
