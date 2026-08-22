import { cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { GraphCanvas } from "./Scene";
import type { LayoutCommand } from "./layout-protocol";
import type { OverviewNode } from "./types";

const workerState = vi.hoisted(() => ({ instances: [] as Array<{ postMessage: ReturnType<typeof vi.fn>; terminate: ReturnType<typeof vi.fn> }> }));

vi.mock("./layout.worker?worker&inline", () => ({
  default: class MockLayoutWorker {
    postMessage = vi.fn();
    terminate = vi.fn();
    onmessage: ((event: MessageEvent) => void) | null = null;
    onerror: ((event: ErrorEvent) => void) | null = null;
    constructor() { workerState.instances.push(this); }
  },
}));

const NODE: OverviewNode = {
  index: 0,
  id: "Function:demo",
  kind: "Function",
  name: "demo",
  degree: 2,
  x: 0,
  y: 0,
  size: 4,
};

let rafCallbacks: Map<number, FrameRequestCallback>;
let nextRaf: number;
let reducedMotion = false;

function flushAnimationFrames() {
  const callbacks = [...rafCallbacks.values()];
  rafCallbacks.clear();
  for (const callback of callbacks) callback(performance.now());
}

function commands(): LayoutCommand[] {
  return workerState.instances.at(-1)?.postMessage.mock.calls.map(([command]) => command as LayoutCommand) ?? [];
}

function renderCanvas(mode: "performance" | "fancy", projectionKey = "repository:root") {
  const onSelect = vi.fn<(node: OverviewNode) => void>();
  const onExplore = vi.fn<(node: OverviewNode) => void>();
  const result = render(<GraphCanvas nodes={[NODE]} edges={[]} selected={null} showLabels={false} resetNonce={0} mode={mode} projectionKey={projectionKey} onSelect={onSelect} onExplore={onExplore} />);
  return { ...result, canvas: screen.getByLabelText("Code graph canvas"), onSelect, onExplore };
}

beforeEach(() => {
  workerState.instances.length = 0;
  rafCallbacks = new Map();
  nextRaf = 1;
  reducedMotion = false;
  vi.stubGlobal("requestAnimationFrame", vi.fn((callback: FrameRequestCallback) => {
    const id = nextRaf++;
    rafCallbacks.set(id, callback);
    return id;
  }));
  vi.stubGlobal("cancelAnimationFrame", vi.fn((id: number) => { rafCallbacks.delete(id); }));
  vi.stubGlobal("PointerEvent", class PointerEvent extends MouseEvent {
    pointerId: number;
    constructor(type: string, init: PointerEventInit = {}) {
      super(type, init);
      this.pointerId = init.pointerId ?? 0;
    }
  });
  vi.stubGlobal("matchMedia", vi.fn(() => ({
    matches: reducedMotion,
    media: "(prefers-reduced-motion: reduce)",
    onchange: null,
    addEventListener: vi.fn(),
    removeEventListener: vi.fn(),
    addListener: vi.fn(),
    removeListener: vi.fn(),
    dispatchEvent: vi.fn(),
  })));
  vi.spyOn(HTMLCanvasElement.prototype, "getContext").mockReturnValue(null);
  Object.defineProperty(HTMLCanvasElement.prototype, "setPointerCapture", { configurable: true, value: vi.fn() });
  Object.defineProperty(HTMLCanvasElement.prototype, "releasePointerCapture", { configurable: true, value: vi.fn() });
  vi.spyOn(HTMLCanvasElement.prototype, "getBoundingClientRect").mockReturnValue({
    x: 0, y: 0, left: 0, top: 0, right: 800, bottom: 600, width: 800, height: 600, toJSON: () => ({}),
  });
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  vi.unstubAllGlobals();
});

describe("GraphCanvas interactions", () => {
  it("keeps Performance static while Fancy emits drag and pin commands", () => {
    const performanceView = renderCanvas("performance");
    fireEvent.pointerDown(performanceView.canvas, { pointerId: 1, button: 0, clientX: 0, clientY: 0 });
    expect(commands().some((command) => command.type === "drag-start")).toBe(false);
    cleanup();

    const fancy = renderCanvas("fancy");
    fireEvent.pointerDown(fancy.canvas, { pointerId: 2, button: 0, clientX: 0, clientY: 0 });
    fireEvent.pointerMove(fancy.canvas, { pointerId: 2, clientX: 10, clientY: 0 });
    fireEvent.pointerUp(fancy.canvas, { pointerId: 2, clientX: 10, clientY: 0, shiftKey: true });

    expect(commands().map((command) => command.type)).toEqual(expect.arrayContaining(["init", "drag-start", "drag-move", "drag-end"]));
    expect([...commands()].reverse().find((command) => command.type === "drag-end")).toMatchObject({ pin: true });
    fireEvent.click(fancy.canvas, { clientX: 10, clientY: 0 });
    expect(fancy.onSelect).not.toHaveBeenCalled();
  });

  it("releases a pinned node on the next normal drag and preserves double-click explore", async () => {
    const view = renderCanvas("fancy");
    fireEvent.pointerDown(view.canvas, { pointerId: 1, button: 0, clientX: 0, clientY: 0 });
    fireEvent.pointerMove(view.canvas, { pointerId: 1, clientX: 8, clientY: 0 });
    fireEvent.pointerUp(view.canvas, { pointerId: 1, clientX: 8, clientY: 0, shiftKey: true });
    fireEvent.pointerDown(view.canvas, { pointerId: 2, button: 0, clientX: 8, clientY: 0 });
    fireEvent.pointerMove(view.canvas, { pointerId: 2, clientX: 12, clientY: 0 });
    fireEvent.pointerUp(view.canvas, { pointerId: 2, clientX: 12, clientY: 0 });
    expect(commands().filter((command) => command.type === "drag-end").at(-1)).toMatchObject({ pin: false });

    flushAnimationFrames();
    await new Promise((resolve) => window.setTimeout(resolve, 0));
    fireEvent.doubleClick(view.canvas, { clientX: 12, clientY: 0 });
    expect(view.onExplore).toHaveBeenCalledWith(expect.objectContaining({ id: NODE.id }));
  });

  it("throttles hover force and disables it for reduced motion", () => {
    const view = renderCanvas("fancy");
    fireEvent.pointerMove(view.canvas, { pointerId: 1, clientX: 20, clientY: 20 });
    fireEvent.pointerMove(view.canvas, { pointerId: 1, clientX: 22, clientY: 20 });
    flushAnimationFrames();
    expect(commands().filter((command) => command.type === "pointer-field")).toHaveLength(1);
    cleanup();

    reducedMotion = true;
    const reduced = renderCanvas("fancy");
    fireEvent.pointerMove(reduced.canvas, { pointerId: 2, clientX: 20, clientY: 20 });
    flushAnimationFrames();
    expect(commands().some((command) => command.type === "pointer-field")).toBe(false);
  });

  it("clears manual pins and reinitializes layout when the projection key changes", () => {
    const view = renderCanvas("fancy");
    fireEvent.pointerDown(view.canvas, { pointerId: 1, button: 0, clientX: 0, clientY: 0 });
    fireEvent.pointerMove(view.canvas, { pointerId: 1, clientX: 10, clientY: 0 });
    fireEvent.pointerUp(view.canvas, { pointerId: 1, clientX: 10, clientY: 0, shiftKey: true });
    expect([...commands()].reverse().find((command) => command.type === "drag-end")).toMatchObject({ pin: true });

    view.rerender(<GraphCanvas nodes={[NODE]} edges={[]} selected={null} showLabels={false} resetNonce={0} mode="fancy" projectionKey="community:Architecture" onSelect={view.onSelect} onExplore={view.onExplore} />);
    const latestInit = [...commands()].reverse().find((command) => command.type === "init");
    expect(latestInit).toMatchObject({ type: "init", nodes: [expect.objectContaining({ pinned: false })] });
    expect(view.canvas).toHaveAttribute("data-projection-key", "community:Architecture");
  });
});
