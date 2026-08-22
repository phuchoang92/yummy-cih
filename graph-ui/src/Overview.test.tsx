import { act, cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { Overview } from "./Overview";
import type { OverviewData } from "./types";

// Mock the Canvas renderer; interaction/state is covered by the Overview tests.
const graphCanvasProps = vi.hoisted(() => ({ current: null as Record<string, unknown> | null }));
vi.mock("./Scene", () => ({
  GraphCanvas: (props: Record<string, unknown>) => { graphCanvasProps.current = props; return <div data-testid="graph-canvas" data-mode={String(props.mode)} />; },
  cameraTarget: () => null,
  hasWebGl: () => true,
}));

afterEach(() => { cleanup(); vi.restoreAllMocks(); graphCanvasProps.current = null; try { localStorage.clear(); } catch { /* optional */ } });

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

const ROOT_NAV_DATA: OverviewData = {
  scope: "repository",
  nodes: [
    { index: 0, id: "Community:Architecture", kind: "Community", name: "Architecture", role: "aggregate", member_count: 20, expandable: true, degree: 4, x: 0, y: 0, size: 8 },
    { index: 1, id: "Method:root", kind: "Method", name: "rootMethod", role: "entity", degree: 2, x: 20, y: 0, size: 4 },
  ],
  edges: [{ source: 0, target: 1, kind: "CALLS" }],
  total_nodes: 2,
  total_edges: 1,
  truncated: false,
};

const CHILD_NAV_DATA: OverviewData = {
  scope: "community",
  parent_id: "Community:Architecture",
  nodes: [{ index: 0, id: "File:src/App.ts", kind: "File", name: "App.ts", role: "aggregate", member_count: 8, expandable: true, degree: 1, x: 0, y: 0, size: 7 }],
  edges: [],
  total_nodes: 1,
  total_edges: 0,
  truncated: false,
};

function response(data: OverviewData) { return { ok: true, text: async () => JSON.stringify(data) }; }
function deferred<T>() {
  let resolve!: (value: T) => void;
  let reject!: (reason?: unknown) => void;
  const promise = new Promise<T>((next, fail) => { resolve = next; reject = fail; });
  return { promise, resolve, reject };
}

function mockFetchOverview(data: OverviewData = MOCK_DATA) {
  vi.stubGlobal("fetch", vi.fn((url: string) => {
    if (url.includes("/api/graph/summary")) {
      const kinds = [...new Set(data.nodes.map((n) => n.kind))].map((kind) => ({ kind, count: 1 }));
      return Promise.resolve({ ok: true, text: async () => JSON.stringify({ kinds, total_nodes: data.total_nodes, total_edges: data.total_edges }) });
    }
    if (url.includes("/api/graph/projection")) {
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
    expect(screen.getByText("3 nodes")).toBeInTheDocument();
  });

  it("renders node type filter chips for each kind", async () => {
    mockFetchOverview();
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getAllByText("Route").length).toBeGreaterThan(0));
    expect(screen.getAllByText("Method").length).toBeGreaterThan(0);
    expect(screen.getAllByText("Class").length).toBeGreaterThan(0);
  });

  it("toggles node type filter on chip click", async () => {
    mockFetchOverview();
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getAllByText("Route").length).toBeGreaterThan(0));
    const routeChip = screen.getAllByText("Route")[0].closest("button")!;
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

  it("renders the bounded projection node list", async () => {
    mockFetchOverview();
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getAllByText("GET /orders").length).toBeGreaterThan(0));
    expect(screen.getAllByText("OrderRepo").length).toBeGreaterThan(0);
  });

  it("shows inspector when a node is selected", async () => {
    mockFetchOverview();
    const onSelectedId = vi.fn();
    render(<Overview selectedId="Route:GET /orders" onSelectedId={onSelectedId} />);
    await waitFor(() => expect(screen.getAllByText("GET /orders").length).toBeGreaterThan(0));
    await waitFor(() => expect(screen.getByText("members")).toBeInTheDocument());
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
    await waitFor(() => expect(screen.getAllByText("Route").length).toBeGreaterThan(0));
    const searchInput = screen.getByPlaceholderText("Find node or group…");
    fireEvent.change(searchInput, { target: { value: "list" } });
    // The search should find the Method node named "list"
    await waitFor(() => expect(screen.getByText("list")).toBeInTheDocument());
  });

  it("shows the clear selection button when nodes are selected", async () => {
    mockFetchOverview();
    render(<Overview selectedId="Route:GET /orders" onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByText("Clear selection")).toBeInTheDocument());
  });

  it("shows the graph canvas component", async () => {
    mockFetchOverview();
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByTestId("graph-canvas")).toBeInTheDocument());
  });

  it("shows HUD with node and relationship counts", async () => {
    mockFetchOverview();
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByText("3 nodes")).toBeInTheDocument());
    expect(screen.getByText("2 relationships")).toBeInTheDocument();
  });

  it("defaults to Performance without reloading the projection when mode changes", async () => {
    mockFetchOverview();
    const fetchMock = vi.mocked(fetch);
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByTestId("graph-canvas")).toHaveAttribute("data-mode", "performance"));
    expect(screen.getByRole("button", { name: "Performance" })).toHaveAttribute("aria-pressed", "true");
    const projectionCalls = fetchMock.mock.calls.filter(([url]) => String(url).includes("/api/graph/projection")).length;

    fireEvent.click(screen.getByRole("button", { name: "Fancy" }));
    expect(screen.getByTestId("graph-canvas")).toHaveAttribute("data-mode", "fancy");
    expect(localStorage.getItem("cih-graph-mode")).toBe("fancy");
    expect(fetchMock.mock.calls.filter(([url]) => String(url).includes("/api/graph/projection"))).toHaveLength(projectionCalls);
    expect(graphCanvasProps.current?.mode).toBe("fancy");
  });

  it("restores Fancy from localStorage and rejects unknown stored modes", async () => {
    localStorage.setItem("cih-graph-mode", "fancy");
    mockFetchOverview();
    const first = render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByTestId("graph-canvas")).toHaveAttribute("data-mode", "fancy"));
    first.unmount();

    localStorage.setItem("cih-graph-mode", "turbo");
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByTestId("graph-canvas")).toHaveAttribute("data-mode", "performance"));
  });

  it("keeps the current projection mounted while a child projection loads", async () => {
    const child = deferred<{ ok: boolean; text: () => Promise<string> }>();
    vi.stubGlobal("fetch", vi.fn((url: string) => {
      if (url.includes("scope=community")) return child.promise;
      return Promise.resolve(response(ROOT_NAV_DATA));
    }));
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByText("Architecture")).toBeInTheDocument());

    fireEvent.doubleClick(screen.getByText("Architecture").closest("button")!);
    expect(screen.getByTestId("graph-canvas")).toBeInTheDocument();
    expect(screen.getByText("2 nodes")).toBeInTheDocument();
    expect(screen.queryByText("Loading bounded projection")).not.toBeInTheDocument();
    expect(screen.getByRole("status")).toHaveTextContent("Opening Architecture…");

    await act(async () => { child.resolve(response(CHILD_NAV_DATA)); });
    await waitFor(() => expect(screen.getByText("App.ts")).toBeInTheDocument());
    expect(document.querySelector(".overview-shell")).toHaveClass("is-enter-in");
    expect(screen.getByRole("button", { name: "Back to Repository" })).toBeInTheDocument();
  });

  it("restores the parent data and controls from cache without refetching", async () => {
    const fetchMock = vi.fn((url: string) => Promise.resolve(response(url.includes("scope=community") ? CHILD_NAV_DATA : ROOT_NAV_DATA)));
    vi.stubGlobal("fetch", fetchMock);
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByText("Architecture")).toBeInTheDocument());
    const searchInput = screen.getByPlaceholderText("Find node or group…");
    fireEvent.change(searchInput, { target: { value: "Architecture" } });
    fireEvent.doubleClick(screen.getByText("Architecture").closest("button")!);
    await waitFor(() => expect(screen.getByText("App.ts")).toBeInTheDocument());
    expect(screen.getByPlaceholderText("Find node or group…")).toHaveValue("");
    const projectionCalls = fetchMock.mock.calls.filter(([url]) => String(url).includes("/api/graph/projection")).length;

    fireEvent.click(screen.getByRole("button", { name: "Back to Repository" }));
    expect(screen.getByText("Architecture")).toBeInTheDocument();
    expect(screen.getByPlaceholderText("Find node or group…")).toHaveValue("Architecture");
    expect(fetchMock.mock.calls.filter(([url]) => String(url).includes("/api/graph/projection"))).toHaveLength(projectionCalls);
    expect(document.querySelector(".overview-shell")).toHaveClass("is-enter-out");
    expect(screen.queryByRole("button", { name: /Back to/ })).not.toBeInTheDocument();
  });

  it("keeps the parent view and breadcrumbs when a child projection fails", async () => {
    vi.stubGlobal("fetch", vi.fn((url: string) => url.includes("scope=community")
      ? Promise.reject(new Error("child unavailable"))
      : Promise.resolve(response(ROOT_NAV_DATA))));
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByText("Architecture")).toBeInTheDocument());
    fireEvent.doubleClick(screen.getByText("Architecture").closest("button")!);

    await waitFor(() => expect(screen.getByText("child unavailable")).toBeInTheDocument());
    expect(screen.getByText("Architecture")).toBeInTheDocument();
    expect(screen.getByTestId("graph-canvas")).toBeInTheDocument();
    expect(screen.queryByRole("button", { name: /Back to/ })).not.toBeInTheDocument();
    expect(screen.queryByRole("status")).not.toBeInTheDocument();
  });

  it("ignores an older projection response after a newer hop completes", async () => {
    const first = deferred<{ ok: boolean; text: () => Promise<string> }>();
    const second = deferred<{ ok: boolean; text: () => Promise<string> }>();
    let childRequest = 0;
    vi.stubGlobal("fetch", vi.fn((url: string) => {
      if (!url.includes("scope=community")) return Promise.resolve(response(ROOT_NAV_DATA));
      childRequest += 1;
      return childRequest === 1 ? first.promise : second.promise;
    }));
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByText("Architecture")).toBeInTheDocument());
    const target = screen.getByText("Architecture").closest("button")!;
    fireEvent.doubleClick(target);
    fireEvent.doubleClick(target);
    const newer = { ...CHILD_NAV_DATA, nodes: [{ ...CHILD_NAV_DATA.nodes[0], id: "File:new.ts", name: "new.ts" }] };
    const older = { ...CHILD_NAV_DATA, nodes: [{ ...CHILD_NAV_DATA.nodes[0], id: "File:old.ts", name: "old.ts" }] };
    await act(async () => { second.resolve(response(newer)); });
    await waitFor(() => expect(screen.getByText("new.ts")).toBeInTheDocument());
    await act(async () => { first.resolve(response(older)); });
    expect(screen.getByText("new.ts")).toBeInTheDocument();
    expect(screen.queryByText("old.ts")).not.toBeInTheDocument();
  });

  it("preserves active search and filters when refreshing the current projection", async () => {
    const fetchMock = vi.fn(() => Promise.resolve(response(ROOT_NAV_DATA)));
    vi.stubGlobal("fetch", fetchMock);
    render(<Overview selectedId={null} onSelectedId={() => {}} />);
    await waitFor(() => expect(screen.getByText("Architecture")).toBeInTheDocument());
    fireEvent.click(screen.getAllByText("Method")[0].closest("button")!);
    fireEvent.change(screen.getByPlaceholderText("Find node or group…"), { target: { value: "Architecture" } });
    fireEvent.click(screen.getByRole("button", { name: "Refresh" }));
    await waitFor(() => expect(fetchMock.mock.calls.length).toBeGreaterThan(1));

    expect(screen.getByPlaceholderText("Find node or group…")).toHaveValue("Architecture");
    expect(screen.getAllByText("Method")[0].closest("button")).not.toHaveClass("is-active");
  });
});
