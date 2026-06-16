(function () {
  const state = {
    activeView: "search",
    selectedId: null,
    currentGraph: { nodes: [], links: [] },
    lastMermaid: "",
    lastOpenApi: null,
    simulation: null,
  };

  const els = {
    searchForm: document.getElementById("search-form"),
    searchInput: document.getElementById("search-input"),
    tabs: Array.from(document.querySelectorAll(".tab")),
    panels: Array.from(document.querySelectorAll("[data-panel]")),
    resultsTitle: document.getElementById("results-title"),
    resultCount: document.getElementById("result-count"),
    resultsList: document.getElementById("results-list"),
    activeViewLabel: document.getElementById("active-view-label"),
    graphTitle: document.getElementById("graph-title"),
    svg: document.getElementById("graph-svg"),
    emptyState: document.getElementById("empty-state"),
    statusPill: document.getElementById("status-pill"),
    graphCounts: document.getElementById("graph-counts"),
    selectedSummary: document.getElementById("selected-summary"),
    detailsJson: document.getElementById("details-json"),
    fitGraph: document.getElementById("fit-graph"),
    clearGraph: document.getElementById("clear-graph"),
    impactDirection: document.getElementById("impact-direction"),
    impactDepth: document.getElementById("impact-depth"),
    loadImpact: document.getElementById("load-impact"),
    flowDepth: document.getElementById("flow-depth"),
    loadFlow: document.getElementById("load-flow"),
    copyMermaid: document.getElementById("copy-mermaid"),
    loadCommunities: document.getElementById("load-communities"),
    routePrefix: document.getElementById("route-prefix"),
    loadRoutes: document.getElementById("load-routes"),
    copyOpenApi: document.getElementById("copy-openapi"),
  };

  function setStatus(label, tone) {
    els.statusPill.textContent = label;
    els.statusPill.classList.toggle("is-error", tone === "error");
    els.statusPill.classList.toggle(
      "is-busy",
      tone !== "error" && !["Ready", "No Context"].includes(label),
    );
  }

  function setDetails(value) {
    els.detailsJson.textContent = JSON.stringify(value || {}, null, 2);
  }

  async function fetchJson(url) {
    const res = await fetch(url, { headers: { Accept: "application/json" } });
    const text = await res.text();
    const payload = text ? JSON.parse(text) : {};
    if (!res.ok) {
      throw new Error(payload.error || `${res.status} ${res.statusText}`);
    }
    return payload;
  }

  function idOf(value) {
    if (!value) return "";
    if (typeof value === "string") return value;
    if (typeof value.id === "string") return value.id;
    if (typeof value.node_id === "string") return value.node_id;
    return String(value);
  }

  function labelOf(node) {
    return (
      node.label ||
      node.name ||
      node.qualified_name ||
      node.qualifiedName ||
      shortLabel(idOf(node)) ||
      "node"
    );
  }

  function kindOf(node) {
    if (!node) return "Node";
    if (typeof node.kind === "string") return node.kind;
    return "Node";
  }

  function shortLabel(id) {
    const raw = String(id || "");
    const byHash = raw.split("#").pop();
    return byHash.split(":").pop();
  }

  function truncate(text, max) {
    const raw = String(text || "");
    return raw.length > max ? `${raw.slice(0, max - 1)}...` : raw;
  }

  function normalizeGraph(input) {
    const graph = input || {};
    const rawNodes = graph.nodes || [];
    const rawLinks = graph.links || graph.edges || [];
    const nodeMap = new Map();

    rawNodes.forEach((node) => {
      const id = idOf(node);
      if (!id || nodeMap.has(id)) return;
      nodeMap.set(id, {
        id,
        label: truncate(labelOf(node), 34),
        kind: kindOf(node),
        file: node.file || "",
        depth: Number.isFinite(node.depth) ? node.depth : 0,
        raw: node,
      });
    });

    rawLinks.forEach((link) => {
      const source = idOf(link.source || link.src);
      const target = idOf(link.target || link.dst);
      if (!source || !target) return;
      if (!nodeMap.has(source)) {
        nodeMap.set(source, {
          id: source,
          label: truncate(shortLabel(source), 34),
          kind: "Node",
          file: "",
          depth: 0,
          raw: { id: source },
        });
      }
      if (!nodeMap.has(target)) {
        nodeMap.set(target, {
          id: target,
          label: truncate(shortLabel(target), 34),
          kind: "Node",
          file: "",
          depth: 0,
          raw: { id: target },
        });
      }
    });

    const nodes = Array.from(nodeMap.values());
    const links = rawLinks
      .map((link) => ({
        source: idOf(link.source || link.src),
        target: idOf(link.target || link.dst),
        label: link.label || link.kind || link.via || "",
      }))
      .filter((link) => link.source && link.target);

    return { nodes, links };
  }

  function renderResults(items, options = {}) {
    els.resultsList.innerHTML = "";
    els.resultCount.textContent = String(items.length);
    els.resultsTitle.textContent = options.title || "Results";

    if (!items.length) {
      const empty = document.createElement("div");
      empty.className = "selected-summary";
      empty.textContent = options.empty || "No results.";
      els.resultsList.appendChild(empty);
      return;
    }

    items.forEach((item) => {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "result-item";
      const id = item.handler_id || item.node_id || item.id || "";
      button.dataset.id = id;
      if (id && id === state.selectedId) button.classList.add("is-active");

      if (item.path && item.http_method) {
        button.innerHTML = `
          <div class="route-row">
            <span class="http-method">${escapeHtml(item.http_method)}</span>
            <div>
              <div class="result-title"><strong>${escapeHtml(item.path)}</strong></div>
              <div class="result-meta">${escapeHtml(item.handler_qualified || item.handler_name || "")}</div>
            </div>
          </div>`;
      } else {
        button.innerHTML = `
          <div class="result-title">
            <span class="kind">${escapeHtml(kindOf(item))}</span>
            <strong>${escapeHtml(item.name || shortLabel(id))}</strong>
          </div>
          <div class="result-meta">${escapeHtml(item.qualified_name || item.file || id)}</div>
          ${renderResultFooter(item)}`;
      }

      button.addEventListener("click", () => {
        const nextId = item.handler_id || item.node_id || item.id;
        if (nextId) selectNode(nextId, item);
      });
      els.resultsList.appendChild(button);
    });
  }

  function renderResultFooter(item) {
    const rank = item.rank ? `#${item.rank}` : "";
    const score = Number.isFinite(item.score) ? item.score.toFixed(4) : "";
    const sources = Array.isArray(item.sources) ? item.sources.join(" + ") : "";
    if (!rank && !score && !sources) return "";
    return `
      <div class="result-footer">
        <span>${escapeHtml([rank, score].filter(Boolean).join(" / "))}</span>
        ${sources ? `<span class="source-pill">${escapeHtml(sources)}</span>` : "<span></span>"}
      </div>`;
  }

  function escapeHtml(value) {
    return String(value || "")
      .replaceAll("&", "&amp;")
      .replaceAll("<", "&lt;")
      .replaceAll(">", "&gt;")
      .replaceAll('"', "&quot;");
  }

  async function runSearch(event) {
    if (event) event.preventDefault();
    const q = els.searchInput.value.trim();
    if (!q) return;
    setStatus("Searching");
    try {
      const data = await fetchJson(`/api/graph/search?q=${encodeURIComponent(q)}&limit=20`);
      renderResults(data.hits || [], {
        title: "Search Results",
        empty: "No symbols matched this query.",
      });
      if (data.subgraph) {
        drawGraph(normalizeGraph(data.subgraph), `Search: ${q}`);
      }
      setDetails(data);
      setStatus("Ready");
    } catch (err) {
      showError(err);
    }
  }

  async function selectNode(id, raw) {
    state.selectedId = idOf(id);
    highlightSelected();
    updateSelectedSummary(raw || { id: state.selectedId });
    setStatus("Loading");
    try {
      const data = await fetchJson(`/api/graph/context?id=${encodeURIComponent(state.selectedId)}`);
      updateSelectedSummary(data.node || raw || { id: state.selectedId });
      setDetails(data);
      setStatus("Ready");
    } catch (err) {
      setDetails(raw || { id: state.selectedId });
      setStatus(err.message.includes("not found") ? "No Context" : "Ready");
    }
  }

  function updateSelectedSummary(node) {
    const id = idOf(node);
    const name = node.name || node.label || shortLabel(id);
    const kind = kindOf(node);
    const file = node.file || "";
    const qualified = node.qualified_name || node.qualifiedName || "";
    els.selectedSummary.innerHTML = `
      <h3>${escapeHtml(name || "Selected node")}</h3>
      <div class="summary-row">
        <span class="summary-chip">${escapeHtml(kind)}</span>
        ${qualified ? `<span class="summary-chip">qualified</span>` : ""}
      </div>
      <p>${escapeHtml(id)}</p>
      ${qualified ? `<p>${escapeHtml(qualified)}</p>` : ""}
      ${file ? `<p>${escapeHtml(file)}</p>` : ""}`;
  }

  async function loadImpact() {
    if (!requireSelection()) return;
    setActiveView("impact");
    setStatus("Loading Impact");
    try {
      const direction = els.impactDirection.value;
      const depth = clampNumber(els.impactDepth.value, 1, 8, 4);
      const data = await fetchJson(
        `/api/graph/impact?id=${encodeURIComponent(state.selectedId)}&direction=${direction}&depth=${depth}`,
      );
      drawGraph(normalizeGraph(data), `Impact: ${shortLabel(state.selectedId)}`);
      setDetails(data);
      setStatus("Ready");
    } catch (err) {
      showError(err);
    }
  }

  async function loadFlow() {
    if (!requireSelection()) return;
    setActiveView("flow");
    setStatus("Tracing Flow");
    try {
      const depth = clampNumber(els.flowDepth.value, 1, 10, 6);
      const data = await fetchJson(
        `/api/graph/flow?id=${encodeURIComponent(state.selectedId)}&depth=${depth}`,
      );
      state.lastMermaid = data.mermaid || "";
      drawGraph(normalizeGraph(data), `Flow: ${shortLabel(state.selectedId)}`);
      setDetails(data);
      setStatus("Ready");
    } catch (err) {
      showError(err);
    }
  }

  async function loadCommunities() {
    setActiveView("communities");
    setStatus("Loading Communities");
    try {
      const data = await fetchJson("/api/graph/communities");
      drawGraph(normalizeGraph(data), "Community Map");
      renderResults((data.nodes || []).map((node) => ({ ...node, kind: "Community" })), {
        title: "Communities",
        empty: "No community graph found. Run cih-engine discover first.",
      });
      setDetails(data);
      setStatus("Ready");
    } catch (err) {
      showError(err);
    }
  }

  async function loadRoutes() {
    setActiveView("routes");
    setStatus("Loading Routes");
    try {
      const prefix = els.routePrefix.value.trim();
      const data = await fetchJson(
        `/api/graph/routes?limit=500${prefix ? `&prefix=${encodeURIComponent(prefix)}` : ""}`,
      );
      state.lastOpenApi = data.openapi || null;
      renderResults(data.routes || [], {
        title: "Routes",
        empty: "No routes matched this prefix.",
      });
      setDetails(data);
      setStatus("Ready");
    } catch (err) {
      showError(err);
    }
  }

  function requireSelection() {
    if (state.selectedId) return true;
    setStatus("Select a node", "error");
    els.selectedSummary.innerHTML =
      "<p>Select a search result, route handler, or graph node before loading this view.</p>";
    return false;
  }

  function showError(err) {
    setStatus("Error", "error");
    setDetails({ error: err.message });
    els.selectedSummary.innerHTML = `<p>${escapeHtml(err.message)}</p>`;
  }

  function clampNumber(value, min, max, fallback) {
    const parsed = Number.parseInt(value, 10);
    if (!Number.isFinite(parsed)) return fallback;
    return Math.max(min, Math.min(max, parsed));
  }

  function setActiveView(view) {
    state.activeView = view;
    els.tabs.forEach((tab) => tab.classList.toggle("is-active", tab.dataset.view === view));
    els.panels.forEach((panel) => panel.classList.toggle("is-hidden", panel.dataset.panel !== view));
    const label = view.charAt(0).toUpperCase() + view.slice(1);
    els.activeViewLabel.textContent = label;
  }

  function clearGraph() {
    state.currentGraph = { nodes: [], links: [] };
    state.selectedId = null;
    state.lastMermaid = "";
    drawGraph(state.currentGraph, "Search the indexed graph");
    renderResults([], { title: "Results", empty: "No results." });
    updateSelectedSummary({});
    setDetails({});
  }

  function fitGraph() {
    drawGraph(state.currentGraph, els.graphTitle.textContent || "Graph");
  }

  function drawGraph(graph, title) {
    state.currentGraph = graph || { nodes: [], links: [] };
    els.graphTitle.textContent = title || "Graph";
    if (els.graphCounts) {
      els.graphCounts.textContent = `${state.currentGraph.nodes.length} nodes / ${state.currentGraph.links.length} links`;
    }
    els.emptyState.style.display = state.currentGraph.nodes.length ? "none" : "block";
    els.svg.innerHTML = "";
    if (!state.currentGraph.nodes.length) return;

    const width = els.svg.clientWidth || 900;
    const height = els.svg.clientHeight || 640;
    const nodes = state.currentGraph.nodes.map((node, index) => {
      const angle = (Math.PI * 2 * index) / Math.max(1, state.currentGraph.nodes.length);
      return {
        ...node,
        x: width / 2 + Math.cos(angle) * Math.min(width, height) * 0.25,
        y: height / 2 + Math.sin(angle) * Math.min(width, height) * 0.25,
        vx: 0,
        vy: 0,
      };
    });
    const byId = new Map(nodes.map((node) => [node.id, node]));
    const links = state.currentGraph.links
      .map((link) => ({
        ...link,
        sourceNode: byId.get(link.source),
        targetNode: byId.get(link.target),
      }))
      .filter((link) => link.sourceNode && link.targetNode);

    const linkLayer = svgEl("g", { class: "links" });
    const labelLayer = svgEl("g", { class: "link-labels" });
    const nodeLayer = svgEl("g", { class: "nodes" });
    els.svg.append(arrowDefs(), linkLayer, labelLayer, nodeLayer);

    const linkEls = links.map((link) => {
      const line = svgEl("line", { class: "graph-link" });
      linkLayer.appendChild(line);
      return { link, line };
    });

    const linkLabelEls = links.slice(0, 80).map((link) => {
      const text = svgEl("text", { class: "graph-link-label" });
      text.textContent = truncate(link.label, 18);
      labelLayer.appendChild(text);
      return { link, text };
    });

    const nodeEls = nodes.map((node) => {
      const group = svgEl("g", {
        class: `graph-node ${kindClass(node.kind)}`,
        tabindex: "0",
        "data-id": node.id,
      });
      const radius = radiusFor(node);
      const circle = svgEl("circle", { r: String(radius) });
      const text = svgEl("text", { y: String(radius + 14) });
      text.textContent = truncate(node.label, 22);
      const titleEl = svgEl("title");
      titleEl.textContent = `${node.kind}: ${node.id}`;
      group.append(circle, text, titleEl);
      group.addEventListener("click", () => selectNode(node.id, node.raw || node));
      group.addEventListener("keydown", (event) => {
        if (event.key === "Enter" || event.key === " ") selectNode(node.id, node.raw || node);
      });
      nodeLayer.appendChild(group);
      return { node, group };
    });

    let tick = 0;
    if (state.simulation) cancelAnimationFrame(state.simulation);

    function step() {
      tick += 1;
      applyForces(nodes, links, width, height);
      renderFrame(nodeEls, linkEls, linkLabelEls);
      highlightSelected();
      if (tick < 260) state.simulation = requestAnimationFrame(step);
    }

    step();
  }

  function applyForces(nodes, links, width, height) {
    const centerX = width / 2;
    const centerY = height / 2;
    const charge = Math.min(4600, Math.max(900, nodes.length * 44));

    for (let i = 0; i < nodes.length; i += 1) {
      for (let j = i + 1; j < nodes.length; j += 1) {
        const a = nodes[i];
        const b = nodes[j];
        let dx = a.x - b.x;
        let dy = a.y - b.y;
        let dist2 = dx * dx + dy * dy;
        if (dist2 < 0.01) {
          dx = 0.5;
          dy = 0.5;
          dist2 = 0.5;
        }
        const force = charge / dist2;
        const dist = Math.sqrt(dist2);
        const fx = (dx / dist) * force;
        const fy = (dy / dist) * force;
        a.vx += fx;
        a.vy += fy;
        b.vx -= fx;
        b.vy -= fy;
      }
    }

    links.forEach((link) => {
      const a = link.sourceNode;
      const b = link.targetNode;
      const dx = b.x - a.x;
      const dy = b.y - a.y;
      const dist = Math.max(1, Math.sqrt(dx * dx + dy * dy));
      const desired = 116;
      const force = (dist - desired) * 0.012;
      const fx = (dx / dist) * force;
      const fy = (dy / dist) * force;
      a.vx += fx;
      a.vy += fy;
      b.vx -= fx;
      b.vy -= fy;
    });

    nodes.forEach((node) => {
      node.vx += (centerX - node.x) * 0.004;
      node.vy += (centerY - node.y) * 0.004;
      node.vx *= 0.84;
      node.vy *= 0.84;
      node.x = Math.max(36, Math.min(width - 36, node.x + node.vx));
      node.y = Math.max(36, Math.min(height - 36, node.y + node.vy));
    });
  }

  function renderFrame(nodeEls, linkEls, labelEls) {
    linkEls.forEach(({ link, line }) => {
      line.setAttribute("x1", link.sourceNode.x);
      line.setAttribute("y1", link.sourceNode.y);
      line.setAttribute("x2", link.targetNode.x);
      line.setAttribute("y2", link.targetNode.y);
    });
    labelEls.forEach(({ link, text }) => {
      text.setAttribute("x", (link.sourceNode.x + link.targetNode.x) / 2);
      text.setAttribute("y", (link.sourceNode.y + link.targetNode.y) / 2);
    });
    nodeEls.forEach(({ node, group }) => {
      group.setAttribute("transform", `translate(${node.x},${node.y})`);
      group.classList.toggle("is-selected", node.id === state.selectedId);
    });
  }

  function highlightSelected() {
    Array.from(els.svg.querySelectorAll(".graph-node")).forEach((group) => {
      group.classList.toggle("is-selected", group.dataset.id === state.selectedId);
    });
    Array.from(document.querySelectorAll(".result-item")).forEach((item) => {
      item.classList.toggle("is-active", item.dataset.id === state.selectedId);
    });
  }

  function radiusFor(node) {
    switch (node.kind) {
      case "Route":
      case "ExternalEndpoint":
        return 18;
      case "Community":
        return 22;
      case "Class":
      case "Interface":
        return 17;
      default:
        return 14;
    }
  }

  function kindClass(kind) {
    const normalized = String(kind || "node")
      .toLowerCase()
      .replace(/[^a-z0-9]+/g, "");
    return `kind-${normalized || "node"}`;
  }

  function arrowDefs() {
    const defs = svgEl("defs");
    const marker = svgEl("marker", {
      id: "arrow",
      viewBox: "0 0 10 10",
      refX: "9",
      refY: "5",
      markerWidth: "6",
      markerHeight: "6",
      orient: "auto-start-reverse",
    });
    const path = svgEl("path", {
      d: "M 0 0 L 10 5 L 0 10 z",
      fill: "#4b5565",
    });
    marker.appendChild(path);
    defs.appendChild(marker);
    return defs;
  }

  function svgEl(name, attrs = {}) {
    const el = document.createElementNS("http://www.w3.org/2000/svg", name);
    Object.entries(attrs).forEach(([key, value]) => el.setAttribute(key, value));
    return el;
  }

  async function copyText(label, value) {
    if (!value) {
      setStatus(`No ${label}`, "error");
      return;
    }
    try {
      await navigator.clipboard.writeText(value);
      setStatus(`${label} Copied`);
    } catch (_err) {
      setDetails({ [label]: value });
      setStatus("Copy Blocked");
    }
  }

  els.searchForm.addEventListener("submit", runSearch);
  els.tabs.forEach((tab) =>
    tab.addEventListener("click", () => {
      setActiveView(tab.dataset.view);
      if (tab.dataset.view === "communities") loadCommunities();
      if (tab.dataset.view === "routes") loadRoutes();
    }),
  );
  els.loadImpact.addEventListener("click", loadImpact);
  els.loadFlow.addEventListener("click", loadFlow);
  els.copyMermaid.addEventListener("click", () => copyText("Mermaid", state.lastMermaid));
  els.loadCommunities.addEventListener("click", loadCommunities);
  els.loadRoutes.addEventListener("click", loadRoutes);
  els.copyOpenApi.addEventListener("click", () =>
    copyText("OpenAPI", state.lastOpenApi ? JSON.stringify(state.lastOpenApi, null, 2) : ""),
  );
  els.fitGraph.addEventListener("click", fitGraph);
  els.clearGraph.addEventListener("click", clearGraph);
  window.addEventListener("resize", fitGraph);

  setActiveView("search");
  drawGraph(state.currentGraph, "Search the indexed graph");
})();
