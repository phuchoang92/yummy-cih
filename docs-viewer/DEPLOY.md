# CIH Docs Viewer — Deployment Guide

## Prerequisites

- Docker Desktop with buildx enabled
- Logged in to Docker Hub: `docker login`

---

## Build & push to Docker Hub

```bash
# From the repo root
cd yummy-cih

docker buildx build \
  --builder multi-builder \
  --platform linux/amd64,linux/arm64 \
  -f Dockerfile.docs-viewer \
  -t phuchoang29/yummy-cih-docs:latest \
  --push \
  .
```

To tag a version at the same time:

```bash
docker buildx build \
  --builder multi-builder \
  --platform linux/amd64,linux/arm64 \
  -f Dockerfile.docs-viewer \
  -t phuchoang29/yummy-cih-docs:latest \
  -t phuchoang29/yummy-cih-docs:v1 \
  --push \
  .
```

> `--push` is required for multi-platform builds — Docker cannot load a
> multi-arch image into the local daemon, so it goes straight to the registry.

---

## Generate docs for a repo

> **New**: You can also use `cih-engine start` to interactively configure and generate docs. Run it first, then continue with the viewer commands below.

Run these once per repo before starting the viewer:

```bash
# Using the engine binary directly
cih-engine analyze /path/to/my-repo --all
cih-engine discover /path/to/my-repo
cih-engine wiki /path/to/my-repo

# Or via Docker Compose (needs FalkorDB running)
export REPO_PATH=/path/to/my-repo
docker compose run --rm engine analyze /repo --all
docker compose run --rm engine discover /repo
docker compose run --rm engine wiki /repo
```

Output is written to `/path/to/my-repo/.cih/wiki/` (contains `pages/` and `manifest.json`).

---

## Pull & run from Docker Hub

No build step needed. `docker run` pulls automatically on first use; run
`docker pull phuchoang29/yummy-cih-docs:latest` explicitly only when you want
to force a refresh to the latest version.

### Single repo

```bash
docker run -d \
  --name cih-docs \
  -p 3001:3001 \
  -e CIH_WIKI_PATH=/wiki/pages \
  -v /path/to/my-repo/.cih/wiki:/wiki:ro \
  phuchoang29/yummy-cih-docs:latest
```

Mount the whole `.cih/wiki/` directory (not just `pages/`) so the container can also
read `manifest.json` one level up from `pages/` — this powers the landing page community cards.

Open: http://localhost:3001

### Multiple repos

Each volume mount adds one repo. The subfolder name becomes the URL prefix.

```bash
docker run -d \
  --name cih-docs \
  -p 3001:3001 \
  -v /path/to/repo-a/.cih/wiki/pages:/wiki/repo-a:ro \
  -v /path/to/repo-b/.cih/wiki/pages:/wiki/repo-b:ro \
  phuchoang29/yummy-cih-docs:latest
```

Open:
- http://localhost:3001/repo-a/
- http://localhost:3001/repo-b/

### Stop & remove

```bash
docker stop cih-docs && docker rm cih-docs
```

### Via Docker Compose

```bash
export REPO_PATH=/path/to/my-repo
export REPO_NAME=my-repo        # becomes the URL prefix

docker compose --profile docs up -d docs-viewer
```

Open: http://localhost:3001/my-repo/

---

## Update after re-generating docs

The pages folder is mounted read-only. Restart the container to pick up new pages:

```bash
# Standalone container
docker restart cih-docs

# Docker Compose
docker compose --profile docs restart docs-viewer
```

---

## Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `CIH_WIKI_PATH` | — | Single-repo mode: path to `pages/` inside the container (e.g. `/wiki/pages` when `.cih/wiki` is mounted at `/wiki`) |
| `CIH_WIKI_REPOS_DIR` | `/wiki` | Multi-repo mode: parent dir, each subdir = one repo |
| `CIH_REPO_NAME` | folder name | Display name override (fallback when no `manifest.json`) |
