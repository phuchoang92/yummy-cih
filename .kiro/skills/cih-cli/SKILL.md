---
name: cih-cli
description: Run CIH indexing, status, wiki, setup, and maintenance commands safely.
---

# CIH CLI

Use the unified `cih` executable for the portable workflow.

- `cih index [REPO]` runs analyze, discover, wiki, and agent-context generation.
- `cih index [REPO] --no-agent-context` opts out for that run.
- `cih engine status <name>` checks an indexed repository.
- `cih engine config show --repo <path>` explains effective configuration.
- `cih setup --coding-agent <agents>` configures global MCP and skills.
- `cih uninstall --coding-agent <agents>` previews removal; add `--force` to apply it.

Do not put raw access tokens in configuration. Use an environment-variable name with
`--token-env`, and keep remote MCP endpoints on HTTPS.
