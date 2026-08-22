import { expect, test } from "@playwright/test";

const nodes = Array.from({ length: 80 }, (_, index) => {
  const angle = index * 0.73;
  return {
    index, id: `Method:demo.Service#method${index}/0`, kind: index % 9 === 0 ? "Route" : "Method",
    name: index % 9 === 0 ? `GET /orders/${index}` : `method${index}`, qualified_name: null,
    file: `src/${index % 5}/Service${index % 12}.java`, degree: 1 + (index * 7) % 60,
    x: Math.cos(angle) * (260 + index * 2), y: Math.sin(angle) * (260 + index * 2), z: ((index % 11) - 5) * 28,
    role: index % 9 === 0 ? "aggregate" : "entity", member_count: index % 9 === 0 ? 30 + index : 1,
    expandable: true, size: index % 9 === 0 ? 9 : 4,
  };
});
const edges = Array.from({ length: 150 }, (_, index) => ({ source: index % 80, target: (index * 7 + 11) % 80, kind: index % 4 === 0 ? "CALLS" : "IMPORTS", count: 1 + index % 5 }));
const hopRoot = {
  scope: "repository",
  nodes: [{ index: 0, id: "Community:Architecture", kind: "Community", name: "Architecture", role: "aggregate", member_count: 20, expandable: true, degree: 4, x: 0, y: 0, size: 8 }],
  edges: [], total_nodes: 1, total_edges: 0, truncated: false,
};
const hopChild = {
  scope: "community", parent_id: "Community:Architecture",
  nodes: [{ index: 0, id: "File:src/App.ts", kind: "File", name: "App.ts", role: "aggregate", member_count: 8, expandable: true, degree: 1, x: 0, y: 0, size: 7 }],
  edges: [], total_nodes: 1, total_edges: 0, truncated: false,
};

test.beforeEach(async ({ page }) => {
  await page.route("**/api/graph/projection?*", (route) => route.fulfill({ json: { scope: "repository", nodes, edges, total_nodes: 1880, total_edges: 5290, truncated: true } }));
  await page.route("**/api/graph/context**", (route) => route.fulfill({ json: { node: { id: nodes[0].id, kind: "Route", name: nodes[0].name, file: nodes[0].file }, callers: [], callees: [], processes: [] } }));
});

test("stellar overview desktop", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByText("1,880")).toBeVisible();
  await expect(page).toHaveScreenshot("overview-desktop.png", { animations: "disabled", maxDiffPixelRatio: 0.02 });
});

test("stellar overview narrow", async ({ page }) => {
  await page.setViewportSize({ width: 900, height: 760 });
  await page.goto("/");
  await expect(page.getByText("1,880")).toBeVisible();
  await expect(page).toHaveScreenshot("overview-narrow.png", { animations: "disabled", maxDiffPixelRatio: 0.02 });
});

test("hop-in keeps the old canvas visible and Back restores without refetch", async ({ page }) => {
  await page.unroute("**/api/graph/projection?*");
  let releaseChild!: () => void;
  const childGate = new Promise<void>((resolve) => { releaseChild = resolve; });
  let rootRequests = 0;
  await page.route("**/api/graph/projection?*", async (route) => {
    const scope = new URL(route.request().url()).searchParams.get("scope");
    if (scope === "community") {
      await childGate;
      await route.fulfill({ json: hopChild });
    } else {
      rootRequests += 1;
      await route.fulfill({ json: hopRoot });
    }
  });
  await page.goto("/");
  const canvas = page.getByLabel("Code graph canvas");
  await expect(canvas).toHaveAttribute("data-projection-key", "repository:root");

  await page.getByRole("button", { name: "Architecture aggregate 20" }).dblclick();
  await expect(page.getByRole("status")).toHaveText("Opening Architecture…");
  await expect(canvas).toHaveAttribute("data-projection-key", "repository:root");
  await expect(canvas).toBeVisible();
  releaseChild();

  await expect(page.getByText("App.ts")).toBeVisible();
  await expect(canvas).toHaveAttribute("data-projection-key", "community:Community:Architecture");
  await expect(page).toHaveScreenshot("overview-community.png", { animations: "disabled", maxDiffPixelRatio: 0.02 });
  const requestsBeforeBack = rootRequests;
  await page.getByRole("button", { name: "Back to Repository" }).click();
  await expect(canvas).toHaveAttribute("data-projection-key", "repository:root");
  await expect(page.getByText("Architecture")).toBeVisible();
  expect(rootRequests).toBe(requestsBeforeBack);
});

test("reduced motion removes hop transform while preserving loading continuity", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.unroute("**/api/graph/projection?*");
  await page.route("**/api/graph/projection?*", (route) => {
    const scope = new URL(route.request().url()).searchParams.get("scope");
    return route.fulfill({ json: scope === "community" ? hopChild : hopRoot });
  });
  await page.goto("/");
  await page.getByRole("button", { name: "Architecture aggregate 20" }).dblclick();
  await expect(page.getByText("App.ts")).toBeVisible();
  const animationName = await page.locator(".overview-shell").evaluate((element) => getComputedStyle(element).animationName);
  expect(animationName).toBe("none");
});

test("Fancy mode persists and supports drag-to-pin physics", async ({ page }) => {
  const pageErrors: string[] = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));
  await page.goto("/");
  const canvas = page.getByLabel("Code graph canvas");
  await expect(canvas).toHaveAttribute("data-graph-mode", "performance");
  await page.getByRole("button", { name: "Fancy" }).click();
  await expect(canvas).toHaveAttribute("data-graph-mode", "fancy");
  await page.waitForTimeout(650);

  const point = await canvas.evaluate((element) => {
    const graph = element as HTMLCanvasElement;
    const context = graph.getContext("2d", { willReadFrequently: true });
    if (!context) return null;
    const pixels = context.getImageData(0, 0, graph.width, graph.height).data;
    let best = { x: 0, y: 0, score: -1 };
    for (let y = 3; y < graph.height - 3; y += 2) {
      for (let x = 3; x < graph.width - 3; x += 2) {
        const offset = (y * graph.width + x) * 4;
        const r = pixels[offset]; const g = pixels[offset + 1]; const b = pixels[offset + 2];
        const score = (Math.max(r, g, b) - Math.min(r, g, b)) * 2 + Math.max(r, g, b);
        if (score > best.score) best = { x, y, score };
      }
    }
    return { x: best.x * graph.clientWidth / Math.max(1, graph.width), y: best.y * graph.clientHeight / Math.max(1, graph.height), score: best.score, width: graph.width, height: graph.height };
  });
  expect(pageErrors, JSON.stringify(point)).toEqual([]);
  expect(point.score, JSON.stringify(point)).toBeGreaterThan(0);
  const box = await canvas.boundingBox();
  expect(box).not.toBeNull();
  await page.mouse.move(box!.x + point.x, box!.y + point.y);
  await expect(page.getByText("Drag to move · Shift-drag to pin")).toBeVisible();
  await page.keyboard.down("Shift");
  await page.mouse.down();
  await page.mouse.move(box!.x + point.x + 36, box!.y + point.y + 22, { steps: 4 });
  await page.mouse.up();
  await page.keyboard.up("Shift");
  await expect(page.getByText(/· pinned$/)).toBeVisible();

  await page.reload();
  await expect(page.getByLabel("Code graph canvas")).toHaveAttribute("data-graph-mode", "fancy");
});

test("maximum visible projection paints without main-thread layout", async ({ page }) => {
  const largeNodes = Array.from({ length: 10_000 }, (_, index) => ({
    index, id: `Function:large_${index}`, kind: "Function", name: `large_${index}`,
    role: "entity", member_count: 1, degree: 10, expandable: true,
    x: Math.cos(index * .21) * Math.sqrt(index + 1) * 12,
    y: Math.sin(index * .21) * Math.sqrt(index + 1) * 12, size: 3.2,
  }));
  const largeEdges = Array.from({ length: 50_000 }, (_, index) => ({
    source: index % largeNodes.length,
    target: (index * 17 + 23) % largeNodes.length,
    kind: "CALLS",
    count: 1,
  }));
  await page.unroute("**/api/graph/projection?*");
  await page.route("**/api/graph/projection?*", (route) => route.fulfill({
    json: { scope: "repository", nodes: largeNodes, edges: largeEdges, total_nodes: 400_000, total_edges: 1_200_000, truncated: true },
  }));
  const started = Date.now();
  await page.goto("/");
  await expect(page.getByLabel("Code graph canvas")).toBeVisible({ timeout: 5_000 });
  expect(Date.now() - started).toBeLessThan(5_000);
  await expect(page.getByText("10,000 nodes")).toBeVisible();
  await page.getByRole("button", { name: "Fancy" }).click();
  await expect(page.getByLabel("Code graph canvas")).toHaveAttribute("data-graph-mode", "fancy");
  const box = await page.getByLabel("Code graph canvas").boundingBox();
  await page.mouse.move(box!.x + box!.width / 2, box!.y + box!.height / 2);
  await expect(page.getByLabel("Code graph canvas")).toBeVisible();
});
