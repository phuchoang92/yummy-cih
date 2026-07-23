# Runbook: Re-index OCB after the OSGi/CXF stitching fixes (Windows)

Companion to [`analyze-by-copying-source-into-container.md`](./analyze-by-copying-source-into-container.md)
(the base copy-into-volume workflow — container names, volumes, and env vars below come from there).
Run this once after upgrading to **`dev ≥ 5c7b1f3`**, which contains the completed
OSGi/CXF stitching fixes.

## Why a full re-index is required

The fixes change **route identity**: paths gain the per-bundle CXF base
(`/beneficiaries` → `/rest/remittance/v1/beneficiaries`), and dual-server bundles get one
Route node **per address** (`/v1` secured + `/ns/v1` non-secured). Node ids embed the path,
so everything keyed on old `Route:` ids — the loaded graph, wiki pages, embeddings, saved
queries — goes stale together. Re-analyze, re-embed, re-discover, regenerate the wiki.

## What to expect afterwards (not errors)

| Observation | Meaning |
|---|---|
| Route count roughly **doubles** on dual-server bundles | Intended — both real URLs are now modeled; the old count underreported the API surface |
| Paths like `/rest/<bundle>/v1/…` and `/rest/<bundle>/ns/v1/…` | Per-bundle whiteboard pattern + jaxrs:server address stitching |
| `servlet_prefix_source: "osgi_whiteboard"` on routes | The prefix came from that bundle's `beans_rest_web_servlets.xml` |
| New `di-xml-blueprint-reference` / `di-xml-bean-field` edges | Spring-DM `<osgi:reference>` + `bundle-context-*.xml` bean wiring now parsed (label says "blueprint" but covers Spring-DM too) |
| `servlet_prefix_source: "none"` on some routes | No whiteboard pattern shares that server's bundle directory — inspect the layout, or set `cxf_base_path` in `cih.toml` |

---

## Steps

Variables as in the base runbook (`$SRC`, `$NET`, `$IMG`, `$env:PGPW`).

### 1. Refresh the image to `dev ≥ 5c7b1f3`

Either pull the published image (if it has been rebuilt from `dev`):

```powershell
docker pull phuchoang29/yummy-cih:latest
```

or build locally from the branch:

```powershell
cd C:\projects\yummy-cih
git fetch origin; git checkout dev; git pull
git log --oneline -1          # must be 5c7b1f3 or later
docker build -t yummy-cih:local .
$IMG = "yummy-cih:local"
```

### 2. (Only if OCB source changed) re-sync the volume

Skip when the source is unchanged — the re-index is needed regardless because the *engine* changed.

```powershell
docker exec -u 0 cih-box sh -c 'rm -rf /repo/.[!.]* /repo/*'
docker cp "$SRC\." cih-box:/repo
docker exec -u 0 cih-box chown -R cih:cih /repo     # uid 1001, or registry/artifact writes fail
```

### 3. Recreate the toolbox on the new image and re-run the pipeline

```powershell
docker rm -f cih-box
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

# --no-cache: route stitching changed; do not trust cached per-file results
docker exec cih-box cih-engine analyze /repo --all --no-cache
docker exec cih-box cih-engine embed /repo                             # re-embeds changed route/class chunks
docker exec cih-box cih-engine discover /repo --feature-strategy embed
docker exec cih-box cih-engine wiki /repo                              # route ids changed → wiki must regenerate
```

### 4. Restart the MCP server on the new image

```powershell
docker rm -f cih-server
docker run -d --name cih-server `
  --network $NET `
  -p 8080:8080 `
  -e FALKOR_URL=redis://falkordb:6379 `
  -e CIH_GRAPH_KEY=cih `
  -e CIH_BIND=0.0.0.0:8080 `
  -e CIH_ALLOW_INSECURE=1 `
  -e CIH_ARTIFACTS_DIR=/repo/.cih/artifacts `
  -e CIH_PG_URL="postgres://cih:$($env:PGPW)@postgres:5432/cih" `
  -e HF_HOME=/data/hf-cache `
  -v cih-repo:/repo `
  -v cih-data:/data `
  -v cih-home:/home/cih/.cih `
  $IMG
```

`CIH_ALLOW_INSECURE=1` (or a `CIH_API_TOKEN`) is required on current images: the server refuses a
non-loopback bind without auth. Trusted-LAN laptop use → allow-insecure is fine.

---

## Verification — did the fixes take?

```powershell
# 1. Dual routes exist for a known bundle (expect BOTH /v1 and /ns/v1 lines)
docker exec cih-box sh -c "grep -o '\"id\":\"Route:[^\"]*remittance[^\"]*\"' /repo/.cih/artifacts/*/nodes.jsonl | sort -u | head -20"

# 2. Prefix provenance: counts per servlet_prefix_source (osgi_whiteboard should dominate;
#    investigate any large 'none' bucket)
docker exec cih-box sh -c "grep -o '\"servlet_prefix_source\":\"[a-z_]*\"' /repo/.cih/artifacts/*/nodes.jsonl | sort | uniq -c"

# 3. Spring-DM / DI edges present (both counts must be > 0)
docker exec cih-box sh -c "grep -c 'di-xml-blueprint-reference' /repo/.cih/artifacts/*/edges.jsonl"
docker exec cih-box sh -c "grep -c 'di-xml-bean-field' /repo/.cih/artifacts/*/edges.jsonl"

# 4. Graph loaded with the new routes
docker exec falkordb redis-cli -p 6379 GRAPH.QUERY cih "MATCH (r:Route) RETURN count(r)"
```

Then from Claude Code (`claude mcp add --transport http cih http://localhost:8080/mcp`):

- `route_map()` — spot-check a bundle: both `GET /rest/remittance/v1/beneficiaries` and
  `GET /rest/remittance/ns/v1/beneficiaries` present.
- `trace_flow(name="Route:GET /rest/remittance/v1/beneficiaries")` — reaches the impl method
  (`RemittanceServiceRestEndPointImpl`) via HANDLES_ROUTE + heritage.
- `context(name="AuthenticationService")` (any `<osgi:reference>` interface) — callers now include
  the consuming bundle's classes via the new DI edges.
- `taint_paths(category="sql")` — the `/ns/v1` entry points are now visible sources; expect more
  reported paths, not fewer.

## Troubleshooting

| Symptom | Cause / fix |
|---|---|
| Route count did NOT increase | Stale image — check `docker inspect $IMG --format '{{.Created}}'` postdates your build/pull and the build checkout was `dev ≥ 5c7b1f3`; or `analyze` ran without `--no-cache` |
| All routes `servlet_prefix_source: "none"` | Whiteboard XML not detected — check `beans_rest_web_servlets.xml` files were copied into `/repo` (step 2 wipes + re-copies); as a stopgap set `cxf_base_path` in `cih.toml` |
| Zero `di-xml-*` edges | `META-INF/spring/*.xml` missing from the copy, or the files lack the `springframework.org/schema/osgi` / `schema/beans` namespaces |
| Server exits immediately after start | Auth posture check — set `CIH_ALLOW_INSECURE=1` or `CIH_API_TOKEN` (see step 4) |
| `read_file` says "no repos in registry" | `cih-home` volume not mounted on the new containers (base runbook §caveats) |
| Wiki links 404 on route pages | Wiki generated before the re-analyze — re-run `cih-engine wiki /repo` after `analyze` |
