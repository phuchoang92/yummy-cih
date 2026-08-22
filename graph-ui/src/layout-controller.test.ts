import { describe, expect, it } from "vitest";
import { LayoutController } from "./layout-controller";
import type { LayoutCommand } from "./layout-protocol";

function init(mode: "performance" | "fancy", generation = 1): LayoutCommand {
  return {
    type: "init",
    generation,
    mode,
    nodes: [
      { index: 10, x: -20, y: 0, size: 4 },
      { index: 20, x: 20, y: 0, size: 4 },
    ],
    edges: [{ source: 10, target: 20 }],
  };
}

function settle(controller: LayoutController, start = 0): number {
  let now = start;
  for (let step = 0; step < 400 && controller.running; step += 1) {
    now += 10;
    controller.tick(now);
  }
  expect(controller.running).toBe(false);
  return now;
}

describe("LayoutController", () => {
  it("keeps Performance bounded and ignores interactive commands", () => {
    const controller = new LayoutController();
    expect(controller.handle(init("performance"), 0)).toBe(true);
    controller.tick(10);
    expect(controller.takeFrame(20)).toBeNull();
    expect(controller.takeFrame(50)?.done).toBe(false);
    const finishedAt = settle(controller, 50);
    expect(finishedAt).toBeLessThanOrEqual(550);
    expect(controller.handle({ type: "drag-start", generation: 1, index: 10, x: 100, y: 50 }, finishedAt)).toBe(false);
  });

  it("reheats Fancy for drag, supports pin, and releases a pinned node", () => {
    const controller = new LayoutController();
    controller.handle(init("fancy"), 0);
    let now = settle(controller);

    expect(controller.handle({ type: "drag-start", generation: 1, index: 10, x: 100, y: 50 }, ++now)).toBe(true);
    controller.tick(++now);
    expect(controller.takeFrame(now, true)?.positions[0]).toEqual([100, 50]);
    expect(controller.handle({ type: "drag-end", generation: 1, index: 10, pin: true }, ++now)).toBe(true);
    expect(controller.isPinned(10)).toBe(true);
    now = settle(controller, now);
    expect(controller.isPinned(10)).toBe(true);

    controller.handle({ type: "drag-start", generation: 1, index: 10, x: 110, y: 60 }, ++now);
    controller.handle({ type: "drag-move", generation: 1, index: 10, x: 120, y: 70 }, ++now);
    controller.handle({ type: "drag-end", generation: 1, index: 10, pin: false }, ++now);
    expect(controller.isPinned(10)).toBe(false);
  });

  it("runs hover force in bounded bursts and ignores stale generations", () => {
    const controller = new LayoutController();
    controller.handle(init("fancy", 7), 0);
    let now = settle(controller);
    const before = controller.takeFrame(now, true)!.positions;

    expect(controller.handle({ type: "pointer-field", generation: 6, x: 0, y: 0, radius: 90 }, ++now)).toBe(false);
    expect(controller.handle({ type: "pointer-field", generation: 7, x: 0, y: 0, radius: 90 }, ++now)).toBe(true);
    for (let step = 0; step < 8; step += 1) controller.tick(now += 10);
    const during = controller.takeFrame(now, true)!.positions;
    expect(during).not.toEqual(before);
    expect(controller.handle({ type: "pointer-clear", generation: 7 }, ++now)).toBe(true);
    now = settle(controller, now);

    const asleep = controller.takeFrame(now, true)!.positions;
    expect(controller.tick(now + 1_000)).toBe(false);
    expect(controller.takeFrame(now + 1_000, true)!.positions).toEqual(asleep);
  });
});
