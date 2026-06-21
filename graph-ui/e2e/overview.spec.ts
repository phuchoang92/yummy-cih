import { expect, test } from "@playwright/test";

const nodes = Array.from({ length: 80 }, (_, index) => {
  const angle = index * 0.73;
  return {
    index, id: `Method:demo.Service#method${index}/0`, kind: index % 9 === 0 ? "Route" : "Method",
    name: index % 9 === 0 ? `GET /orders/${index}` : `method${index}`, qualified_name: null,
    file: `src/${index % 5}/Service${index % 12}.java`, degree: 1 + (index * 7) % 60,
    x: Math.cos(angle) * (260 + index * 2), y: Math.sin(angle) * (260 + index * 2), z: ((index % 11) - 5) * 28,
    size: index % 9 === 0 ? 9 : 4, color: index % 7 === 0 ? "#c0d0ff" : "#ff8855",
  };
});
const edges = Array.from({ length: 150 }, (_, index) => ({ source: index % 80, target: (index * 7 + 11) % 80, kind: index % 4 === 0 ? "CALLS" : "IMPORTS" }));

test.beforeEach(async ({ page }) => {
  await page.route("**/api/graph/overview", (route) => route.fulfill({ json: { nodes, edges, total_nodes: 1880, total_edges: 5290, truncated: true } }));
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
