# Runbook: CIH + Kiro — full flow (index a repo, query it from Kiro)

End-to-end guide from a raw codebase to querying it in the [Kiro](https://kiro.dev) IDE via MCP.
Uses the isolated **copy-source-into-a-volume** pattern (companion to
[`analyze-by-copying-source-into-container.md`](./analyze-by-copying-source-into-container.md)),
parameterized so one Docker stack can host many repos side by side. Fineract is the worked example.

## Why isolated per repo

One FalkorDB + one Postgres host **every** repo: each gets a distinct **graph key** (FalkorDB
graph), **database** (Postgres embeddings), **volumes**, and **host port** for its MCP server.
Nothing cross-contaminates; you add/remove repos independently.

---

## 0. Prerequisites

- **Docker Desktop** running.
- **Node.js / `npx`** on PATH — Kiro launches the `mcp-remote` bridge via npx.
- **Kiro** installed.
- The `yummy-cih:local` image built from this repo:
  ```bash
  cd /path/to/yummy-cih && docker build -t yummy-cih:local .
  ```
  (The published `phuchoang29/yummy-cih:latest` also works but may lag this repo.)

## 1. Start shared infra (once, stays up)

```bash
cd /path/to/yummy-cih
docker compose up -d falkordb postgres      # FalkorDB :6380, Postgres :5433
```

## 2. Pick per-repo identifiers

| Var    | Fineract example                    | Meaning              |
|--------|-------------------------------------|----------------------|
| `SRC`  | `/Users/you/BigMoves/fineract`      | host source path     |
| `KEY`  | `fineract`                          | FalkorDB graph key   |
| `DB`   | `fineract`                          | Postgres database    |
| `PORT` | `8081`                              | host port for its MCP server |

```bash
SRC=/Users/you/BigMoves/fineract; KEY=fineract; DB=fineract; PORT=8081
PGPW=$(grep '^POSTGRES_PASSWORD=' /path/to/yummy-cih/.env | cut -d= -f2-)
```

## 3. Index the repo (copy source into a volume, run the pipeline)

```bash
# isolated Postgres DB + repo volume
docker exec yummy-cih-postgres-1 psql -U cih -d cih -c "CREATE DATABASE $DB;" 2>/dev/null
docker volume create ${KEY}-repo

# toolbox container (sleeps; we run the engine inside it via docker exec)
docker run -d --name ${KEY}-box --network yummy-cih_default \
  -e FALKOR_URL=redis://falkordb:6379 -e CIH_GRAPH_KEY=$KEY \
  -e CIH_PG_URL="postgres://cih:${PGPW}@postgres:5432/$DB" \
  -e HF_HOME=/data/hf-cache \
  -v ${KEY}-repo:/repo -v yummy-cih_cih-data:/data -v ${KEY}-home:/home/cih/.cih \
  --entrypoint sleep yummy-cih:local infinity

# copy source in + fix ownership (engine runs as uid 1001)
docker cp "$SRC/." ${KEY}-box:/repo
docker exec -u 0 ${KEY}-box chown -R 1001:1001 /repo /home/cih/.cih

# the pipeline
docker exec ${KEY}-box cih-engine analyze /repo --all                        # graph → FalkorDB
docker exec ${KEY}-box cih-engine embed   /repo                              # vectors → Postgres (1st run downloads model)
docker exec ${KEY}-box cih-engine discover /repo --feature-strategy embed \
                                            --embed-leiden-resolution 1.2     # communities + embedding clusters
```

`discover` loads FalkorDB by default (do **not** pass `--no-load`) so the server can query the graph.

### 3a (optional) — Decompile internal JAR dependencies

**Skip unless** your repo ships **first-party / internal closed-source JARs** (e.g. `mfa-core`,
`bank-auth`) whose *internals* you want in the graph. Without this, CIH still auto-extracts JAR
**signature stubs** (class/method nodes) during `analyze` — you just don't get their method bodies,
internal call graph, or taint flow. Third-party libs (Spring, commons) should **not** be decompiled.

The decompiler runs **automatically as a pre-step of `analyze`** when a `cih.decompile.toml` exists
at the repo root with `[[sources]]`. So configure it *before* the `analyze` call above. The runtime
image already ships a JVM (openjdk-17) to run the decompiler; Vineflower auto-downloads on first run
(needs network), or set `tool_jar` to a pre-placed jar.

```bash
# Write the config into the repo volume (edit dir/prefix for your JARs).
# `dir` is a path INSIDE the container — the JARs must be under /repo (copied with the source)
# or a path you also mount/copy into the box. `prefix` matches the JAR *filename*.
docker exec -i ${KEY}-box sh -c 'cat > /repo/cih.decompile.toml' <<'TOML'
tool      = "vineflower"          # "vineflower" (recommended) | "cfr" | "jadx"
cache_dir = ".cih/decompiled"
# tool_jar = "/opt/vineflower.jar"  # optional: skip the auto-download

[[sources]]
dir    = "libs"                   # e.g. /repo/libs, target/lib, …
prefix = "mfa-"                   # decompiles mfa-*.jar, skips commons-*.jar / spring-*.jar
TOML
docker exec -u 0 ${KEY}-box chown 1001:1001 /repo/cih.decompile.toml

# Then run analyze WITH --include-decompiled so the decompiled .java are in scope:
docker exec ${KEY}-box cih-engine analyze /repo --all --include-decompiled
# …then embed + discover as in step 3.
```

Decompiled classes are injected as ordinary source (no `external: true`), so `CALLS` edges flow from
your code **into** the library internals. It's cached by JAR hash — the second run is a ~ms hash
check. (Interactive alternative to hand-writing the TOML:
`docker exec -it ${KEY}-box cih-engine config decompile --repo /repo`.)

## 4. Start that repo's MCP server

```bash
docker run -d --name ${KEY}-server --network yummy-cih_default -p ${PORT}:8080 \
  -e FALKOR_URL=redis://falkordb:6379 -e CIH_GRAPH_KEY=$KEY -e CIH_BIND=0.0.0.0:8080 \
  -e CIH_ARTIFACTS_DIR=/repo/.cih/artifacts \
  -e CIH_PG_URL="postgres://cih:${PGPW}@postgres:5432/$DB" \
  -e HF_HOME=/data/hf-cache -e CIH_ALLOW_INSECURE=1 \
  -v ${KEY}-repo:/repo -v yummy-cih_cih-data:/data -v ${KEY}-home:/home/cih/.cih \
  yummy-cih:local

curl -s localhost:${PORT}/health            # → {"status":"ok"}
```

- The `${KEY}-box` toolbox can now be stopped (`docker rm -f ${KEY}-box`) — the server + FalkorDB +
  Postgres are what matter.
- Visual UI: open **`http://localhost:${PORT}/graph`** (Overview + Clusters tabs).

## 5. Wire it into Kiro

Kiro connects to MCP servers as **stdio commands**, so bridge to the HTTP endpoint with `mcp-remote`.
Edit **`~/.kiro/settings/mcp.json`** (global) or `.kiro/settings/mcp.json` (workspace) and add one
entry per repo — **always with `--transport http-only`**:

```json
{
  "mcpServers": {
    "cih-fineract": {
      "command": "npx",
      "args": ["-y", "mcp-remote", "http://localhost:8081/mcp", "--transport", "http-only"],
      "disabled": false,
      "autoApprove": ["context","impact","trace_flow","search_code","query",
                      "communities","feature_map","route_map","read_file",
                      "test_coverage","regression_scope","list_repos","status"]
    }
  }
}
```

Then in Kiro → **MCP Servers** panel → reconnect (or reopen Kiro).

> **Why `--transport http-only` is mandatory:** `mcp-remote`'s default transport auto-detection
> probes with a throwaway "fallback-test" session and mishandles it against CIH's `rmcp`
> streamable-HTTP server — it fires `tools/list` on a session that never completed the
> `initialize → notifications/initialized` handshake, and the server rejects it with
> *"expect initialized notification, but received ListToolsRequest."* Forcing `http-only` skips
> the probe and uses the correct streamable-HTTP flow.

## 6. Use it in Kiro

Tools are now in Kiro chat. **First ask it to `list_repos`** (the server instructs this), then work
by intent — Kiro picks the tools:

- *"list_repos, then use feature_map to find where loan disbursement is implemented."*
- *"Run impact upstream on `LoanWritePlatformService` — what's the blast radius before I change it?"*
- *"trace_flow from the POST /loans route end-to-end."*
- *"Show the communities and summarize the loan-related ones."*
- *"search_code for 'interest recalculation' and read_file the top hit."*

Source tools (`read_file`, snippets) work because the server mounts the repo volume + registry.

## 7. Re-index after code changes

```bash
# (start ${KEY}-box again if you removed it)
docker exec -u 0 ${KEY}-box sh -c 'rm -rf /repo/.[!.]* /repo/*'
docker cp "$SRC/." ${KEY}-box:/repo
docker exec -u 0 ${KEY}-box chown -R 1001:1001 /repo
docker exec ${KEY}-box cih-engine analyze /repo --all
docker exec ${KEY}-box cih-engine embed /repo          # skips unchanged chunks (fast)
docker exec ${KEY}-box cih-engine discover /repo --feature-strategy embed --embed-leiden-resolution 1.2
docker restart ${KEY}-server
```

No Kiro change needed — it queries the live server.

## 8. Add another repo

Repeat steps 2–5 with a new `KEY` / `DB` / `PORT` (e.g. `8082`). Kiro shows each as a separate tool
group; add a matching `mcp.json` entry.

## 9. Troubleshooting

| Symptom | Cause / fix |
|---|---|
| Kiro shows no tools; server logs *"expect initialized notification"* | Add `--transport http-only` to the `mcp-remote` args (this runbook does). |
| Tools missing, no error | `npx`/Node not on PATH, or `curl localhost:${PORT}/health` ≠ 200. |
| *"no repos in registry"* | `${KEY}-home` volume not shared, or the `chown` step was skipped. |
| `read_file` → *"cannot read …"* | `${KEY}-repo` volume not mounted on the server. |
| Server down | `docker ps | grep ${KEY}-server`; restart it. |

## Teardown (per repo)

```bash
docker rm -f ${KEY}-box ${KEY}-server
docker volume rm ${KEY}-repo ${KEY}-home
docker exec yummy-cih-postgres-1 psql -U cih -d cih -c "DROP DATABASE $DB;"
docker exec yummy-cih-falkordb-1 redis-cli -p 6379 GRAPH.DELETE $KEY
```
