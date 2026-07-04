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

## 2. LLM data egress — `ask_codebase`

Most MCP tools are fully local: they query FalkorDB / the `.cih` artifacts and send
nothing outside the machine.

**The exception is `ask_codebase`.** It runs an internal agent loop that calls an
external, OpenAI-compatible LLM endpoint. The requests include the user's question,
a codebase description, and tool results — which contain **symbol names, fully
qualified method signatures, file paths, and code-search snippets** from your repo.

- It is **opt-in and off by default**: the tool is only enabled when an API key is
  configured (`CIH_AGENT_API_KEY`, or `GEMINI_API_KEY` / `OPENAI_API_KEY` /
  `ANTHROPIC_API_KEY`). With no key set, `ask_codebase` is disabled.
- The **default endpoint is Google Gemini** (`generativelanguage.googleapis.com`).
  Override with `CIH_AGENT_LLM_BASE_URL` / `CIH_AGENT_LLM_MODEL` to point at an
  internal or approved endpoint.

### Recommendation for sensitive / regulated codebases

Leave `ask_codebase` **disabled** (set no agent API key). Do LLM reasoning in your
downstream agent, which you control and can point at an approved model — and drive
it with the raw structured tools (`search_code`, `context`, `impact`, `trace_flow`,
`detect_changes`, `taint_paths`, …), none of which send code off-box. If you do want
`ask_codebase` on, point `CIH_AGENT_LLM_BASE_URL` at an internally hosted,
contractually-approved endpoint — never the public default.

## 3. Other notes

- **`read_file`** is sandboxed to the repo root and rejects `..` path components;
  it also caps file size and returned lines (`CIH_READ_FILE_MAX_BYTES`,
  `CIH_READ_FILE_MAX_LINES`) so a large file cannot be pulled wholesale.
- **Cypher queries** are built with escaping (`cstr`); there is no raw-query
  passthrough tool.
- **Secrets**: `.env` is git-ignored; API tokens are redacted in the server's
  config debug log. Do not commit `.env` or bake tokens into images.

## Reporting

Report suspected vulnerabilities privately to the maintainers rather than opening a
public issue.
