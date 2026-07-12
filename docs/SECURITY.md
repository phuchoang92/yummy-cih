# Security

CIH indexes source code into a graph and exposes it over an MCP server. When you
point it at a sensitive codebase (e.g. a banking monolith), two things matter:
who can reach the server, and whether any code leaves the machine.

## 1. Authentication on the MCP server

The server (`cih-server`) protects `/mcp` and `/graph` with an optional static
bearer token, `CIH_API_TOKEN`. Requests must send `Authorization: Bearer <token>`.

**The server refuses to start if it is network-exposed without a token.** On any
non-loopback bind (e.g. `CIH_BIND=0.0.0.0:8080`, the Docker Compose default) with
`CIH_API_TOKEN` unset, startup fails with a clear error. To run a shared/team
server:

```bash
CIH_API_TOKEN=$(openssl rand -hex 32) docker compose up -d
```

Escape hatches:
- **Loopback binds** (`127.0.0.1`, `::1`, `localhost`) start without a token — a
  warning is logged. Intended for single-user local dev.
- **`CIH_ALLOW_INSECURE=1`** allows a non-loopback bind without a token. Use only
  on a genuinely trusted, isolated network. It is off by default so the server
  fails safe.

Put the server behind TLS (reverse proxy) for any non-local deployment; the bearer
token is sent in a header and must not travel in cleartext.

## 2. LLM data egress

`cih-server` no longer performs any **LLM** egress. The embedded `ask_codebase`
agent — the one tool that POSTed your code (symbol names, method signatures, file
paths, search snippets) to an external LLM — has been **removed**. All MCP tools are
now deterministic and local: they query FalkorDB / the `.cih` artifacts and send no
code off-box.

Natural-language Q&A now lives in your **MCP client** (Claude Code or any agent),
which you control and can point at an approved model. The client drives CIH's raw
structured tools (`search_code`, `context`, `impact`, `route_map`, `trace_flow`,
`trace_flow_x`, `detect_changes`, `taint_paths`, …), none of which send code off-box —
and, unlike the old embedded agent, it reasons across your **whole repo group**, not
just one primary repo. For a headless "ask" endpoint with no model client, run a
separate sidecar that is itself an MCP client and holds your own key (see the
`cih-agent` follow-up); the graph server stays egress-free.

### Remaining outbound path — embedding model download (not LLM, not code)

`cih-server` still makes **one** kind of outbound call when semantic search is
enabled: the `cih-embed` crate (via `fastembed` → `hf-hub`) downloads the sentence
embedding model from **huggingface.co** on first use. This sends **no repository
data** — only a public model fetch. For a fully air-gapped deployment, pre-provision
the model and set `HF_HUB_OFFLINE=1`, or run without `CIH_PG_URL` (BM25-only search
needs no embeddings). Making the core provably zero-egress is a tracked follow-up.

### Recommendation for sensitive / regulated codebases

This is now the default posture — the graph server holds no LLM key and makes no
code-bearing outbound call. Keep LLM reasoning in a client/sidecar you control, and
air-gap the embedding model (above) if required.

## 3. Other notes

- **`read_file`** is sandboxed to the repo root and rejects `..` path components;
  it also caps file size and returned lines (`CIH_READ_FILE_MAX_BYTES`,
  `CIH_READ_FILE_MAX_LINES`) so a large file cannot be pulled wholesale.
- **Cypher queries** are built with escaping (`cstr`); there is no raw-query
  passthrough tool.
- **Query backpressure**: concurrent Cypher execution is capped
  (`CIH_MAX_CONCURRENT_QUERIES`, default 64); excess requests shed with a
  retryable overloaded error after `CIH_QUERY_QUEUE_TIMEOUT_MS` rather than
  letting a client burst exhaust FalkorDB.
- **Secrets**: `.env` is git-ignored; API tokens are redacted in the server's
  config debug log. Do not commit `.env` or bake tokens into images.

## Reporting

Report suspected vulnerabilities privately to the maintainers rather than opening a
public issue.
