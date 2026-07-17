# AWS POC — one box, one afternoon

Goal: prove CIH serves a real repo from AWS to Claude Code on your laptop. Nothing
else. The real designs live in
[`../plans/aws-deployment-shoestring.md`](../plans/aws-deployment-shoestring.md)
($20/mo) and [`../plans/aws-deployment-architecture.md`](../plans/aws-deployment-architecture.md)
(org-wide) — **don't build either yet.**

## Cost framing

For a POC the monthly bill is the wrong unit. **Cost = hours × instance.**

`t4g.large` (8 GiB) is $0.0672/hr → a 2-day POC with ~16 h powered on is **~$1.10**.
Your $20 is ~60 hours of it. So take the big box — build and index headroom are worth
more than 2¢/hr — and **terminate it when you're done**. The only way this costs $20
is leaving it running for a month.

## The two decisions, settled

**AMI: Amazon Linux 2023 (arm64).** With Docker the AMI barely matters for the app —
everything runs in containers built from `debian:bookworm-slim`, so the host's
glibc/OpenSSL are irrelevant. It's a *host-tooling* choice, and AL2023 ships the **SSM
Agent preinstalled**: you get a port-forward with **zero inbound security-group rules**
— not even port 22 — and no SSH key to manage.

**Docker Compose, not native `cargo`.** Not because of Rust: **FalkorDB is a Redis
module** distributed as a Docker image, so Docker is on the box regardless. Once it's
there, native binaries buy nothing and cost a Rust toolchain, `libssl-dev`, and a
hand-written systemd unit. (Native is more viable than you'd think — the `/graph`
assets are committed and `include_str!`'d so no Node is needed, Java is only for
optional JAR-decompile, and ONNX is statically linked — it's just pointless here.)

**Build on the box.** No CI builds or pushes the image, so `phuchoang29/yummy-cih:latest`
is hand-pushed from an arm64 Mac: arch-specific *and* stale — it predates the
arrow-const/CommonJS extraction work. Pulling it would show the old ~3% coverage and
prove the wrong thing. Building here makes the arch automatically right and indexes
the code you actually committed.

## 1. Launch

- **AMI**: Amazon Linux 2023, **arm64**
- **Instance**: `t4g.large` (8 GiB — build + index headroom)
- **Disk**: 30 GB gp3
- **Network**: public subnet + public IP (the build needs egress: crates.io, npm, the
  ort/ONNX fetch)
- **Security group**: **no inbound rules at all**
- **IAM instance profile**: attach `AmazonSSMManagedInstanceCore`

That last one is what replaces SSH. Without it you'll have no way in — it's the step
people skip.

## 2. Host prep

```bash
aws ssm start-session --target i-0123456789abcdef0
sudo su - ec2-user                     # SSM drops you in as ssm-user

sudo dnf install -y docker git
sudo systemctl enable --now docker
sudo usermod -aG docker ec2-user

# Compose v2 isn't in AL2023's repos — install the plugin (aarch64 for Graviton).
sudo mkdir -p /usr/local/lib/docker/cli-plugins
sudo curl -SL https://github.com/docker/compose/releases/latest/download/docker-compose-linux-aarch64 \
  -o /usr/local/lib/docker/cli-plugins/docker-compose
sudo chmod +x /usr/local/lib/docker/cli-plugins/docker-compose

exit; exit    # re-connect so the docker group applies
```

## 3. Clone and build

```bash
git clone <your-cih-remote> ~/yummy-cih && cd ~/yummy-cih

export CIH_API_TOKEN=$(openssl rand -hex 32); echo "$CIH_API_TOKEN"   # save it
export REPO_PATH=$HOME/repos/myapp
git clone <repo-to-index> "$REPO_PATH"

docker compose -f docker-compose.poc.yml build     # ~20-30 min, once
docker compose -f docker-compose.poc.yml up -d
```

`docker-compose.poc.yml` is in the repo — two services, no Postgres. (The main
`docker-compose.yml` can't be used here: it declares `${POSTGRES_PASSWORD:?}` and
Compose interpolates the whole file before choosing services, so it fails at *parse*
time even though you never start Postgres.)

Semantic search is off by design — `CIH_PG_URL` is optional and search falls back to
BM25 over the artifacts. That's one less container and one less GB of RAM.

## 4. Index

Same service, entrypoint overridden — identical mounts and paths, no third service:

```bash
IDX="docker compose -f docker-compose.poc.yml run --rm --entrypoint cih-engine cih-server"
$IDX analyze  /repo --all --graph-key poc
$IDX discover /repo --graph-key poc
```

**Read the summary `analyze` prints:**

```
Resolve    59  edges  (513 unresolved refs)
Coverage   61%  of 89 callables extracted
```

If **coverage is near zero, stop** — the graph is junk and the tools aren't worth
testing. That's your fastest signal that something's wrong (e.g. the box built stale
code), and it's exactly why we build here instead of pulling `:latest`.

## 5. Connect from your laptop

Needs the [Session Manager plugin](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html)
installed locally. Keep this running:

```bash
aws ssm start-session --target i-0123456789abcdef0 \
  --document-name AWS-StartPortForwardingSession \
  --parameters '{"portNumber":["8080"],"localPortNumber":["8080"]}'
```

```bash
claude mcp add --transport http cih http://localhost:8080/mcp \
  --header "Authorization: Bearer $CIH_API_TOKEN"
```

No ALB, no TLS cert, no VPN, no open ports: SSM gives you the tunnel, IAM gives you
the access control.

## 6. Prove it — the actual POC

In Claude Code:

- `list_repos()` → your repo appears
- `context(name="<some symbol>")` → callers/callees
- `trace_flow(name="Route:GET /...")` → an end-to-end chain
- `http://localhost:8080/graph` in a browser through the same tunnel — also proves the
  committed UI assets baked into the binary correctly

That's the POC passing: **a real repo, indexed on AWS, answering structural queries
from your laptop.**

## 7. Tear down

```bash
docker compose -f docker-compose.poc.yml down -v
```

Then **terminate the instance** and delete the EBS volume. This is the step that keeps
the POC at ~$1 instead of $20. To pause instead: `stop` the instance — you keep the
disk (~$2.40/mo for 30 GB) and pay no compute.

## Deliberately skipped

Each is real; each belongs *after* the POC proves the thing works.

| Skipped | Add when | Where |
|---|---|---|
| Tailscale / ALB / TLS | more than one user | shoestring doc |
| pgvector (semantic search) | BM25 isn't enough | shoestring doc |
| Cron / webhook indexing | the index must stay fresh | shoestring doc |
| S3 wiki | you want the docs site | shoestring doc |
| EFS, cells, RDS, HA | org-wide rollout | architecture doc |

## Gotchas that will actually bite

- **No IAM instance profile → no way in.** SSM is the only door; there's no SSH key
  and no inbound rule.
- **SSM logs you in as `ssm-user`** — `sudo su - ec2-user` or docker-group membership
  won't apply.
- **Build needs egress.** A failure in `ort-sys` is the ONNX archive fetch — network,
  not code.
- **BuildKit required** — the Dockerfile uses `--mount=type=cache`. Default in Compose
  v2; don't fall back to legacy `docker-compose`.
- **Don't set `CIH_ALLOW_INSECURE`.** The server refusing to start without a token is
  the guardrail, and this box has a public IP. The compose file makes the token
  mandatory on purpose.
- **One graph key per repo.** `poc` is fine for one; never share a key across repos.
- **RAM**: 8 GiB covers most repos. A Fineract/liferay-scale monolith may still need
  `sudo fallocate -l 8G /swapfile` — turns an OOM-kill into a slow batch job.
