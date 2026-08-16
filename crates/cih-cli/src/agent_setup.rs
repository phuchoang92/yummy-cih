use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use clap::ValueEnum;
use jsonc_parser::cst::{CstInputValue, CstRootNode};
use serde::Serialize;
use serde_json::{json, Value};
use toml_edit::{value, DocumentMut, Item, Table};

const DEFAULT_AGENTS: &[CodingAgent] = &[
    CodingAgent::Codex,
    CodingAgent::Claude,
    CodingAgent::OpenCode,
    CodingAgent::Kiro,
];

const STANDARD_SKILLS: &[&str] = &[
    "cih-exploring",
    "cih-impact-analysis",
    "cih-debugging",
    "cih-product-owner",
    "cih-testing",
    "cih-security",
    "cih-documenting",
    "cih-cli",
    "cih-guide",
];

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CodingAgent {
    Codex,
    Claude,
    #[value(name = "opencode")]
    OpenCode,
    Kiro,
}

#[derive(Debug, Serialize)]
struct SetupReport {
    mode: &'static str,
    dry_run: bool,
    agents: Vec<CodingAgent>,
    actions: Vec<String>,
}

pub fn setup(
    agents: Vec<CodingAgent>,
    url: &str,
    token_env: Option<&str>,
    dry_run: bool,
    force: bool,
    json_output: bool,
) -> Result<()> {
    validate_url(url)?;
    if let Some(name) = token_env {
        validate_env_name(name)?;
    }
    let home = home_dir()?;
    let agents = normalized_agents(agents);
    if !dry_run {
        let mut preflight = Vec::new();
        for agent in &agents {
            configure_agent(*agent, &home, url, token_env, force, false, &mut preflight)?;
        }
    }
    let mut actions = Vec::new();
    for agent in &agents {
        configure_agent(*agent, &home, url, token_env, force, !dry_run, &mut actions)?;
    }
    render_report(
        SetupReport {
            mode: "setup",
            dry_run,
            agents,
            actions,
        },
        json_output,
    )
}

pub fn uninstall(agents: Vec<CodingAgent>, force: bool, json_output: bool) -> Result<()> {
    let home = home_dir()?;
    let agents = normalized_agents(agents);
    if force {
        let mut preflight = Vec::new();
        for agent in &agents {
            uninstall_agent(*agent, &home, false, &mut preflight)?;
        }
    }
    let mut actions = Vec::new();
    for agent in &agents {
        uninstall_agent(*agent, &home, force, &mut actions)?;
    }
    render_report(
        SetupReport {
            mode: "uninstall",
            dry_run: !force,
            agents,
            actions,
        },
        json_output,
    )
}

fn normalized_agents(agents: Vec<CodingAgent>) -> Vec<CodingAgent> {
    let mut agents = if agents.is_empty() {
        DEFAULT_AGENTS.to_vec()
    } else {
        agents
    };
    let mut unique = Vec::new();
    agents.retain(|agent| {
        if unique.contains(agent) {
            false
        } else {
            unique.push(*agent);
            true
        }
    });
    agents
}

fn configure_agent(
    agent: CodingAgent,
    home: &Path,
    url: &str,
    token_env: Option<&str>,
    force: bool,
    apply: bool,
    actions: &mut Vec<String>,
) -> Result<()> {
    match agent {
        CodingAgent::Codex => {
            let path = home.join(".codex/config.toml");
            configure_codex(&path, url, token_env, force, apply)?;
            actions.push(format!("upsert Codex MCP entry: {}", path.display()));
            install_skills(&home.join(".agents/skills"), force, apply, actions)?;
        }
        CodingAgent::Claude => {
            let path = home.join(".claude.json");
            configure_jsonc(
                &path,
                &["mcpServers"],
                claude_entry(url, token_env),
                force,
                apply,
            )?;
            actions.push(format!("upsert Claude MCP entry: {}", path.display()));
            install_skills(&home.join(".claude/skills"), force, apply, actions)?;
        }
        CodingAgent::OpenCode => {
            let path = opencode_config_path(home);
            let hierarchy = opencode_hierarchy(&path)?;
            configure_jsonc(
                &path,
                &hierarchy,
                opencode_entry(url, token_env),
                force,
                apply,
            )?;
            actions.push(format!("upsert OpenCode MCP entry: {}", path.display()));
            let compatible = home.join(".agents/skills/cih-guide/SKILL.md").exists()
                || home.join(".claude/skills/cih-guide/SKILL.md").exists();
            if compatible {
                actions.push("reuse OpenCode-compatible ~/.agents or ~/.claude skills".into());
            } else {
                install_skills(&home.join(".config/opencode/skills"), force, apply, actions)?;
            }
        }
        CodingAgent::Kiro => {
            let path = home.join(".kiro/settings/mcp.json");
            configure_jsonc(
                &path,
                &["mcpServers"],
                kiro_entry(url, token_env),
                force,
                apply,
            )?;
            actions.push(format!("upsert Kiro MCP entry: {}", path.display()));
            install_skills(&home.join(".kiro/skills"), force, apply, actions)?;
        }
    }
    Ok(())
}

fn uninstall_agent(
    agent: CodingAgent,
    home: &Path,
    apply: bool,
    actions: &mut Vec<String>,
) -> Result<()> {
    match agent {
        CodingAgent::Codex => {
            let path = home.join(".codex/config.toml");
            remove_codex(&path, apply)?;
            actions.push(format!("remove Codex MCP entry: {}", path.display()));
            remove_skills(&home.join(".agents/skills"), apply, actions)?;
        }
        CodingAgent::Claude => {
            let path = home.join(".claude.json");
            remove_jsonc(&path, &["mcpServers"], apply)?;
            actions.push(format!("remove Claude MCP entry: {}", path.display()));
            remove_skills(&home.join(".claude/skills"), apply, actions)?;
        }
        CodingAgent::OpenCode => {
            let path = opencode_config_path(home);
            let hierarchy = opencode_hierarchy(&path)?;
            remove_jsonc(&path, &hierarchy, apply)?;
            actions.push(format!("remove OpenCode MCP entry: {}", path.display()));
            remove_skills(&home.join(".config/opencode/skills"), apply, actions)?;
        }
        CodingAgent::Kiro => {
            let path = home.join(".kiro/settings/mcp.json");
            remove_jsonc(&path, &["mcpServers"], apply)?;
            actions.push(format!("remove Kiro MCP entry: {}", path.display()));
            remove_skills(&home.join(".kiro/skills"), apply, actions)?;
        }
    }
    Ok(())
}

fn configure_codex(
    path: &Path,
    url: &str,
    token_env: Option<&str>,
    force: bool,
    apply: bool,
) -> Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let mut document = if existing.trim().is_empty() {
        DocumentMut::new()
    } else {
        existing
            .parse::<DocumentMut>()
            .with_context(|| format!("invalid TOML in {}", path.display()))?
    };
    let current_item = document
        .get("mcp_servers")
        .and_then(Item::as_table)
        .and_then(|table| table.get("cih"));
    if let Some(current) = current_item {
        let matches = current.as_table().is_some_and(|table| {
            table.get("url").and_then(Item::as_str) == Some(url)
                && table.get("bearer_token_env_var").and_then(Item::as_str) == token_env
        });
        if !matches && !force {
            bail!(
                "{} contains a different mcp_servers.cih entry; pass --force to replace it",
                path.display()
            );
        }
    }
    let servers = document
        .entry("mcp_servers")
        .or_insert(Item::Table(Table::new()))
        .as_table_mut()
        .context("mcp_servers must be a TOML table")?;
    if !servers.get("cih").is_some_and(Item::is_table) {
        servers.insert("cih", Item::Table(Table::new()));
    }
    let cih = servers["cih"]
        .as_table_mut()
        .context("mcp_servers.cih must be a TOML table")?;
    cih["url"] = value(url);
    if let Some(token_env) = token_env {
        cih["bearer_token_env_var"] = value(token_env);
    } else {
        cih.remove("bearer_token_env_var");
    }
    write_if_changed(path, &existing, &document.to_string(), apply)
}

fn remove_codex(path: &Path, apply: bool) -> Result<()> {
    let Ok(existing) = fs::read_to_string(path) else {
        return Ok(());
    };
    let mut document = existing
        .parse::<DocumentMut>()
        .with_context(|| format!("invalid TOML in {}", path.display()))?;
    if let Some(servers) = document.get_mut("mcp_servers").and_then(Item::as_table_mut) {
        servers.remove("cih");
    }
    write_if_changed(path, &existing, &document.to_string(), apply)
}

fn configure_jsonc(
    path: &Path,
    hierarchy: &[&str],
    expected: Value,
    force: bool,
    apply: bool,
) -> Result<()> {
    let existing = fs::read_to_string(path).unwrap_or_default();
    let root = CstRootNode::parse(&existing, &Default::default())
        .with_context(|| format!("invalid JSON/JSONC in {}", path.display()))?;
    let mut object = root.object_value_or_set();
    for name in hierarchy {
        object = object.object_value_or_set(name);
    }
    if let Some(current) = object.get("cih") {
        let current_value = current
            .value()
            .map(|value| value.to_string())
            .unwrap_or_default();
        let semantic: Value =
            jsonc_parser::parse_to_serde_value(&current_value, &Default::default())
                .with_context(|| format!("invalid existing cih entry in {}", path.display()))?;
        if semantic != expected && !force {
            bail!(
                "{} contains a different cih MCP entry; pass --force to replace it",
                path.display()
            );
        }
        current.set_value(to_cst(&expected));
    } else {
        object.append("cih", to_cst(&expected));
    }
    write_if_changed(path, &existing, &root.to_string(), apply)
}

fn remove_jsonc(path: &Path, hierarchy: &[&str], apply: bool) -> Result<()> {
    let Ok(existing) = fs::read_to_string(path) else {
        return Ok(());
    };
    let root = CstRootNode::parse(&existing, &Default::default())
        .with_context(|| format!("invalid JSON/JSONC in {}", path.display()))?;
    let Some(mut object) = root.object_value() else {
        return Ok(());
    };
    for name in hierarchy {
        let Some(next) = object.object_value(name) else {
            return Ok(());
        };
        object = next;
    }
    if let Some(prop) = object.get("cih") {
        prop.remove();
    }
    write_if_changed(path, &existing, &root.to_string(), apply)
}

fn claude_entry(url: &str, token_env: Option<&str>) -> Value {
    let mut entry = json!({ "type": "http", "url": url });
    if let Some(name) = token_env {
        entry["headers"] = json!({ "Authorization": format!("Bearer ${{{name}}}") });
    }
    entry
}

fn opencode_entry(url: &str, token_env: Option<&str>) -> Value {
    let mut entry = json!({ "type": "remote", "url": url, "enabled": true });
    if let Some(name) = token_env {
        entry["headers"] = json!({ "Authorization": format!("Bearer {{env:{name}}}") });
    }
    entry
}

fn kiro_entry(url: &str, token_env: Option<&str>) -> Value {
    let mut entry = json!({ "url": url });
    if let Some(name) = token_env {
        entry["headers"] = json!({ "Authorization": format!("Bearer ${{{name}}}") });
    }
    entry
}

fn opencode_config_path(home: &Path) -> PathBuf {
    let dir = home.join(".config/opencode");
    for name in ["opencode.jsonc", "opencode.json"] {
        let path = dir.join(name);
        if path.exists() {
            return path;
        }
    }
    dir.join("opencode.jsonc")
}

fn opencode_hierarchy(path: &Path) -> Result<Vec<&'static str>> {
    let Ok(content) = fs::read_to_string(path) else {
        return Ok(vec!["mcp"]);
    };
    let value: Value = jsonc_parser::parse_to_serde_value(&content, &Default::default())
        .with_context(|| format!("invalid JSON/JSONC in {}", path.display()))?;
    if value.pointer("/mcp/servers").is_some_and(Value::is_object) {
        Ok(vec!["mcp", "servers"])
    } else {
        Ok(vec!["mcp"])
    }
}

fn install_skills(root: &Path, _force: bool, apply: bool, actions: &mut Vec<String>) -> Result<()> {
    actions.push(format!("install CIH standard skills: {}", root.display()));
    if apply {
        cih_engine::agent_context::install_standard_skills(root, true)?;
    }
    Ok(())
}

fn remove_skills(root: &Path, apply: bool, actions: &mut Vec<String>) -> Result<()> {
    for name in STANDARD_SKILLS {
        let path = root.join(name);
        if path.exists() {
            actions.push(format!("remove CIH skill: {}", path.display()));
            if apply {
                fs::remove_dir_all(&path)
                    .with_context(|| format!("removing {}", path.display()))?;
            }
        }
    }
    Ok(())
}

fn validate_url(url: &str) -> Result<()> {
    let authority = url
        .split_once("://")
        .map(|(_, rest)| rest.split('/').next().unwrap_or_default())
        .unwrap_or_default();
    if authority.contains('@') {
        bail!("MCP URLs must not contain embedded credentials; use --token-env")
    }
    if url
        .strip_prefix("https://")
        .is_some_and(|rest| !rest.is_empty() && !rest.starts_with('/'))
    {
        return Ok(());
    }
    let loopback = url.strip_prefix("http://").is_some_and(|rest| {
        let authority = rest.split('/').next().unwrap_or_default();
        let host = if authority.starts_with('[') {
            authority
                .split(']')
                .next()
                .unwrap_or_default()
                .trim_start_matches('[')
        } else {
            authority.split(':').next().unwrap_or_default()
        };
        matches!(host, "127.0.0.1" | "localhost" | "::1")
    });
    if loopback {
        Ok(())
    } else {
        bail!("remote CIH MCP URLs must use HTTPS; plain HTTP is allowed only on loopback")
    }
}

fn validate_env_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let valid = chars
        .next()
        .is_some_and(|ch| ch == '_' || ch.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric());
    if valid {
        Ok(())
    } else {
        bail!("--token-env must be an environment variable name, not a token value")
    }
}

fn home_dir() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .context("cannot determine the user home directory")
}

fn to_cst(value: &Value) -> CstInputValue {
    match value {
        Value::Null => CstInputValue::Null,
        Value::Bool(value) => CstInputValue::Bool(*value),
        Value::Number(value) => CstInputValue::Number(value.to_string()),
        Value::String(value) => CstInputValue::String(value.clone()),
        Value::Array(values) => CstInputValue::Array(values.iter().map(to_cst).collect()),
        Value::Object(values) => CstInputValue::Object(
            values
                .iter()
                .map(|(key, value)| (key.clone(), to_cst(value)))
                .collect(),
        ),
    }
}

fn write_if_changed(path: &Path, old: &str, new: &str, apply: bool) -> Result<()> {
    if !apply || old == new {
        return Ok(());
    }
    let parent = path.parent().context("configuration path has no parent")?;
    fs::create_dir_all(parent)?;
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config");
    let tmp = parent.join(format!(".{name}.{}.tmp", std::process::id()));
    fs::write(&tmp, new)?;
    fs::rename(&tmp, path)?;
    Ok(())
}

fn render_report(report: SetupReport, json_output: bool) -> Result<()> {
    if json_output {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        let qualifier = if report.dry_run { " (dry-run)" } else { "" };
        println!("CIH {}{}", report.mode, qualifier);
        for action in report.actions {
            println!("- {action}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn url_policy_allows_loopback_http_and_requires_remote_https() {
        assert!(validate_url("http://127.0.0.1:8080/mcp").is_ok());
        assert!(validate_url("http://localhost:8080/mcp").is_ok());
        assert!(validate_url("http://[::1]:8080/mcp").is_ok());
        assert!(validate_url("https://cih.example.com/mcp").is_ok());
        assert!(validate_url("http://cih.example.com/mcp").is_err());
        assert!(validate_url("https://secret@cih.example.com/mcp").is_err());
    }

    #[test]
    fn jsonc_update_preserves_comments_and_requires_force_for_conflict() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.jsonc");
        fs::write(&path, "{\n  // keep me\n  \"mcpServers\": {}\n}\n").unwrap();
        configure_jsonc(
            &path,
            &["mcpServers"],
            claude_entry("https://cih.example/mcp", Some("CIH_TOKEN")),
            false,
            true,
        )
        .unwrap();
        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("// keep me"));
        assert!(configure_jsonc(
            &path,
            &["mcpServers"],
            claude_entry("https://different.example/mcp", None),
            false,
            true,
        )
        .is_err());
    }

    #[test]
    fn codex_update_preserves_unrelated_toml() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        fs::write(&path, "# keep\nmodel = \"gpt\"\n").unwrap();
        configure_codex(&path, "http://127.0.0.1:8080/mcp", None, false, true).unwrap();
        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("# keep"));
        assert!(content.contains("model = \"gpt\""));
        assert!(content.contains("[mcp_servers.cih]"));
    }

    #[test]
    fn jsonc_uninstall_removes_only_cih_entry() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.jsonc");
        fs::write(
            &path,
            "{\n  // keep\n  \"mcpServers\": {\n    \"other\": { \"url\": \"https://other\" },\n    \"cih\": { \"url\": \"https://cih\" }\n  }\n}\n",
        )
        .unwrap();
        remove_jsonc(&path, &["mcpServers"], true).unwrap();
        let content = fs::read_to_string(path).unwrap();
        assert!(content.contains("// keep"));
        assert!(content.contains("\"other\""));
        assert!(!content.contains("\"cih\""));
    }

    #[test]
    fn opencode_v1_and_v2_hierarchies_are_supported() {
        let dir = tempfile::tempdir().unwrap();
        let v1 = dir.path().join("v1.jsonc");
        fs::write(&v1, "{ \"mcp\": {} }").unwrap();
        let v1_path = opencode_hierarchy(&v1).unwrap();
        assert_eq!(v1_path, vec!["mcp"]);
        configure_jsonc(
            &v1,
            &v1_path,
            opencode_entry("https://cih.example/mcp", None),
            false,
            true,
        )
        .unwrap();
        let parsed: Value = jsonc_parser::parse_to_serde_value(
            &fs::read_to_string(v1).unwrap(),
            &Default::default(),
        )
        .unwrap();
        assert!(parsed.pointer("/mcp/cih").is_some());

        let v2 = dir.path().join("v2.jsonc");
        fs::write(&v2, "{ \"mcp\": { \"servers\": {} } }").unwrap();
        let v2_path = opencode_hierarchy(&v2).unwrap();
        assert_eq!(v2_path, vec!["mcp", "servers"]);
        configure_jsonc(
            &v2,
            &v2_path,
            opencode_entry("https://cih.example/mcp", None),
            false,
            true,
        )
        .unwrap();
        let parsed: Value = jsonc_parser::parse_to_serde_value(
            &fs::read_to_string(v2).unwrap(),
            &Default::default(),
        )
        .unwrap();
        assert!(parsed.pointer("/mcp/servers/cih").is_some());
    }
}
