import { cleanup, fireEvent, render, screen, waitFor } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { App } from "./App";

afterEach(() => { cleanup(); vi.restoreAllMocks(); delete document.documentElement.dataset.theme; try { localStorage.clear(); } catch { /* ignore */ } });

describe("App", () => {
  it("opens on the bounded 3D overview", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({
      ok: true,
      text: async () => JSON.stringify({ nodes: [], edges: [], total_nodes: 0, total_edges: 0, truncated: false }),
    }));
    render(<App />);
    expect(screen.getByRole("button", { name: "Overview" })).toHaveClass("is-active");
    await waitFor(() => expect(screen.getByText("No graph data")).toBeInTheDocument());
  });

  it("keeps analytical views available", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({
      ok: true,
      text: async () => JSON.stringify({ nodes: [], edges: [], total_nodes: 0, total_edges: 0, truncated: false }),
    }));
    render(<App />);
    expect(screen.getByRole("button", { name: "Impact" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Flow" })).toBeInTheDocument();
    expect(screen.getByRole("button", { name: "Routes" })).toBeInTheDocument();
  });

  it("switches from Overview to Impact tab", async () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({
      ok: true,
      text: async () => JSON.stringify({ nodes: [], edges: [], total_nodes: 0, total_edges: 0, truncated: false }),
    }));
    render(<App />);
    const overviewBtn = screen.getByRole("button", { name: "Overview" });
    const impactBtn = screen.getByRole("button", { name: "Impact" });
    expect(overviewBtn).toHaveClass("is-active");
    expect(impactBtn).not.toHaveClass("is-active");
    fireEvent.click(impactBtn);
    expect(impactBtn).toHaveClass("is-active");
    expect(overviewBtn).not.toHaveClass("is-active");
    // The classic view should now be visible with the Impact heading
    await waitFor(() => expect(screen.getByText("Dependency impact")).toBeInTheDocument());
  });

  it("toggles the color theme", () => {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue({
      ok: true,
      text: async () => JSON.stringify({ nodes: [], edges: [], total_nodes: 0, total_edges: 0, truncated: false }),
    }));
    render(<App />);
    expect(document.documentElement.dataset.theme).toBe("dark");
    fireEvent.click(screen.getByRole("button", { name: "Switch to light theme" }));
    expect(document.documentElement.dataset.theme).toBe("light");
    expect(screen.getByRole("button", { name: "Switch to dark theme" })).toBeInTheDocument();
  });
});

