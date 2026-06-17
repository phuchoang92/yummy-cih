# LLM Provider Guide

The `wiki` command optionally calls an LLM to generate richer documentation.
Pass `--llm` to enable enrichment, then pick a provider with `--llm-provider`.

## Quick-start examples

```bash
# DeepSeek (recommended — cheap, reliable, clean JSON output)
DEEPSEEK_API_KEY="sk-..." \
cargo run -p cih-engine -- wiki "$REPO" \
  --wiki-mode llm-summary \
  --llm --llm-provider deepseek \
  --llm-model deepseek-chat \
  --llm-max-tokens 4096

# Google Gemini
GEMINI_API_KEY="AQ...." \
cargo run -p cih-engine -- wiki "$REPO" \
  --wiki-mode llm-summary \
  --llm --llm-provider gemini \
  --llm-model gemini-2.5-flash \
  --llm-max-tokens 4096

# Anthropic Claude
ANTHROPIC_API_KEY="sk-ant-..." \
cargo run -p cih-engine -- wiki "$REPO" \
  --wiki-mode llm-summary \
  --llm --llm-provider anthropic \
  --llm-model claude-haiku-4-5-20251001 \
  --llm-max-tokens 2048

# OpenAI
OPENAI_API_KEY="sk-..." \
cargo run -p cih-engine -- wiki "$REPO" \
  --wiki-mode llm-summary \
  --llm --llm-provider openai-compatible \
  --llm-model gpt-4o-mini \
  --llm-max-tokens 2048

# Self-hosted (Ollama, vLLM, LM Studio, etc.)
CIH_LLM_API_KEY="your-key" \
cargo run -p cih-engine -- wiki "$REPO" \
  --wiki-mode llm-summary \
  --llm --llm-provider openai-compatible \
  --llm-base-url http://localhost:11434/v1 \
  --llm-model llama3:8b \
  --llm-max-tokens 2048
```

## Provider reference

| Provider | `--llm-provider` | Default model | API key env var | Notes |
|---|---|---|---|---|
| DeepSeek | `deepseek` | `deepseek-chat` | `DEEPSEEK_API_KEY` | Reliable JSON, no fences |
| Google Gemini | `gemini` | `gemini-2.5-flash` | `GEMINI_API_KEY` | Use `--llm-max-tokens 4096` |
| Anthropic | `anthropic` | `claude-haiku-4-5-20251001` | `ANTHROPIC_API_KEY` | Native Messages API |
| OpenAI / compatible | `openai-compatible` | `gpt-4o-mini` | `OPENAI_API_KEY` | Set `--llm-base-url` for custom endpoints |
| Custom HTTP | `http-json` | — | `CIH_LLM_API_KEY` | Requires `--llm-provider-config <path>` |

## API key resolution order

When you do not pass `--llm-api-key-env`, the engine checks these env vars in order:

1. `CIH_LLM_API_KEY`
2. `DEEPSEEK_API_KEY`
3. `GEMINI_API_KEY`
4. `OPENAI_API_KEY`
5. `ANTHROPIC_API_KEY`

Set only the one matching your provider to avoid ambiguity.

## Wiki modes

| `--wiki-mode` | LLM calls | Output |
|---|---|---|
| `graph` | none | Structural pages only (routes, tables, communities) |
| `llm-summary` | summary per community + controller descriptions | Adds Overview / PO text + controller feature grouping |
| `llm-full` | full enrichment per community | Adds Capabilities, Workflows, Open Questions sections |

## Useful flags

| Flag | Purpose |
|---|---|
| `--llm-max-tokens` | Max tokens per response. Use `4096` for Gemini; `2048` for others |
| `--llm-timeout-secs` | Per-call timeout (default `30`) |
| `--llm-retries` | Retry count on transient errors (default `2`) |
| `--llm-concurrency` | Parallel community calls (default `8`; reduce if rate-limited) |
| `--llm-dry-run` | Print prompts without calling the API |
| `--filter-community` | Limit to specific communities for a quick test |
| `--max-communities` | Hard cap on communities processed |

## Notes on specific providers

### DeepSeek
Returns clean JSON without markdown fences. Handles batches of 10 controllers reliably.
Model `deepseek-chat` covers both V2 and V3 depending on the tier.

### Google Gemini
May wrap JSON in ` ```json ``` ` fences — the engine strips them automatically.
Use `gemini-2.5-flash` (not `gemini-2.0-flash`, which is deprecated).
Occasional 503 errors during peak load; the engine retries automatically.

### Anthropic
Uses the native Anthropic Messages API (not OpenAI-compatible).
`claude-haiku-4-5-20251001` is the fastest and cheapest option; upgrade to
`claude-sonnet-4-6` for higher quality at higher cost.

### Self-hosted (Ollama / vLLM / LM Studio)
Use `--llm-provider openai-compatible` with `--llm-base-url http://localhost:<port>/v1`.
HTTP (non-TLS) is only allowed for `localhost`, `127.0.0.1`, and `::1`.
No API key is required; set `CIH_LLM_API_KEY=unused` if the adapter requires a non-empty value.

### Custom HTTP (http-json)
Provide a JSON config file describing the request/response shape of your endpoint:

```bash
cargo run -p cih-engine -- wiki "$REPO" \
  --llm --llm-provider http-json \
  --llm-provider-config /path/to/adapter.json
```
