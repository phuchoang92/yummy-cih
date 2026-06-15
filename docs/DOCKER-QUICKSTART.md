# CIH Docker Quickstart

Pull the pre-built image and build a code-intelligence graph for any Java/Spring codebase.

---

## Prerequisites

- [Docker Desktop](https://www.docker.com/products/docker-desktop/) installed and running
- Your Java/Spring source code available on disk

---

## Step 1 — Create a workspace folder

Create a new folder anywhere on your machine. All commands below are run from inside it.

**Windows:**
```powershell
mkdir C:\cih-workspace
cd C:\cih-workspace
```

**Mac / Linux:**
```bash
mkdir ~/cih-workspace
cd ~/cih-workspace
```

---

## Step 2 — Create `docker-compose.yml`

Save this file inside the workspace folder:

```yaml
services:
  falkordb:
    image: falkordb/falkordb:latest
    ports:
      - "6380:6379"
    volumes:
      - falkordb-data:/data
    restart: unless-stopped
    healthcheck:
      test: ["CMD", "redis-cli", "-p", "6379", "ping"]
      interval: 5s
      timeout: 3s
      retries: 10

  cih-server:
    image: phuchoang29/yummy-cih:latest
    ports:
      - "8080:8080"
    environment:
      FALKOR_URL: redis://falkordb:6379
      CIH_GRAPH_KEY: cih
      CIH_BIND: 0.0.0.0:8080
      CIH_ARTIFACTS_DIR: /data/artifacts
      HF_HOME: /data/hf-cache
      RUST_LOG: info
    volumes:
      - cih-data:/data
    depends_on:
      falkordb:
        condition: service_healthy
    restart: unless-stopped

volumes:
  falkordb-data:
  cih-data:
```

---

## Step 3 — Pull the image and start services

```bash
docker compose pull
docker compose up -d
```

Wait about 10 seconds, then confirm both containers are running:

```bash
docker compose ps
```

Expected output — both services should show `running`:

```
NAME                    STATUS
cih-workspace-falkordb-1    running
cih-workspace-cih-server-1  running
```

---

## Step 4 — Find your Docker network name

The indexing container in the next step needs to join the same network as FalkorDB.

```bash
docker network ls
```

Look for a network named `<your-folder-name>_default`. If your folder is `cih-workspace`, the network is `cih-workspace_default`.

---

## Step 5 — Index your source code

Run `cih-engine analyze` as a one-shot container. Replace the highlighted values:

- `<NETWORK>` → network name from Step 4 (e.g. `cih-workspace_default`)
- `<PATH_TO_SOURCE>` → absolute path to your Java/Spring project on disk

**Windows (PowerShell):**
```powershell
docker run --rm `
  --network <NETWORK> `
  -v "<PATH_TO_SOURCE>:/repo" `
  -v cih-workspace_cih-data:/data `
  -e FALKOR_URL=redis://falkordb:6379 `
  -e CIH_GRAPH_KEY=cih `
  -e CIH_ARTIFACTS_DIR=/data/artifacts `
  phuchoang29/yummy-cih:latest `
  cih-engine analyze /repo --all
```

**Mac / Linux:**
```bash
docker run --rm \
  --network <NETWORK> \
  -v "<PATH_TO_SOURCE>:/repo" \
  -v cih-workspace_cih-data:/data \
  -e FALKOR_URL=redis://falkordb:6379 \
  -e CIH_GRAPH_KEY=cih \
  -e CIH_ARTIFACTS_DIR=/data/artifacts \
  phuchoang29/yummy-cih:latest \
  cih-engine analyze /repo --all
```

**Example (Windows):**
```powershell
docker run --rm `
  --network cih-workspace_default `
  -v "C:\projects\payment-service:/repo" `
  -v cih-workspace_cih-data:/data `
  -e FALKOR_URL=redis://falkordb:6379 `
  -e CIH_GRAPH_KEY=cih `
  -e CIH_ARTIFACTS_DIR=/data/artifacts `
  phuchoang29/yummy-cih:latest `
  cih-engine analyze /repo --all
```

This step takes **2–15 minutes** depending on codebase size. Progress is printed to the terminal:

```
[INFO] parsed 450 files
[INFO] resolved 48 230 edges
[INFO] loaded 12 000 nodes → FalkorDB
```

---

## Step 6 — Run community detection *(optional but recommended)*

Detects module clusters and business process traces. Enables the `communities` MCP tool
and `cih://repo/.../processes` resource.

**Windows (PowerShell):**
```powershell
docker run --rm `
  --network <NETWORK> `
  -v "<PATH_TO_SOURCE>:/repo" `
  -v cih-workspace_cih-data:/data `
  -e FALKOR_URL=redis://falkordb:6379 `
  -e CIH_GRAPH_KEY=cih `
  -e CIH_ARTIFACTS_DIR=/data/artifacts `
  phuchoang29/yummy-cih:latest `
  cih-engine discover /repo
```

**Mac / Linux:**
```bash
docker run --rm \
  --network <NETWORK> \
  -v "<PATH_TO_SOURCE>:/repo" \
  -v cih-workspace_cih-data:/data \
  -e FALKOR_URL=redis://falkordb:6379 \
  -e CIH_GRAPH_KEY=cih \
  -e CIH_ARTIFACTS_DIR=/data/artifacts \
  phuchoang29/yummy-cih:latest \
  cih-engine discover /repo
```

---

## Step 7 — Verify the graph is ready

```bash
docker run --rm \
  --network <NETWORK> \
  -v cih-workspace_cih-data:/data \
  -e FALKOR_URL=redis://falkordb:6379 \
  -e CIH_ARTIFACTS_DIR=/data/artifacts \
  phuchoang29/yummy-cih:latest \
  cih-engine list
```

Expected output:

```
name              indexed_at     nodes    edges  files  path
------------------------------------------------------------------------------
payment-service   2026-06-15     12345    48230    450  /repo
```

---

## Step 8 — Connect to the MCP server

The MCP server is running at `http://localhost:8080/mcp`.

### Test with curl

```bash
curl -s -X POST http://localhost:8080/mcp \
  -H "Content-Type: application/json" \
  -d '{"jsonrpc":"2.0","id":1,"method":"tools/list","params":{}}'
```

### Connect from Claude Desktop

Add this to your `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "cih": {
      "url": "http://localhost:8080/mcp"
    }
  }
}
```

Restart Claude Desktop. You can now ask Claude questions like:
- *"What HTTP endpoints does this service expose?"*
- *"What is the blast radius of changing OrderService?"*
- *"Show me the module breakdown of this codebase"*

---

## Stopping and restarting

**Stop (data is preserved):**
```bash
docker compose down
```

**Restart later — graph is already loaded, no re-indexing needed:**
```bash
docker compose up -d
```

---

## Re-indexing after code changes

Re-run Step 5. `cih-engine analyze` is incremental — it only re-parses files that changed
since the last run.

---

## Troubleshooting

| Problem | Fix |
|---------|-----|
| `network not found` | Run `docker network ls` and use the exact network name shown |
| No Java files found | Check that `<PATH_TO_SOURCE>` points to the project root; confirm `--all` flag is present |
| MCP server connection refused | Run `docker compose ps` — `cih-server` may still be starting up; wait 10s and retry |
| Graph empty after analyze | Check analyze logs for errors; confirm FalkorDB is healthy with `docker compose ps` |
| Windows path errors | Use double quotes around the `-v` path and forward slashes: `-v "C:/projects/app:/repo"` |
| `cih-engine list` shows 0 nodes | Community detection (`discover`) has not run; go back to Step 6 |
