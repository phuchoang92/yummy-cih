import { describe, expect, it } from "vitest";
import { normalizeGraph } from "./ClassicViews";

describe("normalizeGraph", () => {
  it("normalizes CIH nodes and deduplicates missing endpoints", () => {
    const graph = normalizeGraph({
      nodes: [{ id: "Method:A#run/0", name: "run", kind: "Method", depth: 0 }],
      edges: [{ src: "Method:A#run/0", dst: "Method:B#save/0", kind: "Calls" }],
    });
    expect(graph.nodes).toHaveLength(2);
    expect(graph.edges).toEqual([{ source: "Method:A#run/0", target: "Method:B#save/0", label: "Calls" }]);
  });
});
