import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Overview } from "./Overview";
import type { OverviewData } from "./types";

// Mock the Scene module since WebGL is not available in jsdom.
vi.mock("./Scene", () => ({
  GalaxyScene: () => <div data-testid="galaxy-scene" />,
  cameraTarget: () => null,
  hasWebGl: () => true,
}));

afterEach(() => { cleanup(); vi.restoreAllMocks(); });

const MOCK_DATA: OverviewData = {
  nodes: [
    { index: 0, id: "Route:GET /orders", kind: "Route", name: "GET /orders", qualified_name: null, file: "", degree: 3, x: 0, y: 0, z: 0, size: 9, color: "#ffc070" },
    { index: 1, id: "Method:Orders#list/0", kind: "Method", name: "list", qualified_name: "Orders#list/0", file: "src/orders.rs", degree: 5, x: 10, y: 0, z: 0, size: 4, color: "#ffe080" },
    { index: 2, id: "Class:OrderRepo", kind: "Class", name: "OrderRepo", qualified_name: "OrderRepo", file: "src/repo.rs", degree: 1, x: -10, y: 0, z: 0, size: 5, color: "#ff6050" },
  ],
  edges: [
    { source: 0, target: 1, kind: "HANDLES_ROUTE" },
    { source: 1, target: 2, kind: "CALLS" },
  ],
  total_nodes: 300,
  total_edges: 890,
  truncated: true,
};

function mockFetchOverview(data: OverviewData = MOCK_DATA) {
  vi.stubGlobal("fetch", vi.fn((url: string) => {
    if (url.includes("/api/graph/overview")) {
      return Promise.resolve({ ok: true, text: async () => JSON.stringify(data) });
    }
    if (url.includes("/api/graph/context")) {
      return Promise.resolve({
        ok: true,
        text: async () => JSON.stringify({
          node: { id: MOCK_DATA.nodes[0].id, kind: "Route", name: "GET /orders", file: "" },
          callers: [], callees: [{ id: "Method:Orders#list/0", kind: "Method", name: "list", file: "src/orders.rs" }],
          processes: [], community: null,
        }),
      });
    }
    return Promise.resolve({ ok: true, text: async () => "{}" });
  }));
}

describe("Overview", () => {
  it("renders node and edge counts with truncation indicator", async () => {
    mockFetchOverview();
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    // Wait for data to load — the projection meta shows "3 of 300 nodes" and "2 of 890 edges"
    await waitFor(() => expect(screen.getByText("bounded view")).toBeInTheDocument());
    // The "3 stars" HUD confirms rendered node count
    expect(screen.getByText("3 stars")).toBeInTheDocument();
  });

  it("renders node type filter chips for each kind", async () => {
    mockFetchOverview();
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByText("Route")).toBeInTheDocument());
    expect(screen.getByText("Method")).toBeInTheDocument();
    expect(screen.getByText("Class")).toBeInTheDocument();
  });

  it("toggles node type filter on chip click", async () => {
    mockFetchOverview();
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByText("Route")).toBeInTheDocument());
    const routeChip = screen.getByText("Route").closest("button")!;
    expect(routeChip).toHaveClass("is-active");
    fireEvent.click(routeChip);
    expect(routeChip).not.toHaveClass("is-active");
    // Clicking again re-enables it
    fireEvent.click(routeChip);
    expect(routeChip).toHaveClass("is-active");
  });

  it("renders edge relationship filter chips", async () => {
    mockFetchOverview();
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByText("handles route")).toBeInTheDocument());
    expect(screen.getByText("calls")).toBeInTheDocument();
  });

  it("renders file cluster tree list", async () => {
    mockFetchOverview();
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    // Cluster names are derived from the first 2 path components of the file
    await waitFor(() => expect(screen.getByText("src/orders.rs")).toBeInTheDocument());
    expect(screen.getByText("src/repo.rs")).toBeInTheDocument();
  });

  it("shows inspector when a node is selected", async () => {
    mockFetchOverview();
    const onSelectedId = vi.fn();
    render(<Overview selectedId="Route:GET /orders" onSelectedId={onSelectedId} />);
    await waitFor(() => expect(screen.getByText("GET /orders")).toBeInTheDocument());
    // Inspector should appear with context data
    await waitFor(() => expect(screen.getByText("outbound")).toBeInTheDocument());
  });

  it("displays error state and retry button on fetch failure", async () => {
    vi.stubGlobal("fetch", vi.fn().mockRejectedValue(new Error("network down")));
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByText("Overview unavailable")).toBeInTheDocument());
    expect(screen.getByText("network down")).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Retry" })).toBeInTheDocument();
  });

  it("displays empty state when graph has no nodes", async () => {
    mockFetchOverview({ nodes: [], edges: [], total_nodes: 0, total_edges: 0, truncated: false });
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByText("No graph data")).toBeInTheDocument());
    expect(screen.getByText("Index a repository, then refresh this view.")).toBeInTheDocument();
  });

  it("filters search results as user types", async () => {
    mockFetchOverview();
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByText("Route")).toBeInTheDocument());
    const searchInput = screen.getByPlaceholderText("Find node or file…");
    fireEvent.change(searchInput, { target: { value: "list" } });
    // The search should find the Method node named "list"
    await waitFor(() => expect(screen.getByText("list")).toBeInTheDocument());
  });

  it("shows the clear selection button when nodes are selected", async () => {
    mockFetchOverview();
    render(<Overview selectedId="Route:GET /orders" onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByText("Clear selection")).toBeInTheDocument());
  });

  it("shows the galaxy scene component", async () => {
    mockFetchOverview();
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByTestId("galaxy-scene")).toBeInTheDocument());
  });

  it("shows HUD with star and link counts", async () => {
    mockFetchOverview();
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByText("3 stars")).toBeInTheDocument());
    expect(screen.getByText("2 links")).toBeInTheDocument();
  });
});
