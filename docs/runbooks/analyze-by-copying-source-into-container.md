# Runbook: Analyze a repo by copying source **into** a container (Windows), then serve MCP

Companion to [`embed-feature-clustering.md`](./embed-feature-clustering.md). Use this when you must
index a repo **inside** the container instead of bind-mounting it — the typical case being a Windows
Docker Desktop laptop where bind-mounts are slow.

## Why copy instead of bind-mount

The normal flow bind-mounts the source (`${REPO_PATH}:/repo`). On Windows, Docker Desktop runs
containers in a Linux VM, so a bind-mounted host dir crosses a slow filesystem-sharing layer —
reading thousands of files and writing `.cih/` artifacts back is painfully slow on a large repo.

Instead: **copy the source into a Docker-native named volume** (fast overlay I/O), run the whole
pipeline inside the container, keep results there, and point the MCP server at the same volume.
Nothing large crosses the host boundary except a one-time `docker cp`.

## Architecture

```
            docker network  yummy-cih_default   (created by `docker compose up`)
   ┌───────────────┬──────────────┬───────────────────────────┬─────────────────┐
   │ falkordb      │ postgres     │ cih-box (toolbox)         │ cih-server      │
   │ :6379 (graph) │ :5432 (pgvec)│ entrypoint: sleep         │ CMD: cih-server │
   │               │              │ runs analyze/embed/       │ serves :8080    │
   │               │              │ discover via docker exec  │ /mcp            │
   └───────────────┴──────────────┴───────────────────────────┴─────────────────┘
```

`cih-box` and `cih-server` share **two** named volumes:
- **`cih-repo`** at `/repo` — copied source **and** the `.cih/` artifacts the engine writes.
- **`cih-home`** at `/home/cih/.cih` — the **registry** (`registry.json`). `analyze` records
  `path=/repo, graph_key=cih` here; the server resolves the repo root from it. Without it shared, the
  server errors *"no repos in registry"*.

### How the server retrieves source (why both volumes are needed)

Source-returning MCP tools (`read_file`, snippets in `taint_paths`) read **bytes off disk**, not from
FalkorDB/Postgres — the graph stores only paths + line ranges (`cih-server/src/files.rs`,
`symbol.rs:66`):

1. `read_file` → `find_repo_path(graph_key="cih")` → `~/.cih/registry.json` → `repo_root = /repo`.
2. Reads `/repo/<relative-path>` off disk, size-capped.

Both containers mount `cih-repo` at the same `/repo`, so the absolute path in the registry resolves
identically — which is why copying into a volume works exactly like a bind mount for `read_file`.

---

## Prerequisites

- Docker Desktop on Windows; the project `docker-compose.yml` + `.env` with `POSTGRES_PASSWORD=...`.
- Target Java/Spring source on disk, e.g. `C:\projects\payment-service`.
- Commands are **PowerShell** (backtick continuation); Mac/Linux use `\`.

```powershell
$env:PGPW = (Select-String -Path .env -Pattern '^POSTGRES_PASSWORD=').Line -replace '^POSTGRES_PASSWORD=',''
$SRC      = "C:\projects\payment-service"   # <-- your source repo
$IMG      = "phuchoang29/yummy-cih:latest"
```

---

## Steps

### 1. Start infra (also creates the network + volumes)
```powershell
docker compose up -d falkordb postgres
docker network ls | Select-String default        # confirm the network name
$NET = "yummy-cih_default"                         # <-- set to what the line above shows
```

### 2. Create the shared repo volume
```powershell
docker volume create cih-repo
```

### 3. Start the long-lived "toolbox" container
```powershell
docker run -d --name cih-box `
  --network $NET `
  -e FALKOR_URL=redis://falkordb:6379 `
  -e CIH_GRAPH_KEY=cih `
  -e CIH_PG_URL="postgres://cih:$($env:PGPW)@postgres:5432/cih" `
  -e HF_HOME=/data/hf-cache `
  -v cih-repo:/repo `
  -v cih-data:/data `
  -v cih-home:/home/cih/.cih `
  --entrypoint sleep `
  $IMG infinity
```

### 4. Copy the source into the volume
```powershell
docker cp "$SRC\." cih-box:/repo
docker exec -u 0 cih-box chown -R cih:cih /repo    # engine runs as uid 1001
docker exec cih-box sh -c "ls /repo | head"        # sanity check
```

### 5. Run the pipeline inside the container
```powershell
docker exec cih-box cih-engine analyze /repo --all                    # graph → FalkorDB + .cih/artifacts
docker exec cih-box cih-engine embed /repo                            # vectors → Postgres (1st run downloads model)
docker exec cih-box cih-engine discover /repo --feature-strategy embed # communities + embed feature groups
```
`CIH_PG_URL`/`FALKOR_URL` come from the container env. `discover` loads FalkorDB by default (do **not**
pass `--no-load`) so the server can query the graph.

### 6. Start the MCP server against the same volumes
```powershell
docker run -d --name cih-server `
  --network $NET `
  -p 8080:8080 `
  -e FALKOR_URL=redis://falkordb:6379 `
  -e CIH_GRAPH_KEY=cih `
  -e CIH_BIND=0.0.0.0:8080 `
  -e CIH_ARTIFACTS_DIR=/repo/.cih/artifacts `
  -e CIH_PG_URL="postgres://cih:$($env:PGPW)@postgres:5432/cih" `
  -e HF_HOME=/data/hf-cache `
  -v cih-repo:/repo `
  -v cih-data:/data `
  -v cih-home:/home/cih/.cih `
  $IMG

claude mcp add --transport http cih http://localhost:8080/mcp
```

### 7. (Optional) Export results to the host later
```powershell
docker cp cih-box:/repo/.cih "C:\projects\payment-service\.cih"
```

---

## Updating after upstream changes (`git pull`)

The volume is a **frozen snapshot** and the runtime image has **no `git`**, so pull on the **host**,
then re-sync the volume and re-index.

```powershell
cd $SRC; git pull

# Wipe + full re-copy (docker cp doesn't delete, so upstream-deleted files would otherwise linger)
docker exec -u 0 cih-box sh -c 'rm -rf /repo/.[!.]* /repo/*'
docker cp "$SRC\." cih-box:/repo
docker exec -u 0 cih-box chown -R cih:cih /repo

docker exec cih-box cih-engine analyze /repo --all
docker exec cih-box cih-engine embed /repo                            # skips unchanged chunks (BLAKE3)
docker exec cih-box cih-engine discover /repo --feature-strategy embed
docker restart cih-server
```

- Cheap because `embed` skips unchanged chunks via the content-hash cache in **Postgres** (`pg-data`
  survives the wipe) and prunes vectors for deleted classes.
- Wiping `/repo` is safe: `.cih/` regenerates, embed cache is in Postgres, registry is in `cih-home`.
- **Source and graph must move together** — `read_file` reads on-disk lines while the graph stores
  ranges from `analyze`; update source without re-analyzing (or vice versa) and line numbers drift.

---

## Verification

```powershell
# Graph loaded
docker exec falkordb redis-cli -p 6379 GRAPH.QUERY cih "MATCH (n) RETURN count(n)"
# Embed feature groups present with meaningful slugs
docker exec cih-box sh -c "head -3 /repo/.cih/artifacts-features/*/groups.jsonl"
# Per-node vectors populated (if psql is available in the image)
docker exec cih-box sh -c 'psql "$CIH_PG_URL" -c "SELECT count(*) FROM cih_node_vectors;"'
```
Then in Claude Code: `list_repos`, `search_code(query="payment")`, and
`read_file(path="<some/file>.java")`. A `read_file` error of *"no repos in registry"* means the
`cih-home` volume isn't shared; *"cannot read '…'"* means `cih-repo` isn't mounted.

---

## Caveats

- **Model download at `embed`** hits HuggingFace once into `/data/hf-cache` (`cih-data` volume). On a
  locked-down network, set `-e HTTPS_PROXY=...` or pre-seed the cache; otherwise skip `embed` and use a
  plain `discover /repo` (package strategy).
- **`sleep` toolbox** (not `docker compose run engine`) because `docker cp` needs a running container
  and the compose services hard-code the `${REPO_PATH}:/repo` bind mount we're avoiding.
- **Permissions**: the step-4 `chown` covers the uid-1001 engine user (usually harmless on Docker
  Desktop, cheap insurance).
- **Cleanup**: `docker rm -f cih-box cih-server`; `docker compose down`; `docker volume rm cih-repo`
  to discard the copied source + artifacts.
```
