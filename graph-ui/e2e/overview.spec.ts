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
});
