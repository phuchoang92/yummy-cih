
## Task 3 Completion — Safe .env Create/Update Behavior

### Created: `crates/cih-engine/src/start_env.rs`

**Functions:**
- `render_env()` — Generate complete .env with header + REPO_PATH/REPO_NAME + optional LLM key
- `load_env_file()` — Read existing .env lines, return Vec<String> or Ok(vec![]) if missing
- `merge_env_values()` — Smart merge: replace known keys, preserve comments/blanks/unknown keys
- `write_env_file()` — Write .env with backup + dry-run support

**Tests: 17/17 passing** ✅
- render_env: 3 tests
- load_env_file: 2 tests  
- merge_env_values: 8 tests (includes all 4 LLM key names)
- write_env_file: 4 tests (new file, backup, dry-run modes)

**Integration:**
- Module added to main.rs: `mod start_env;`
- No new dependencies (stdlib + anyhow only)
- cargo check -p cih-engine: PASS
- All functions marked `#[allow(dead_code)]` — will be used by Task 4

**Key Design:**
- Preservation strategy: Unknown keys/comments survive merge (user customization safe)
- Dry-run capability: Prints to stdout without filesystem modification
- Backup timestamping: Uses `SystemTime::now().duration_since(UNIX_EPOCH).as_secs()`
- LLM key detection: Matches DEEPSEEK_API_KEY, GEMINI_API_KEY, ANTHROPIC_API_KEY, OPENAI_API_KEY
