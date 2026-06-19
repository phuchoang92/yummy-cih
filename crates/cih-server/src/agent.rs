//! Phase 4 — Provider-neutral AI agent loop.
//!
//! `AgentRunner` holds references to the server's search and graph state, runs a
//! multi-turn LLM tool-use loop, and returns a grounded final answer.
//!
//! Tool calls the LLM can make:
//!   - `search_code(query, limit)` — BM25+semantic hybrid search
//!   - `get_context(node_id)` — callers, callees, community membership
//!   - `trace_impact(node_id, direction)` — upstream/downstream BFS impact
//!
//! The agent loop runs up to `MAX_TURNS` rounds. Each round:
//!   1. Send conversation history + tool definitions to LLM.
//!   2. If the response contains tool_calls: execute each, append results, loop.
//!   3. If finish_reason is "stop": return the assistant message as the answer.

use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use cih_graph_store::GraphStore;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::debug;

use cih_graph_store::Direction;

use crate::search::SearchState;

const MAX_TURNS: usize = 8;

#[derive(Clone)]
pub struct AgentRunner {
    search: SearchState,
    store: Arc<dyn GraphStore>,
    llm_base_url: String,
    llm_api_key: String,
    llm_model: String,
}

impl AgentRunner {
    pub fn new(
        search: SearchState,
        store: Arc<dyn GraphStore>,
        llm_base_url: String,
        llm_api_key: String,
        llm_model: String,
    ) -> Self {
        Self { search, store, llm_base_url, llm_api_key, llm_model }
    }

    pub async fn ask(&self, question: &str, codebase_description: &str) -> Result<AgentAnswer> {
        let client = reqwest::Client::new();
        let system = format!(
            "You are a code intelligence assistant. The codebase is: {codebase_description}\n\
             Answer questions by calling the available tools to look up real code facts. \
             Always cite specific node IDs, file names, or route paths from tool results. \
             Do not invent code details not present in tool results."
        );

        let tools = agent_tool_definitions();
        let mut messages: Vec<Value> = vec![json!({"role": "user", "content": question})];
        let mut turns = 0;

        loop {
            turns += 1;
            if turns > MAX_TURNS {
                return Err(anyhow!("agent exceeded {} turns without finishing", MAX_TURNS));
            }

            let mut full_messages: Vec<Value> = vec![json!({"role": "system", "content": &system})];
            full_messages.extend(messages.iter().cloned());
            let body = json!({
                "model": self.llm_model,
                "messages": full_messages,
                "tools": tools,
                "max_tokens": 2048,
            });

            let url = format!("{}/chat/completions", self.llm_base_url.trim_end_matches('/'));
            let resp = client
                .post(&url)
                .bearer_auth(&self.llm_api_key)
                .json(&body)
                .send()
                .await
                .context("agent LLM HTTP call failed")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let text = resp.text().await.unwrap_or_default();
                return Err(anyhow!("LLM returned {status}: {text}"));
            }

            let resp_json: Value = resp.json().await.context("agent LLM response JSON parse")?;
            let choice = &resp_json["choices"][0];
            let finish_reason = choice["finish_reason"].as_str().unwrap_or("stop");
            let assistant_msg = &choice["message"];

            // Append assistant message to history.
            messages.push(assistant_msg.clone());

            if finish_reason == "tool_calls" {
                let tool_calls = assistant_msg["tool_calls"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();

                for tool_call in &tool_calls {
                    let call_id = tool_call["id"].as_str().unwrap_or("").to_string();
                    let fn_name = tool_call["function"]["name"].as_str().unwrap_or("");
                    let args_str = tool_call["function"]["arguments"].as_str().unwrap_or("{}");
                    let args: Value = serde_json::from_str(args_str).unwrap_or(json!({}));

                    debug!(tool = fn_name, call_id = %call_id, "agent tool call");

                    let result = self.execute_tool(fn_name, &args).await;
                    let result_text = match result {
                        Ok(v) => serde_json::to_string_pretty(&v).unwrap_or_default(),
                        Err(err) => format!("{{\"error\": \"{}\"}}", err),
                    };

                    messages.push(json!({
                        "role": "tool",
                        "tool_call_id": call_id,
                        "content": result_text,
                    }));
                }
                // Continue loop to send tool results back to LLM.
                continue;
            }

            // finish_reason == "stop" or similar — extract the answer.
            let answer_text = assistant_msg["content"]
                .as_str()
                .unwrap_or("")
                .to_string();
            let tool_calls_made: Vec<String> = messages
                .iter()
                .filter(|m| m["role"] == "assistant" && !m["tool_calls"].is_null())
                .flat_map(|m| {
                    m["tool_calls"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter_map(|tc| {
                            tc["function"]["name"].as_str().map(str::to_string)
                        })
                })
                .collect();

            return Ok(AgentAnswer {
                answer: answer_text,
                turns,
                tools_called: tool_calls_made,
            });
        }
    }

    async fn execute_tool(&self, name: &str, args: &Value) -> Result<Value> {
        match name {
            "search_code" => {
                let query = args["query"].as_str().unwrap_or("");
                let limit = args["limit"].as_u64().unwrap_or(8) as usize;
                let hits = self.search.query_hits(query, limit).await?;
                Ok(json!(hits
                    .iter()
                    .map(|h| json!({
                        "node_id": h.node_id.as_str(),
                        "kind": h.kind.label(),
                        "name": h.name,
                        "file": h.file,
                        "line": h.range.start_line,
                        "score": h.score,
                    }))
                    .collect::<Vec<_>>()))
            }
            "get_context" => {
                let node_id_str = args["node_id"].as_str().unwrap_or("");
                let node_id = cih_core::NodeId::new(node_id_str);
                let ctx = self.store.context(&node_id).await?;
                Ok(serde_json::to_value(&ctx)?)
            }
            "trace_impact" => {
                let node_id_str = args["node_id"].as_str().unwrap_or("");
                let direction = args["direction"].as_str().unwrap_or("upstream");
                let node_id = cih_core::NodeId::new(node_id_str);
                let dir = match direction {
                    "downstream" => Direction::Downstream,
                    "both" => Direction::Both,
                    _ => Direction::Upstream,
                };
                let impact = self.store.impact(&node_id, dir, 4).await?;
                Ok(serde_json::to_value(&impact)?)
            }
            _ => Err(anyhow!("unknown tool: {name}")),
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct AgentAnswer {
    pub answer: String,
    pub turns: usize,
    pub tools_called: Vec<String>,
}

fn agent_tool_definitions() -> Value {
    json!([
        {
            "type": "function",
            "function": {
                "name": "search_code",
                "description": "Search for code by natural language or keywords. Returns ranked code matches.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": {"type": "string", "description": "Natural language or keyword query"},
                        "limit": {"type": "integer", "description": "Max results (default 8)"}
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_context",
                "description": "Get callers, callees, community, and wiki summary for a specific code node by its ID.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "node_id": {"type": "string", "description": "Stable node ID (e.g. Method:com.acme.OrderService#create/2)"}
                    },
                    "required": ["node_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "trace_impact",
                "description": "Find what code would be affected if a given node changes (upstream callers or downstream callees).",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "node_id": {"type": "string", "description": "Node ID to trace from"},
                        "direction": {"type": "string", "enum": ["upstream", "downstream"], "description": "upstream = callers, downstream = callees"}
                    },
                    "required": ["node_id", "direction"]
                }
            }
        }
    ])
}
