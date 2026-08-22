/// Incremental, cancellable force refinement. Server coordinates are always a
/// usable first frame; Fancy mode keeps the cooled simulation available for
/// bounded interaction bursts without occupying the main thread.
import { LayoutController } from "./layout-controller";
import type { LayoutCommand } from "./layout-protocol";

const controller = new LayoutController();
let scheduled = false;

self.onmessage = (event: MessageEvent<LayoutCommand>) => {
  if (controller.handle(event.data, performance.now())) scheduleStep();
};

function scheduleStep() {
  if (scheduled) return;
  scheduled = true;
  setTimeout(step, 0);
}

function step() {
  scheduled = false;
  const generation = controller.generation;
  if (generation == null || !controller.running) return;
  const sliceStarted = performance.now();
  while (controller.generation === generation && controller.running && performance.now() - sliceStarted < 8) {
    controller.tick(performance.now());
  }
  if (controller.generation !== generation) {
    scheduleStep();
    return;
  }
  const frame = controller.takeFrame(performance.now());
  if (frame) self.postMessage(frame);
  if (controller.running) scheduleStep();
}

export {};
